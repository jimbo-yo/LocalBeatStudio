//! `localbeat superseded`: Miscellaneous folders that are now FLAC rips
//! (same folder name, or same album artist + album tags). Lists them; with
//! --move, moves them out of the library into `Music/Superseded` (nothing
//! is deleted; the phone sync still replaces their old phone copies).

use anyhow::{Context, Result};
use localbeat_core::config::Settings;
use localbeat_core::phone_sync::{self, Origin};
use serde_json::json;

fn dir_size(dir: &std::path::Path) -> (u64, usize) {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .fold((0, 0), |(b, n), e| (b + e.metadata().map(|m| m.len()).unwrap_or(0), n + 1))
}

pub fn run(move_them: bool) -> Result<bool> {
    let s = Settings::load();
    let lib = phone_sync::scan_library(&s.flac_dir, &s.misc_dir);
    let pairs: Vec<(String, String)> = lib
        .iter()
        .filter(|a| a.origin == Origin::Flac)
        .flat_map(|a| a.also_in_misc.iter().map(move |m| (m.clone(), a.rel.clone())))
        .collect();
    if pairs.is_empty() {
        println!("No Miscellaneous folders are FLAC albums now.");
        return Ok(true);
    }
    let dest_root = s.misc_dir.parent().map(|p| p.join("Superseded")).unwrap_or_else(|| s.misc_dir.join("..").join("Superseded"));
    let mut total = 0;
    println!("Miscellaneous folders now in the FLAC library:");
    for (misc, flac) in &pairs {
        let (bytes, files) = dir_size(&s.misc_dir.join(misc));
        total += bytes;
        println!("  {misc}  ({files} files, {:.0} MB)\n      = FLAC {flac}", bytes as f64 / 1e6);
    }
    println!("{} folders, {:.1} GB", pairs.len(), total as f64 / 1e9);
    if !move_them {
        println!("To move them out of the library (into {}): localbeat superseded --move", dest_root.display());
        return Ok(true);
    }

    let record_file = phone_sync::superseded_file(&s.config_dir);
    let mut record: serde_json::Value = std::fs::read(&record_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"moved": []}));
    let mut moved = 0;
    for (misc, flac) in &pairs {
        let from = s.misc_dir.join(misc);
        let to = dest_root.join(misc);
        if to.exists() {
            println!("  ✘ {misc}: {} already exists, left in place", to.display());
            continue;
        }
        std::fs::create_dir_all(to.parent().unwrap())?;
        std::fs::rename(&from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        // Tidy an emptied parent ("Artist/" with no albums left).
        if let Some(parent) = from.parent().filter(|p| *p != s.misc_dir) {
            let _ = std::fs::remove_dir(parent); // only succeeds if empty
        }
        record["moved"].as_array_mut().unwrap().push(json!({"misc": misc, "flac": flac, "to": to.to_string_lossy()}));
        std::fs::write(&record_file, serde_json::to_vec_pretty(&record)?)?;
        println!("  moved {misc}");
        moved += 1;
    }
    println!(
        "Moved {moved} folders to {}. Nothing was deleted: check them, then delete that folder when you're happy.\n\
         Record (for the phone sync): {}",
        dest_root.display(),
        record_file.display()
    );
    Ok(true)
}
