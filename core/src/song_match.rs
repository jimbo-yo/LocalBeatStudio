//! Matching a list of songs from elsewhere ("Artist - Title" lines: a web
//! page, an AI's suggestions) against the library on the server.
//! Design: LocalBeat docs/control.md, "Matching".
//!
//! Never guesses silently: each line comes back Found (same title and
//! artist, ignoring "(Remastered 2011)", "- Live", "feat. …", case, accents
//! and punctuation), Close (suggestions: a near spelling) or NotFound.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

/// A library song, from the server's song object.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub artist: String,
    pub album_artist: String,
    pub title: String,
    pub album: String,
    pub year: u32,
    pub seconds: u64,
    pub suffix: String,
}

impl Candidate {
    pub fn from_json(v: &Value) -> Option<Candidate> {
        let st = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let id = st("id");
        (!id.is_empty()).then(|| Candidate {
            id,
            artist: st("artist"),
            album_artist: v["displayAlbumArtist"].as_str().or(v["albumArtist"].as_str()).unwrap_or("").to_string(),
            title: st("title"),
            album: st("album"),
            year: v["year"].as_u64().unwrap_or(0) as u32,
            seconds: v["duration"].as_u64().unwrap_or(0),
            suffix: st("suffix").to_ascii_lowercase(),
        })
    }
}

/// One line of the list, read.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Wanted {
    /// Empty when the line gave only a title.
    pub artist: String,
    pub title: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The same song (several versions: the best first).
    Found,
    /// Not the same, but near: suggestions to check.
    Close,
    NotFound,
}

#[derive(Debug, Clone, Serialize)]
pub struct Match {
    pub line: String,
    pub wanted: Wanted,
    pub status: Status,
    /// Best first (at most 5).
    pub songs: Vec<Candidate>,
    /// Why, in a few words ("3 versions", "title spelt differently"…).
    pub note: String,
}

impl Match {
    /// The song to use: the best found one.
    pub fn best(&self) -> Option<&Candidate> {
        (self.status == Status::Found).then(|| self.songs.first()).flatten()
    }
}

/// Reads "Artist - Title", "Artist – Title", "\"Title\" by Artist",
/// "Title by Artist", "Artist: Title", with list numbering ("12.", "#3",
/// "4)") and bullets dropped. A line with none of these is a title alone.
/// Empty lines and `#` comments give None.
pub fn parse_line(line: &str) -> Option<Wanted> {
    let mut s = line.trim();
    if s.is_empty() || s.starts_with("# ") || s == "#" {
        return None;
    }
    // Bullets, then numbering: "12.", "12)", "#12", "12 -", "No. 12".
    s = s.trim_start_matches(['-', '*', '•', '·', '–']).trim_start();
    let hash = s.starts_with('#');
    let t = s.strip_prefix('#').unwrap_or(s);
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 {
        let rest = t[digits..].trim_start();
        // "#3 Artist": the # says it's a number; "99 Luftballons" stays.
        let mut cut = hash.then_some(rest);
        for sep in [".", ")", ":", "-", "–", "—"] {
            if let Some(r) = rest.strip_prefix(sep) {
                cut = Some(r.trim_start());
                break;
            }
        }
        if let Some(r) = cut {
            s = r;
        }
    }
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let unquote = |x: &str| x.trim().trim_matches(['"', '“', '”', '\'', '‘', '’']).trim().to_string();
    // "Title" by Artist (quoted only: "Stand by Me" is a title).
    if s.starts_with(['"', '“'])
        && let Some(i) = s.rfind(" by ")
        && s[..i].trim_end().ends_with(['"', '”'])
    {
        return Some(Wanted { artist: unquote(&s[i + 4..]), title: unquote(&s[..i]) });
    }
    for sep in [" - ", " – ", " — ", "\t", " -- ", ": "] {
        if let Some(i) = s.find(sep) {
            return Some(Wanted { artist: unquote(&s[..i]), title: unquote(&s[i + sep.len()..]) });
        }
    }
    Some(Wanted { artist: String::new(), title: unquote(s) })
}

/// Folded for comparing: lower case, accents and curly quotes plain, "&"
/// as "and", punctuation gone, a leading "the" dropped.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        match c {
            '\'' | '’' | '‘' | '`' | '´' | '.' => {}
            '&' | '+' => out.push_str(" and "),
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => out.push('a'),
            'ç' | 'č' | 'ć' => out.push('c'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ě' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' => out.push('i'),
            'ñ' | 'ń' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => out.push('o'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'š' | 'ś' => out.push('s'),
            'ž' | 'ź' | 'ż' => out.push('z'),
            'ł' => out.push('l'),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ß' => out.push_str("ss"),
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    let words: Vec<&str> = out.split_whitespace().collect();
    let words = if words.len() > 1 && words[0] == "the" { &words[1..] } else { &words[..] };
    words.join(" ")
}

/// Words that mark a version, not a different song.
const VERSION_WORDS: &[&str] = &[
    "remaster", "remastered", "live", "version", "edit", "mono", "stereo", "feat", "ft", "featuring", "with",
    "bonus", "demo", "mix", "remix", "single", "album", "explicit", "deluxe", "acoustic", "instrumental", "radio",
    "extended", "take", "anniversary", "edition", "recorded", "session", "outtake", "alternate",
];

fn is_version_note(s: &str) -> bool {
    let n = norm(s);
    n.split_whitespace().any(|w| VERSION_WORDS.contains(&w) || (w.len() == 4 && w.chars().all(|c| c.is_ascii_digit())))
}

/// The title without version notes: "(Remastered 2011)", "[Live]",
/// "- 2014 Remaster", "feat. X". Returns the bare title, folded.
pub fn title_key(title: &str) -> String {
    let mut t = title.trim().to_string();
    loop {
        let before = t.clone();
        // A trailing (…) or […] that's a note.
        if let Some(close) = t.chars().last().filter(|c| *c == ')' || *c == ']') {
            let open = if close == ')' { '(' } else { '[' };
            if let Some(i) = t.rfind(open)
                && i > 0
                && is_version_note(&t[i..])
            {
                t = t[..i].trim_end().to_string();
            }
        }
        // " - Live at …", " - 2011 Remaster".
        for sep in [" - ", " – ", " — "] {
            if let Some(i) = t.rfind(sep)
                && i > 0
                && is_version_note(&t[i + sep.len()..])
            {
                t = t[..i].trim_end().to_string();
            }
        }
        // " feat. X" without brackets.
        let lower = t.to_lowercase();
        for f in [" feat. ", " feat ", " ft. ", " featuring "] {
            if let Some(i) = lower.find(f)
                && i > 0
            {
                t = t[..i].trim_end().to_string();
                break;
            }
        }
        if t == before {
            break;
        }
    }
    let k = norm(&t);
    if k.is_empty() { norm(title) } else { k }
}

/// The artist without "feat. X".
fn artist_key(artist: &str) -> String {
    let lower = artist.to_lowercase();
    let cut = [" feat. ", " feat ", " ft. ", " featuring "].iter().filter_map(|f| lower.find(f)).min().unwrap_or(artist.len());
    norm(&artist[..cut])
}

/// Same artist: equal, or one names the other ("Bruce Springsteen" in
/// "Bruce Springsteen & The E Street Band", "Björk" in "Björk & Mark Bell").
fn same_artist(wanted: &str, c: &Candidate) -> bool {
    if wanted.is_empty() {
        return true;
    }
    [&c.artist, &c.album_artist].iter().filter(|a| !a.is_empty()).any(|a| {
        let a = artist_key(a);
        a == wanted || contains_words(&a, wanted) || contains_words(wanted, &a)
    })
}

fn contains_words(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && format!(" {hay} ").contains(&format!(" {needle} "))
}

/// 1 = the same, 0 = nothing alike (edit distance over the longer length).
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f64 / longest as f64
}

/// Albums that are probably not the original (preferred less).
fn compilation_like(album: &str) -> bool {
    let n = norm(album);
    ["live", "greatest", "hits", "best of", "collection", "anthology", "essential", "unplugged", "concert"]
        .iter()
        .any(|w| contains_words(&n, w))
}

pub struct Matcher<'a> {
    songs: &'a [Candidate],
    by_title: HashMap<String, Vec<usize>>,
    keys: Vec<String>,
}

impl<'a> Matcher<'a> {
    pub fn new(songs: &'a [Candidate]) -> Matcher<'a> {
        let keys: Vec<String> = songs.iter().map(|s| title_key(&s.title)).collect();
        let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, k) in keys.iter().enumerate() {
            by_title.entry(k.clone()).or_default().push(i);
        }
        Matcher { songs, by_title, keys }
    }

    /// Ranks versions of one song: the FLAC, the same full title (a "Live"
    /// asked for gets the live one), not a live album or compilation, the
    /// earliest year.
    fn rank(&self, w: &Wanted, mut found: Vec<&'a Candidate>) -> Vec<&'a Candidate> {
        let full = norm(&w.title);
        found.sort_by_key(|c| {
            (norm(&c.title) != full, c.suffix != "flac", compilation_like(&c.album), if c.year == 0 { u32::MAX } else { c.year }, c.album.clone(), c.id.clone())
        });
        found.dedup_by(|a, b| a.id == b.id);
        found
    }

    pub fn find(&self, line: &str, w: Wanted) -> Match {
        let key = title_key(&w.title);
        let artist = artist_key(&w.artist);
        let done = |status, songs: Vec<&Candidate>, note: String| Match {
            line: line.to_string(),
            wanted: w.clone(),
            status,
            songs: songs.into_iter().take(5).cloned().collect(),
            note,
        };
        let same_title: Vec<&Candidate> = self.by_title.get(&key).into_iter().flatten().map(|&i| &self.songs[i]).collect();
        let exact: Vec<&Candidate> = same_title.iter().copied().filter(|c| same_artist(&artist, c)).collect();
        if !exact.is_empty() {
            let artists: std::collections::HashSet<String> = exact.iter().map(|c| artist_key(&c.artist)).collect();
            let ranked = self.rank(&w, exact);
            // A title alone that several artists recorded: ask.
            if artist.is_empty() && artists.len() > 1 {
                return done(Status::Close, ranked, format!("no artist given; {} artists have this title", artists.len()));
            }
            let note = match ranked.len() {
                1 => String::new(),
                n => format!("{n} versions"),
            };
            return done(Status::Found, ranked, note);
        }
        // Near: the title spelt a little differently by the same artist, or
        // the same title by an artist spelt a little differently.
        let mut near: Vec<(f64, &Candidate)> = Vec::new();
        for (i, c) in self.songs.iter().enumerate() {
            let t = similarity(&self.keys[i], &key);
            let a = if artist.is_empty() {
                1.0
            } else {
                [&c.artist, &c.album_artist].iter().map(|x| similarity(&artist_key(x), &artist)).fold(0.0, f64::max).max(if same_artist(&artist, c) { 1.0 } else { 0.0 })
            };
            let contains = key.len() >= 4 && (contains_words(&self.keys[i], &key) || contains_words(&key, &self.keys[i]));
            let score = if a >= 0.999 && (t >= 0.8 || contains) {
                t.max(0.8)
            } else if t >= 0.999 && a >= 0.75 {
                a
            } else {
                continue;
            };
            near.push((score, c));
        }
        if near.is_empty() {
            let note = if same_title.is_empty() { "" } else { "this title, by other artists" };
            return done(Status::NotFound, Vec::new(), note.to_string());
        }
        near.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| (x.1.suffix != "flac").cmp(&(y.1.suffix != "flac"))));
        let note = format!("not exact (best {:.0}% alike)", near[0].0 * 100.0);
        done(Status::Close, near.into_iter().map(|(_, c)| c).collect(), note)
    }

    /// Every line of `text` that names a song.
    pub fn match_text(&self, text: &str) -> Vec<Match> {
        text.lines().filter_map(|l| parse_line(l).map(|w| self.find(l.trim(), w))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, artist: &str, title: &str, album: &str, year: u32, suffix: &str) -> Candidate {
        Candidate {
            id: id.into(),
            artist: artist.into(),
            album_artist: artist.into(),
            title: title.into(),
            album: album.into(),
            year,
            seconds: 200,
            suffix: suffix.into(),
        }
    }

    fn library() -> Vec<Candidate> {
        vec![
            song("s1", "Bruce Springsteen", "Thunder Road", "Born to Run", 1975, "flac"),
            song("s2", "Bruce Springsteen", "Thunder Road", "Greatest Hits", 1995, "flac"),
            song("s3", "Bruce Springsteen", "Thunder Road", "Born to Run", 1975, "m4a"),
            song("s4", "Bruce Springsteen & The E Street Band", "Thunder Road (Live)", "Live 1975–85", 1986, "flac"),
            song("s5", "Tom Petty and the Heartbreakers", "Runnin’ Down a Dream", "Full Moon Fever", 1989, "flac"),
            song("s6", "Björk", "Jóga", "Homogenic", 1997, "flac"),
            song("s7", "The Beatles", "Here Comes the Sun", "Abbey Road", 1969, "flac"),
            song("s8", "Richie Havens", "Here Comes the Sun", "Alarm Clock", 1971, "mp3"),
            song("s9", "Steppenwolf", "Born to Be Wild", "Steppenwolf", 1968, "flac"),
            song("s10", "Fleetwood Mac", "Go Your Own Way (2004 Remaster)", "Rumours", 1977, "flac"),
        ]
    }

    #[test]
    fn reading_lines() {
        let w = |a: &str, t: &str| Some(Wanted { artist: a.into(), title: t.into() });
        assert_eq!(parse_line("Bruce Springsteen - Thunder Road"), w("Bruce Springsteen", "Thunder Road"));
        assert_eq!(parse_line("12. Steppenwolf – Born to Be Wild"), w("Steppenwolf", "Born to Be Wild"));
        assert_eq!(parse_line("#3 Tom Petty — Runnin' Down a Dream"), w("Tom Petty", "Runnin' Down a Dream"));
        assert_eq!(parse_line("4) \"Go Your Own Way\" by Fleetwood Mac"), w("Fleetwood Mac", "Go Your Own Way"));
        assert_eq!(parse_line("“Here Comes the Sun” by The Beatles"), w("The Beatles", "Here Comes the Sun"));
        assert_eq!(parse_line("- Björk: Jóga"), w("Björk", "Jóga"));
        assert_eq!(parse_line("Thunder Road"), w("", "Thunder Road"));
        assert_eq!(parse_line("AC/DC - Highway to Hell"), w("AC/DC", "Highway to Hell"));
        assert_eq!(parse_line("   "), None);
        assert_eq!(parse_line("# road trip list"), None);
    }

    #[test]
    fn folding() {
        assert_eq!(norm("Runnin’ Down a Dream"), "runnin down a dream");
        assert_eq!(norm("Björk"), "bjork");
        assert_eq!(norm("The Beatles"), "beatles");
        assert_eq!(norm("Simon & Garfunkel"), "simon and garfunkel");
        assert_eq!(title_key("Go Your Own Way (2004 Remaster)"), "go your own way");
        assert_eq!(title_key("Thunder Road - Live at the Hammersmith Odeon"), "thunder road");
        assert_eq!(title_key("Layla - 2011 Remastered"), "layla");
        assert_eq!(title_key("Empire State of Mind (feat. Alicia Keys)"), "empire state of mind");
        assert_eq!(title_key("(What’s the Story) Morning Glory?"), "whats the story morning glory");
        assert_eq!(parse_line("Stand by Me"), Some(Wanted { artist: String::new(), title: "Stand by Me".into() }));
        assert_eq!(parse_line("99 Luftballons"), Some(Wanted { artist: String::new(), title: "99 Luftballons".into() }));
        assert_eq!(title_key("(Live)"), "live", "never empty");
        assert!(similarity("thunder road", "thunder raod") > 0.8);
    }

    #[test]
    fn finding() {
        let lib = library();
        let m = Matcher::new(&lib);
        let one = |line: &str| m.find(line, parse_line(line).unwrap());

        // Several versions: the album's FLAC first, then the compilation, the AAC, the live one.
        let r = one("Bruce Springsteen - Thunder Road");
        assert_eq!(r.status, Status::Found);
        let ids: Vec<&str> = r.songs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2", "s3", "s4"]);
        assert_eq!(r.best().unwrap().id, "s1");
        assert_eq!(r.note, "4 versions");
        // Asking for the live one gets it first.
        assert_eq!(one("Bruce Springsteen - Thunder Road (Live)").best().unwrap().id, "s4");

        // Curly vs straight, "Tom Petty" inside "Tom Petty and the Heartbreakers", no accents.
        assert_eq!(one("Tom Petty - Runnin' Down a Dream").best().unwrap().id, "s5");
        assert_eq!(one("Bjork - Joga").best().unwrap().id, "s6");
        assert_eq!(one("Fleetwood Mac - Go Your Own Way").best().unwrap().id, "s10");
        assert_eq!(one("Beatles - Here Comes The Sun").best().unwrap().id, "s7");

        // A title alone: one artist has it = found; two = ask.
        assert_eq!(one("Born to Be Wild").status, Status::Found);
        let r = one("Here Comes the Sun");
        assert_eq!(r.status, Status::Close);
        assert_eq!(r.songs.len(), 2);
        assert!(r.best().is_none(), "close is never used without asking");

        // Misspelt: close, with the suggestion.
        let r = one("Bruce Springstein - Thunder Road");
        assert_eq!(r.status, Status::Close);
        assert_eq!(r.songs[0].title, "Thunder Road");
        let r = one("Steppenwolf - Born To Be Wilde");
        assert_eq!(r.status, Status::Close);
        assert_eq!(r.songs[0].id, "s9");

        // Not here.
        let r = one("Journey - Don't Stop Believin'");
        assert_eq!(r.status, Status::NotFound);
        assert!(r.songs.is_empty());
        let r = one("Nina Simone - Here Comes the Sun");
        assert_eq!(r.status, Status::NotFound);
        assert_eq!(r.note, "this title, by other artists");
    }

    #[test]
    fn whole_lists() {
        let lib = library();
        let m = Matcher::new(&lib);
        let text = "# Road trip\n\n1. Steppenwolf - Born to Be Wild\n2. Journey - Don't Stop Believin'\n3. Tom Petty - Runnin' Down a Dream\n";
        let r = m.match_text(text);
        assert_eq!(r.len(), 3);
        assert_eq!(r.iter().map(|x| x.status).collect::<Vec<_>>(), [Status::Found, Status::NotFound, Status::Found]);
        assert_eq!(r[1].line, "2. Journey - Don't Stop Believin'");
    }
}
