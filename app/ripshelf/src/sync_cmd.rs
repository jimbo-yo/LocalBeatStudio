//! `ripshelf sync`: the phone sync from the command line, over USB (adb),
//! Wi-Fi (KDE Connect) or to a folder (tests).

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use ripshelf_core::config::Settings;
use ripshelf_core::phone_sync::{self, AlbumAction, FolderPhone, Patient, PhoneFs, Reconnect, SyncEvent};
use ripshelf_platform::phone::{self, AdbPhone, KdeConnectPhone};

pub struct SyncArgs {
    pub to: Option<PathBuf>,
    pub usb: bool,
    pub wifi: bool,
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub dry_run: bool,
}

fn mb(b: u64) -> String {
    if b >= 1_000_000_000 { format!("{:.1} GB", b as f64 / 1e9) } else { format!("{:.0} MB", b as f64 / 1e6) }
}

pub fn sync(a: SyncArgs) -> Result<bool> {
    let s = Settings::load();
    match (&a.to, a.usb, a.wifi) {
        (Some(dir), false, false) => {
            let p = FolderPhone { root: dir.clone() };
            run(&p, &format!("folder {}", dir.display()), &s, &a)
        }
        (None, true, false) => {
            let p = AdbPhone::connect(phone::ADB_MUSIC)?;
            run(&p, &p.describe(), &s, &a)
        }
        (None, false, true) => {
            let p = KdeConnectPhone::connect(phone::preferred_kdeconnect_device(&s.config_dir).as_deref(), phone::KDECONNECT_MUSIC)?;
            run(&p, &p.describe(), &s, &a)
        }
        _ => bail!("say where the phone is: --usb, --wifi, or --to <folder>"),
    }
}

fn run<P: PhoneFs + Reconnect + Sync>(raw: &P, desc: &str, s: &Settings, a: &SyncArgs) -> Result<bool> {
    let fs = Patient {
        phone: raw,
        wait: std::time::Duration::from_secs(600),
        on_wait: &|msg| println!("  {msg}"),
        cancelled: &|| false,
    };
    println!("Library: {} + {}", s.flac_dir.display(), s.misc_dir.display());
    let lib = phone_sync::scan_library(&s.flac_dir, &s.misc_dir);
    let phone = phone_sync::read_phone(&fs)?;
    let flac = lib.iter().filter(|x| x.origin == phone_sync::Origin::Flac).count();
    let drm: usize = lib.iter().map(|x| x.protected.len()).sum();
    println!(
        "  {} albums ({flac} FLAC, {} Miscellaneous), {} DRM-protected files skipped",
        lib.len(),
        lib.len() - flac,
        drm
    );
    println!("Phone: {desc}: {} albums, {} of music", phone.albums.len(), mb(phone.music_bytes()));
    match phone.space {
        Some((total, free)) => println!("  {} free of {}", mb(free), mb(total)),
        None => println!("  (free space unknown over this connection)"),
    }

    let matches = |rel: &str, pats: &[String]| pats.iter().any(|p| rel.to_lowercase().contains(&p.to_lowercase()));
    let mut wanted: BTreeSet<String> = phone_sync::on_phone(&lib, &phone);
    for x in &lib {
        if matches(&x.rel, &a.add) {
            wanted.insert(x.key.clone());
        }
        if matches(&x.rel, &a.remove) {
            wanted.remove(&x.key);
        }
    }
    for x in &phone.albums {
        if matches(&x.rel, &a.remove) {
            wanted.remove(&x.key);
        }
    }

    let plan = phone_sync::plan(&lib, &phone, &wanted);
    let name = |key: &str| {
        lib.iter()
            .find(|x| x.key == key)
            .map(|x| format!("{}{}", x.rel, if x.origin == phone_sync::Origin::Misc { " (Misc)" } else { "" }))
            .or_else(|| phone.album(key).map(|x| format!("{} (phone only)", x.rel)))
            .unwrap_or_else(|| key.to_string())
    };
    let mut counts = std::collections::BTreeMap::new();
    for (key, action) in &plan.actions {
        let label = match action {
            AlbumAction::None => continue,
            AlbumAction::Add => "add",
            AlbumAction::Update { .. } => "update",
            AlbumAction::Replace => "replace",
            AlbumAction::Adopt => "adopt",
            AlbumAction::Remove => "remove",
        };
        *counts.entry(label).or_insert(0) += 1;
        let detail = match action {
            AlbumAction::Update { files } => format!(" ({files} files)"),
            _ => String::new(),
        };
        println!("  {label:8} {}{detail}", name(key));
    }
    println!(
        "Plan: {} | about {} to send, {} to remove",
        if counts.is_empty() { "nothing to do".into() } else { counts.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ") },
        mb(plan.add_bytes()),
        mb(plan.remove_bytes())
    );
    if a.dry_run || plan.is_empty() {
        return Ok(true);
    }

    let sum = phone_sync::execute(
        &fs,
        &phone,
        &plan,
        &s.cache_dir.join("sync-work"),
        s.opus_kbps,
        &|e| match e {
            SyncEvent::Removing(r) => println!("  removing {r}"),
            SyncEvent::Sending { file, done, total } if !file.is_empty() => {
                println!("  [{:3.0}%] {file}", done as f64 * 100.0 / total as f64)
            }
            SyncEvent::Failed { file, error } => println!("  FAILED {file}: {error}"),
            _ => {}
        },
        &|| false,
    )?;
    println!(
        "Done: {} files sent, {} albums removed, {} files removed, {} albums adopted, {} failed",
        sum.sent,
        sum.removed_albums,
        sum.removed_files,
        sum.adopted,
        sum.failed.len()
    );
    Ok(sum.failed.is_empty())
}
