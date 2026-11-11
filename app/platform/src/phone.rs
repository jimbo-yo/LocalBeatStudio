//! Reaching the phone's music folder, for the phone sync
//! (`localbeat_core::phone_sync`): USB through adb, or Wi-Fi through KDE
//! Connect's mount of the phone's storage. Ported from phone-sync.py.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, anyhow, bail};
use localbeat_core::phone_sync::{FolderPhone, PhoneFs, Reconnect};

/// Default music folders: what LocalBeat/Musicolet read ("Internal storage/Music").
pub const ADB_MUSIC: &str = "/sdcard/Music";
pub const KDECONNECT_MUSIC: &str = "Music";

// ---------------------------------------------------------------- USB (adb)

pub struct AdbPhone {
    serial: String,
    root: String,
    pub model: String,
}

/// Quotes for the phone's shell (mksh/toybox).
fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn adb_exe() -> PathBuf {
    PathBuf::from(if cfg!(windows) { "adb.exe" } else { "adb" })
}

fn run(cmd: &mut Command) -> Result<Output> {
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("adb isn't installed (Fedora: sudo dnf install android-tools; Ubuntu: sudo apt install adb)")
        } else {
            anyhow!(e)
        }
    })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if msg.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { msg };
        bail!("{}", if msg.is_empty() { "adb failed".to_string() } else { msg });
    }
    Ok(out)
}

impl AdbPhone {
    /// The first phone connected over USB with debugging allowed.
    pub fn connect(root: &str) -> Result<Self> {
        let out = run(Command::new(adb_exe()).arg("devices"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let devices: Vec<(&str, &str)> = text
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?, it.next()?))
            })
            .collect();
        // Prefer the cable when the phone is also on wireless debugging: it's
        // faster, and adb otherwise uses whichever it lists first.
        let wireless = |serial: &str| serial.contains("tls-connect") || serial.contains(':');
        let ready = |(s, state): &&(&str, &str)| *state == "device" && !s.is_empty();
        let Some((serial, _)) = devices.iter().filter(ready).find(|(s, _)| !wireless(s)).or_else(|| devices.iter().find(ready)) else {
            if devices.iter().any(|(_, s)| *s == "unauthorized") {
                bail!("unlock the phone and tap Allow on the \"Allow USB debugging?\" prompt");
            }
            bail!("no phone found over USB: plug it in and turn on USB debugging (Settings > System > Developer options)");
        };
        let mut p = AdbPhone { serial: serial.to_string(), root: root.trim_end_matches('/').to_string(), model: String::new() };
        p.shell(&format!("mkdir -p {}", sh(&p.root)))?;
        p.model = String::from_utf8_lossy(&p.shell("getprop ro.product.model")?.stdout).trim().to_string();
        Ok(p)
    }

    fn adb(&self) -> Command {
        let mut c = Command::new(adb_exe());
        c.args(["-s", &self.serial]);
        c
    }

    fn shell(&self, cmd: &str) -> Result<Output> {
        run(self.adb().args(["shell", cmd]))
    }

    fn path(&self, rel: &str) -> String {
        format!("{}/{rel}", self.root)
    }

    pub fn describe(&self) -> String {
        // Wireless debugging shows up as "adb-<serial>-<id>._adb-tls-connect._tcp" or ip:port.
        let how = if self.serial.contains("tls-connect") || self.serial.contains(':') {
            "over Wi-Fi (wireless debugging)"
        } else {
            "over USB"
        };
        format!("{} {how}, folder {}", if self.model.is_empty() { &self.serial } else { &self.model }, self.root)
    }
}

impl PhoneFs for AdbPhone {
    fn list(&self) -> Result<BTreeMap<String, u64>> {
        let out = self.shell(&format!("find {} -type f -exec stat -c '%s|%n' {{}} +", sh(&self.root)))?;
        let prefix = format!("{}/", self.root);
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (size, path) = l.split_once('|')?;
                let rel = path.strip_prefix(&prefix)?;
                let name = rel.rsplit('/').next().unwrap_or(rel);
                if name.starts_with('.') && name.ends_with(".part") {
                    return None; // an interrupted copy
                }
                Some((rel.to_string(), size.parse().ok()?))
            })
            .collect())
    }

    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        let p = self.path(rel);
        let exists = self.shell(&format!("[ -f {} ] && echo yes || true", sh(&p)))?;
        if !String::from_utf8_lossy(&exists.stdout).contains("yes") {
            return Ok(None);
        }
        Ok(Some(run(self.adb().args(["exec-out", &format!("cat {}", sh(&p))]))?.stdout))
    }

    fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        let tmp = std::env::temp_dir().join(format!("localbeat-adb-{}.tmp", std::process::id()));
        std::fs::write(&tmp, data)?;
        let r = self.push(&tmp, rel);
        let _ = std::fs::remove_file(&tmp);
        r
    }

    /// Sent as a hidden `.<name>.part` next to its place, then renamed, so the
    /// phone's media scanner never sees half a file.
    fn push(&self, local: &Path, rel: &str) -> Result<()> {
        let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        let dest = self.path(rel);
        let part = if dir.is_empty() { self.path(&format!(".{name}.part")) } else { self.path(&format!("{dir}/.{name}.part")) };
        // adb push sends the path as is (no shell), so odd characters are fine.
        run(self.adb().arg("push").arg(local).arg(&part)).with_context(|| format!("copying to {rel}"))?;
        self.shell(&format!("mv -f {} {}", sh(&part), sh(&dest))).with_context(|| format!("copying to {rel}"))?;
        Ok(())
    }

    fn remove_file(&self, rel: &str) -> Result<()> {
        self.shell(&format!("rm -f {}", sh(&self.path(rel))))?;
        Ok(())
    }

    fn remove_dir(&self, rel: &str) -> Result<()> {
        if rel.is_empty() || rel.split('/').any(|p| p == ".." || p.is_empty()) {
            bail!("refusing an odd phone path: {rel:?}");
        }
        let p = self.path(rel);
        let parent = p.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let tidy = if parent != self.root { format!("; rmdir {} 2>/dev/null", sh(&parent)) } else { String::new() };
        self.shell(&format!("rm -rf {}{tidy}; true", sh(&p)))?;
        Ok(())
    }

    fn space(&self) -> Result<Option<(u64, u64)>> {
        let out = self.shell(&format!("df -k {}", sh(&self.root)))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let f: Vec<&str> = text.lines().last().unwrap_or("").split_whitespace().collect();
        match (f.get(1).and_then(|v| v.parse::<u64>().ok()), f.get(3).and_then(|v| v.parse::<u64>().ok())) {
            (Some(total), Some(free)) => Ok(Some((total * 1024, free * 1024))),
            _ => bail!("couldn't read the phone's free space: {:?}", text.trim()),
        }
    }
}

impl Reconnect for AdbPhone {
    fn reconnect(&self) -> Result<()> {
        // Not wait-for-device: that blocks forever; the caller retries.
        let out = run(Command::new(adb_exe()).args(["-s", &self.serial, "get-state"]))?;
        if String::from_utf8_lossy(&out.stdout).trim() == "device" { Ok(()) } else { bail!("the phone isn't connected") }
    }
}

// ---------------------------------------------------------------- Wi-Fi (KDE Connect)

pub struct KdeConnectPhone {
    folder: FolderPhone,
    device: String,
    subfolder: String,
    pub name: String,
}

fn qdbus(path: &str, method: &str) -> Result<String> {
    let exe = ["qdbus-qt6", "qdbus6", "qdbus"]
        .into_iter()
        .find(|e| Command::new(e).arg("--version").output().is_ok())
        .ok_or_else(|| anyhow!("qdbus not found (Fedora: qt6-qttools; Ubuntu: qdbus-qt6)"))?;
    let out = run(Command::new(exe).args(["org.kde.kdeconnect", path, method]))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl KdeConnectPhone {
    /// Mounts the phone's storage through KDE Connect. `preferred` is a
    /// device id (Phone Sync's choice), else the first reachable phone.
    pub fn connect(preferred: Option<&str>, subfolder: &str) -> Result<Self> {
        let out = Command::new("kdeconnect-cli")
            .args(["-a", "--id-name-only"])
            .output()
            .context("running kdeconnect-cli (is KDE Connect installed?)")?;
        let devices: Vec<(String, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())))
            .collect();
        let (device, name) = devices
            .iter()
            .find(|(id, _)| Some(id.as_str()) == preferred)
            .or(devices.first())
            .cloned()
            .ok_or_else(|| anyhow!("no phone is reachable over Wi-Fi: open KDE Connect on the phone and check it's on the same Wi-Fi and paired"))?;
        let mut p = KdeConnectPhone {
            folder: FolderPhone { root: PathBuf::new() },
            device,
            subfolder: subfolder.trim_matches('/').to_string(),
            name,
        };
        p.mount()?;
        Ok(p)
    }

    fn mount(&mut self) -> Result<()> {
        let sftp = format!("/modules/kdeconnect/devices/{}/sftp", self.device);
        if qdbus(&sftp, "org.kde.kdeconnect.device.sftp.mountAndWait")? != "true" {
            let why = qdbus(&sftp, "org.kde.kdeconnect.device.sftp.getMountError").unwrap_or_default();
            let hint = if why.contains("ermission") {
                " On the phone: open KDE Connect, tap this computer and allow Filesystem expose (or Settings > Apps > KDE Connect > Permissions > Files: allow management of all files)."
            } else {
                ""
            };
            bail!("KDE Connect couldn't open the phone's storage ({}).{hint}", if why.is_empty() { "no reason given" } else { &why });
        }
        let mount = qdbus(&sftp, "org.kde.kdeconnect.device.sftp.mountPoint")?;
        // The shared storage is a folder inside the mount (.../storage/emulated/0).
        let dirs = qdbus(&sftp, "org.kde.kdeconnect.device.sftp.getDirectories").unwrap_or_default();
        let base = dirs
            .lines()
            .filter(|l| l.starts_with('/'))
            .map(|l| l.split_once(':').map(|(p, _)| p).unwrap_or(l).trim().to_string())
            .next()
            .unwrap_or(mount);
        let root = Path::new(&base).join(&self.subfolder);
        std::fs::create_dir_all(&root).with_context(|| format!("opening {}", root.display()))?;
        self.folder = FolderPhone { root };
        Ok(())
    }

    pub fn describe(&self) -> String {
        format!("{} over Wi-Fi, folder {}", self.name, self.subfolder)
    }
}

impl PhoneFs for KdeConnectPhone {
    fn list(&self) -> Result<BTreeMap<String, u64>> {
        // An empty listing of an unmounted folder would look like "no music":
        // make sure the mount is really there.
        if !self.folder.root.is_dir() {
            bail!("the phone's storage isn't mounted");
        }
        self.folder.list()
    }
    fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        self.folder.read(rel)
    }
    fn write(&self, rel: &str, data: &[u8]) -> Result<()> {
        self.folder.write(rel, data)
    }
    fn push(&self, local: &Path, rel: &str) -> Result<()> {
        self.folder.push(local, rel)
    }
    fn remove_file(&self, rel: &str) -> Result<()> {
        self.folder.remove_file(rel)
    }
    fn remove_dir(&self, rel: &str) -> Result<()> {
        self.folder.remove_dir(rel)
    }
    /// KDE Connect's SFTP doesn't report space (sshfs shows a made-up
    /// "1000 GiB, all free"), so: unknown.
    fn space(&self) -> Result<Option<(u64, u64)>> {
        Ok(None)
    }
}

impl Reconnect for KdeConnectPhone {
    fn reconnect(&self) -> Result<()> {
        let sftp = format!("/modules/kdeconnect/devices/{}/sftp", self.device);
        match qdbus(&sftp, "org.kde.kdeconnect.device.sftp.mountAndWait")?.as_str() {
            "true" => Ok(()),
            _ => bail!("KDE Connect couldn't reach the phone"),
        }
    }
}

/// Phone Sync's remembered KDE Connect device, to prefer the same phone.
pub fn preferred_kdeconnect_device(config_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(config_dir.join("phone-sync.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("kdeconnect_device")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}
