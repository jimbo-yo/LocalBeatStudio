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
    out = trimmed.to_string();

    if out.is_empty() {
        return "_".to_string();
    }
    if is_reserved(&out) {
        out.push('_');
    }
    out
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
}
