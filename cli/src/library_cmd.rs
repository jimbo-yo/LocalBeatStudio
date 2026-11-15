//! `localbeat library`: scan the music library (as the player sees it) and
//! report; `--covers` also makes every album's cover thumbnail.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::Result;
use localbeat_core::config::Settings;
use localbeat_core::covers;
use localbeat_core::library::{Library, Origin};

pub fn run(make_covers: bool, list: bool) -> Result<bool> {
    let s = Settings::load();
    let t = Instant::now();
    let (lib, st) = Library::scan(&s, &|done, total| {
        eprint!("\r  reading tags: {done}/{total}   ");
    })?;
    eprintln!();
    println!(
        "{} songs, {} albums, {} artists in {:.1} s ({} files read, {} from the cache)",
        lib.songs.len(),
        lib.albums.len(),
        lib.artists.len(),
        t.elapsed().as_secs_f64(),
        st.read,
        st.files - st.read
    );
    let mut formats: BTreeMap<&str, usize> = BTreeMap::new();
    for x in &lib.songs {
        *formats.entry(if x.drm { "DRM (m4p)" } else { x.codec.as_str() }).or_default() += 1;
    }
    println!("Formats: {}", formats.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", "));
    let flac = lib.albums.iter().filter(|a| a.origin == Origin::Flac).count();
    println!("Albums: {flac} CD rips, {} from Miscellaneous; {} songs hidden (FLAC copy wins)", lib.albums.len() - flac, st.hidden_duplicates);
    let hours = lib.songs.iter().map(|x| x.seconds).sum::<f64>() / 3600.0;
    println!("Playing time: {hours:.0} hours");
    if !st.failed.is_empty() {
        println!("{} files couldn't be read (listed by file name):", st.failed.len());
        for (p, e) in st.failed.iter().take(10) {
            println!("  {}: {e}", p.display());
        }
    }
    println!("Recently added:");
    for &i in &lib.recently_added(5) {
        let a = &lib.albums[i];
        println!("  {} – {}", a.artist, a.title);
    }
    if list {
        for a in &lib.albums {
            println!("{} – {} ({}, {} songs{})", a.artist, a.title, a.year.map(|y| y.to_string()).unwrap_or("?".into()), a.songs.len(), if a.origin == Origin::Misc { ", misc" } else { "" });
        }
    }
    if make_covers {
        let t = Instant::now();
        let (mut made, mut none, mut failed) = (0, 0, 0);
        for (i, a) in lib.albums.iter().enumerate() {
            eprint!("\r  covers: {}/{}   ", i + 1, lib.albums.len());
            match covers::thumbnail(a, &lib.songs, &s.cache_dir, 512) {
                Ok(Some(_)) => made += 1,
                Ok(None) => none += 1,
                Err(e) => {
                    failed += 1;
                    eprintln!("\n  {} – {}: {e:#}", a.artist, a.title);
                }
            }
        }
        eprintln!();
        println!("Covers: {made} thumbnails, {none} albums without a cover, {failed} failed, {:.1} s", t.elapsed().as_secs_f64());
    }
    Ok(true)
}
