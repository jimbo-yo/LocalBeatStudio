//! Library file names: `Artist/Album/NN. Artist - Title.flac`.
//!
//! Names use the same characters on every OS: characters Windows (and the
//! phone's storage) forbid are swapped for full-width lookalikes, the way
//! cyanrip turns `?` into `？`. So a library copied between computers keeps
//! its names.

/// Makes a folder name safe.
pub fn safe_component(name: &str) -> String {
    clean(name, true)
}

/// `trim_dots`: drop trailing dots/spaces (Windows strips them from the end of
/// a full name). Not needed for a file stem, which gets an extension.
fn clean(name: &str, trim_dots: bool) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '<' => '＜',
            '>' => '＞',
            ':' => '：',
            '"' => '＂',
            '/' => '／',
            '\\' => '＼',
            '|' => '｜',
            '?' => '？',
            '*' => '＊',
            c => c,
        })
        .collect();

    // Windows drops trailing dots and spaces; leading spaces are just confusing.
    let trimmed = out.trim_start();
    let trimmed = if trim_dots { trimmed.trim_end_matches(['.', ' ']) } else { trimmed.trim_end() };
    out = shorten(trimmed);

    if out.is_empty() {
        return "_".to_string();
    }
    if is_reserved(&out) {
        out.push('_');
    }
    out
}

/// Longest name kept whole, in characters and in UTF-8 bytes. Every file
/// system involved allows 255 per name (NTFS counts UTF-16 units, ext4 and
/// the phone's storage bytes); the margin leaves room for an extension, and
/// keeps whole paths short. The longest name in the library on 2026-11-18
/// was 86 characters, so no existing album changes.
const MAX_CHARS: usize = 120;
const MAX_BYTES: usize = 200;

/// Cuts a name that's too long (Fiona Apple's "When the Pawn…" is about 440
/// characters) at a word boundary and ends it with "…". Tags keep the full
/// title; only the file or folder name is shortened. Same input, same name.
fn shorten(name: &str) -> String {
    if name.chars().count() <= MAX_CHARS && name.len() <= MAX_BYTES {
        return name.to_string();
    }
    const ELLIPSIS: char = '…';
    let (mut cut, mut chars) = (0, 0);
    for (i, c) in name.char_indices() {
        if chars + 1 >= MAX_CHARS || i + c.len_utf8() + ELLIPSIS.len_utf8() > MAX_BYTES {
            break;
        }
        cut = i + c.len_utf8();
        chars += 1;
    }
    let head = &name[..cut];
    // Back up to the last space when it's near the end, so a word isn't cut.
    let head = match head.rfind(' ') {
        Some(sp) if head[sp..].chars().count() <= 25 && name[cut..].chars().next() != Some(' ') => &head[..sp],
        _ => head,
    };
    let head = head.trim_end_matches(|c: char| c == ' ' || c == '.' || c == ',' || c == ';' || c == '-' || c == '：');
    format!("{head}{ELLIPSIS}")
}

/// Device names Windows won't use as file names, with or without extension.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// File name (no folder, no extension) for a track: `07. Artist - Title`,
/// or `2.07. Artist - Title` for disc 2 of a multi-disc album (all discs share
/// the album folder, as rip-cd.sh does).
pub fn track_stem(disc: Option<usize>, number: usize, artist: &str, title: &str) -> String {
    let prefix = disc.map(|d| format!("{d}.")).unwrap_or_default();
    clean(&format!("{prefix}{number:02}. {artist} - {title}"), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_forbidden_characters() {
        assert_eq!(safe_component("Who's Next?"), "Who's Next？");
        assert_eq!(safe_component("Live: 1975/76"), "Live： 1975／76");
        assert_eq!(safe_component(r#"a<b>c"d\e|f*g"#), "a＜b＞c＂d＼e｜f＊g");
    }

    #[test]
    fn keeps_unicode_and_quotes() {
        assert_eq!(safe_component("Sittin’ on Top of the World"), "Sittin’ on Top of the World");
        assert_eq!(safe_component("Björk"), "Björk");
    }

    #[test]
    fn trims_and_handles_empty() {
        assert_eq!(safe_component("  Album... "), "Album");
        assert_eq!(safe_component("..."), "_");
        assert_eq!(safe_component("tab\there"), "tabhere");
    }

    #[test]
    fn reserved_names() {
        assert_eq!(safe_component("CON"), "CON_");
        assert_eq!(safe_component("nul.txt"), "nul.txt_");
        assert_eq!(safe_component("COM1"), "COM1_");
        assert_eq!(safe_component("COM0"), "COM0");
        assert_eq!(safe_component("Console"), "Console");
    }

    #[test]
    fn track_names() {
        assert_eq!(track_stem(None, 7, "The Kinks", "Days"), "07. The Kinks - Days");
        assert_eq!(track_stem(None, 12, "AC/DC", "T.N.T."), "12. AC／DC - T.N.T.");
        assert_eq!(track_stem(Some(2), 3, "The Kinks", "Lola"), "2.03. The Kinks - Lola");
    }

    const PAWN: &str = "When the Pawn Hits the Conflicts He Thinks Like a King What He Knows Throws the Blows When He Goes to the Fight and He’ll Win the Whole Thing ’fore He Enters the Ring There’s No Body to Batter When Your Mind Is Your Might So When You Go Solo, You Hold Your Own Hand and Remember That Depth Is the Greatest of Heights and If You Know Where You Stand, Then You Know Where to Land and If You Fall It Won’t Matter, Cuz You’ll Know That You’re Right";

    #[test]
    fn shortens_very_long_names() {
        let name = safe_component(PAWN);
        assert!(name.chars().count() <= MAX_CHARS && name.len() <= MAX_BYTES, "{name}");
        assert!(name.starts_with("When the Pawn Hits the Conflicts"));
        assert!(name.ends_with('…') && !name.ends_with(" …"), "{name}");
        // Cut between words, not inside one.
        let kept = name.trim_end_matches('…');
        assert!(PAWN.starts_with(kept) && PAWN[kept.len()..].starts_with(' '), "{name}");
        assert_eq!(safe_component(PAWN), name, "same input, same name");

        let stem = track_stem(None, 3, "Fiona Apple", PAWN);
        assert!(stem.starts_with("03. Fiona Apple - When the Pawn") && stem.chars().count() <= MAX_CHARS);
    }

    #[test]
    fn shortens_by_bytes_too() {
        // 3-byte characters: 120 of them would be 360 bytes.
        let name = safe_component(&"日本語".repeat(60));
        assert!(name.len() <= MAX_BYTES && name.ends_with('…'), "{} bytes", name.len());
    }

    #[test]
    fn leaves_normal_names_alone() {
        let long = "6.05. Pyotr Tchaikovsky - The Sleeping Beauty Ballet Suite： Pas d’action ‐ Adagio";
        assert_eq!(track_stem(Some(6), 5, "Pyotr Tchaikovsky", "The Sleeping Beauty Ballet Suite: Pas d’action ‐ Adagio"), long);
        let exact = "a".repeat(MAX_CHARS);
        assert_eq!(safe_component(&exact), exact);
    }
}
