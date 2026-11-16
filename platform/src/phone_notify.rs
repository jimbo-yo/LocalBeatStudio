//! A notification on the phone through KDE Connect (`kdeconnect-cli
//! --ping-msg`): local network only, no accounts. KDE Connect for Windows
//! ships the same command line tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Sends `message` to a reachable phone and returns its name. Prefers the
/// phone Phone Sync uses (`kdeconnect_device` in `<config_dir>/phone-sync.json`),
/// else the first reachable device.
pub fn notify(config_dir: &Path, message: &str) -> Result<String> {
    let exe = kdeconnect_cli();
    let out = crate::command(&exe)
        .args(["-a", "--id-name-only"])
        .output()
        .with_context(|| format!("running {} (is KDE Connect installed?)", exe.display()))?;
    let devices: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(id, name)| (id.to_string(), name.to_string())))
        .collect();
    if devices.is_empty() {
        bail!("no phone is reachable through KDE Connect (is it on the same Wi-Fi?)");
    }
    let preferred = phone_sync_device(config_dir);
    let (id, name) = devices.iter().find(|(id, _)| Some(id) == preferred.as_ref()).unwrap_or(&devices[0]);
    let status = crate::command(&exe)
        .args(["-d", id, "--ping-msg", message])
        .status()
        .with_context(|| format!("running {}", exe.display()))?;
    if !status.success() {
        bail!("KDE Connect couldn't send the notification to {name}");
    }
    Ok(name.clone())
}

fn phone_sync_device(config_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config_dir.join("phone-sync.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("kdeconnect_device")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn kdeconnect_cli() -> PathBuf {
    #[cfg(windows)]
    {
        // The Windows installer doesn't put it on PATH.
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(base) {
                let p = PathBuf::from(dir).join("KDE Connect").join("bin").join("kdeconnect-cli.exe");
                if p.exists() {
                    return p;
                }
            }
        }
    }
    PathBuf::from("kdeconnect-cli")
}
