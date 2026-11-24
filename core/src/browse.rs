//! Ways into the library besides albums and artists: genres (broad genre →
//! subgenres), moods and decades. The same rules as the phone app
//! (LocalBeat `library/Library.kt`), so both show the same groups; terms in
//! LocalBeat `GLOSSARY.md`.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::library::{Library, norm, sort_name};

/// A named set of albums (indices into `Library::albums`).
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// Stable identity: the folded name ("rock"), or "1980" for a decade.
    pub key: String,
    pub name: String,
    pub albums: Vec<usize>,
}

/// A genre with its subgenres. Its albums include every subgenre's.
#[derive(Debug, Clone, PartialEq)]
pub struct Genre {
    pub group: Group,
    /// Largest first, then A–Z.
    pub subgenres: Vec<Group>,
}

#[derive(Debug, Default)]
pub struct Browse {
    /// A–Z.
    pub genres: Vec<Genre>,
    /// A–Z.
    pub moods: Vec<Group>,
    /// Oldest first.
    pub decades: Vec<Group>,
    /// Each album's genre key (index = album), if it has one.
    pub genre_of: Vec<Option<String>>,
}

impl Browse {
    /// Genres: an AI-enriched album's GENRE is [genre, subgenre, …]. An older
    /// album with a single GENRE that the library knows as the subgenre of
    /// exactly one genre goes under that genre. Moods: every MOOD value.
    /// Decades: by the album's (original release) year.
    pub fn new(lib: &Library) -> Browse {
        let albums = &lib.albums;
        let by_artist_then_year = |v: &mut Vec<usize>| {
            v.sort_by_key(|&i| (sort_name(&albums[i].artist), albums[i].year, sort_name(&albums[i].title)));
            v.dedup();
        };

        // Display names: the most common spelling of each folded name.
        let mut spellings: HashMap<String, Vec<&str>> = HashMap::new();
        let mut primaries = HashSet::new();
        let mut parents: HashMap<String, HashSet<String>> = HashMap::new();
        for a in albums {
            for g in &a.genres {
                spellings.entry(norm(g)).or_default().push(g);
            }
            if a.genres.len() >= 2 {
                let p = norm(&a.genres[0]);
                for sub in &a.genres[1..] {
                    let s = norm(sub);
                    if s != p {
                        parents.entry(s).or_default().insert(p.clone());
                    }
                }
                primaries.insert(p);
            }
        }
        let mut genre_albums: HashMap<String, Vec<usize>> = HashMap::new();
        let mut sub_albums: HashMap<String, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut genre_of = vec![None; albums.len()];
        for (i, a) in albums.iter().enumerate() {
            let Some(first) = a.genres.first() else { continue };
            let first = norm(first);
            let folded_parent = (a.genres.len() == 1 && !primaries.contains(&first))
                .then(|| parents.get(&first).filter(|p| p.len() == 1).and_then(|p| p.iter().next().cloned()))
                .flatten();
            let parent = folded_parent.clone().unwrap_or_else(|| first.clone());
            genre_albums.entry(parent.clone()).or_default().push(i);
            let subs: Vec<String> = if folded_parent.is_some() {
                vec![first]
            } else {
                a.genres[1..].iter().map(|s| norm(s)).filter(|s| *s != parent).collect()
            };
            let map = sub_albums.entry(parent.clone()).or_default();
            for s in subs {
                map.entry(s).or_default().push(i);
            }
            genre_of[i] = Some(parent);
        }
        let name = |key: &str| most_common(spellings.get(key).map(Vec::as_slice).unwrap_or_default()).unwrap_or(key).to_string();
        let mut genres: Vec<Genre> = genre_albums
            .into_iter()
            .map(|(key, mut list)| {
                by_artist_then_year(&mut list);
                let mut subgenres: Vec<Group> = sub_albums
                    .remove(&key)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(sk, mut sl)| {
                        by_artist_then_year(&mut sl);
                        Group { name: name(&sk), key: sk, albums: sl }
                    })
                    .collect();
                subgenres.sort_by(|a, b| b.albums.len().cmp(&a.albums.len()).then_with(|| sort_name(&a.name).cmp(&sort_name(&b.name))));
                Genre { group: Group { name: name(&key), key, albums: list }, subgenres }
            })
            .collect();
        genres.sort_by_key(|g| sort_name(&g.group.name));

        let mut mood_albums: HashMap<String, (Vec<&str>, Vec<usize>)> = HashMap::new();
        for (i, a) in albums.iter().enumerate() {
            for m in &a.moods {
                let e = mood_albums.entry(norm(m)).or_default();
                e.0.push(m);
                e.1.push(i);
            }
        }
        let mut moods: Vec<Group> = mood_albums
            .into_iter()
            .map(|(key, (names, mut list))| {
                by_artist_then_year(&mut list);
                Group { name: most_common(&names).unwrap_or(&key).to_string(), key, albums: list }
            })
            .collect();
        moods.sort_by_key(|g| sort_name(&g.name));

        let mut by_decade: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
        for (i, a) in albums.iter().enumerate() {
            if let Some(y) = a.year.filter(|&y| y > 0) {
                by_decade.entry(y / 10 * 10).or_default().push(i);
            }
        }
        let decades = by_decade
            .into_iter()
            .map(|(d, mut list)| {
                list.sort_by_key(|&i| (albums[i].year, sort_name(&albums[i].artist)));
                Group { key: d.to_string(), name: format!("{d}s"), albums: list }
            })
            .collect();

        Browse { genres, moods, decades, genre_of }
    }

    pub fn genre(&self, key: &str) -> Option<&Genre> {
        self.genres.iter().find(|g| g.group.key == key)
    }

    pub fn mood(&self, key: &str) -> Option<&Group> {
        self.moods.iter().find(|g| g.key == key)
    }

    pub fn decade(&self, key: &str) -> Option<&Group> {
        self.decades.iter().find(|g| g.key == key)
    }

    /// The genre (and subgenre, if it is one) a genre name stands for:
    /// "Jangle Pop" -> ("alternative & indie", Some("jangle pop")).
    pub fn find_genre(&self, name: &str) -> Option<(String, Option<String>)> {
        let k = norm(name);
        if self.genre(&k).is_some() {
            return Some((k, None));
        }
        self.genres.iter().find(|g| g.subgenres.iter().any(|s| s.key == k)).map(|g| (g.group.key.clone(), Some(k)))
    }
}

/// An album's decade key ("1980"), if it has a year.
pub fn decade_key(year: Option<u16>) -> Option<String> {
    year.filter(|&y| y > 0).map(|y| (y / 10 * 10).to_string())
}

fn most_common<'a>(names: &[&'a str]) -> Option<&'a str> {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for n in names {
        match counts.iter_mut().find(|(x, _)| x == n) {
            Some(c) => c.1 += 1,
            None => counts.push((n, 1)),
        }
    }
    // The first spelling seen wins a tie.
    counts.iter().rev().max_by_key(|(_, c)| *c).map(|(n, _)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Album, Origin};

    fn album(title: &str, artist: &str, year: u16, genres: &str, moods: &str) -> Album {
        let split = |s: &str| s.split(';').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>();
        Album {
            key: title.into(),
            title: title.into(),
            artist: artist.into(),
            year: Some(year),
            genre: split(genres).first().cloned(),
            genres: split(genres),
            moods: split(moods),
            ai_enriched: None,
            songs: Vec::new(),
            origin: Origin::Flac,
            dirs: Vec::new(),
            seconds: 0.0,
            added: 0,
            discs: 1,
        }
    }

    /// The phone's test library (LocalBeat `LibraryTest.broadGenresHierarchyAndSearchAndVoice`).
    fn lib() -> Library {
        Library {
            albums: vec![
                album("In My Tribe", "10,000 Maniacs", 1987, "Alternative & Indie; Jangle Pop; Folk Rock", "Thoughtful; Warm; Mellow"),
                album("Appetite for Destruction", "Guns N' Roses", 1987, "Rock; Hard Rock", "Energetic; Driving; Gritty"),
                album("Back in Black", "AC/DC", 1980, "Hard Rock", "Driving; Anthemic"),
                album("Led Zeppelin IV", "Led Zeppelin", 1971, "Rock; Classic Rock", "Driving; Gritty"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn genres_fold_older_single_genre_albums_under_their_genre() {
        let b = Browse::new(&lib());
        assert_eq!(b.genres.iter().map(|g| g.group.name.as_str()).collect::<Vec<_>>(), ["Alternative & Indie", "Rock"]);
        let rock = b.genre("rock").unwrap();
        assert_eq!(rock.group.albums.len(), 3, "Back in Black's lone Hard Rock folds under Rock");
        assert_eq!(rock.subgenres.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Hard Rock", "Classic Rock"]);
        assert_eq!(b.genre_of[2].as_deref(), Some("rock"));
        assert_eq!(b.find_genre("jangle pop"), Some(("alternative & indie".into(), Some("jangle pop".into()))));
    }

    #[test]
    fn moods_and_decades() {
        let b = Browse::new(&lib());
        assert_eq!(b.mood("driving").unwrap().albums.len(), 3);
        assert_eq!(b.decades.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["1970s", "1980s"]);
        assert_eq!(b.decade("1980").unwrap().albums, [2, 0, 1]);
        assert_eq!(decade_key(Some(1987)).as_deref(), Some("1980"));
    }
}
