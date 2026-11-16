//! The WireGuard tunnel to the LocalBeat server, named "localbeat"
//! (docs/joining.md): installed from the config a join produced, and
//! asked about for the Settings page.
//!
//! - Linux: imported into NetworkManager (connects by itself, only the
//!   server's address goes through it), like tools/import-tunnel.py. NM keeps
//!   it in /etc/NetworkManager/system-connections, root only.
//! - Windows: WireGuard for Windows' tunnel service, which reads its config
//!   from the given path at every start (it doesn't copy it). So the config
//!   goes to %ProgramData%\LocalBeat\localbeat.conf, readable by SYSTEM and
//!   Administrators only, through one UAC prompt.

use anyhow::{Context, Result, bail};

pub const NAME: &str = "localbeat";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No "localbeat" tunnel on this computer.
    Missing,
    /// Installed, not up right now.
    Down,
    Up,
}

/// Can a tunnel be installed here? Err says what to install.
pub fn available() -> Result<(), String> {
    #[cfg(windows)]
    {
        if wireguard_exe().is_file() { Ok(()) } else { Err("WireGuard for Windows isn't installed".into()) }
    }
    #[cfg(not(windows))]
    {
        match crate::command("nmcli").arg("--version").output() {
            Ok(o) if o.status.success() => Ok(()),
            _ => Err("NetworkManager (nmcli) isn't available".into()),
        }
    }
}

#[cfg(windows)]
fn wireguard_exe() -> std::path::PathBuf {
    let pf = std::env::var_os("ProgramFiles").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into());
    pf.join(r"WireGuard\wireguard.exe")
}

/// A private scratch folder for the config on its way in.
fn scratch() -> Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("localbeat-tunnel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Installs (or replaces) the tunnel from a wg-quick style `config`, and
/// starts it.
pub fn install(config: &str) -> Result<()> {
    available().map_err(|e| anyhow::anyhow!(e))?;
    let dir = scratch()?;
    let conf = dir.join(format!("{NAME}.conf"));
    let r = (|| {
        std::fs::write(&conf, config).with_context(|| format!("writing {}", conf.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600))?;
        }
        install_from(&conf)
    })();
    let _ = std::fs::remove_dir_all(&dir); // the private key never stays in a temp folder
    r
}

#[cfg(not(windows))]
fn nmcli(args: &[&str]) -> Result<String> {
    let out = crate::command("nmcli").args(args).output().context("running nmcli")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("nmcli {}: {}", args.first().unwrap_or(&""), if e.is_empty() { "failed".into() } else { e });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(windows))]
fn install_from(conf: &std::path::Path) -> Result<()> {
    if state() != State::Missing {
        nmcli(&["connection", "delete", NAME])?;
    }
    // NetworkManager names the connection and the interface after the file.
    let path = conf.to_string_lossy();
    nmcli(&["connection", "import", "type", "wireguard", "file", &path])?;
    nmcli(&["connection", "modify", NAME, "connection.autoconnect", "yes"])?;
    nmcli(&["connection", "up", NAME]).context("the tunnel was added but didn't come up")?;
    Ok(())
}

#[cfg(windows)]
fn install_from(conf: &std::path::Path) -> Result<()> {
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:ProgramData 'LocalBeat'
New-Item -ItemType Directory -Force $dir | Out-Null
# SYSTEM and Administrators only: the file holds the tunnel's private key.
icacls $dir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
$dest = Join-Path $dir '{NAME}.conf'
Copy-Item -LiteralPath '{src}' -Destination $dest -Force
Remove-Item -LiteralPath '{src}' -Force
if (Get-Service 'WireGuardTunnel${NAME}' -ErrorAction SilentlyContinue) {{
    & '{wg}' /uninstalltunnelservice {NAME}
    for ($i = 0; $i -lt 20 -and (Get-Service 'WireGuardTunnel${NAME}' -ErrorAction SilentlyContinue); $i++) {{ Start-Sleep -Milliseconds 250 }}
}}
& '{wg}' /installtunnelservice $dest
"#,
        src = q(conf),
        wg = q(&wireguard_exe()),
    );
    let encoded = {
        use base64::Engine as _;
        let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        base64::engine::general_purpose::STANDARD.encode(utf16)
    };
    // Windows PowerShell is always there; Start-Process -Verb RunAs is the
    // UAC prompt. Declining it is an error with "canceled by the user".
    let outer = format!(
        "$p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-EncodedCommand','{encoded}'; exit $p.ExitCode"
    );
    let out = crate::command("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &outer])
        .output()
        .context("running PowerShell")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        if e.contains("canceled by the user") {
            bail!("Windows asked for permission and it was declined: the tunnel needs it once");
        }
        bail!("installing the tunnel failed ({})", e.lines().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim());
    }
    // The service takes a moment to appear.
    for _ in 0..20 {
        if state() != State::Missing {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("WireGuard didn't set the tunnel up (no WireGuardTunnel${NAME} service)")
}

/// Is the tunnel installed, and up?
pub fn state() -> State {
    #[cfg(windows)]
    {
        let Ok(out) = crate::command("sc.exe").args(["query", &format!("WireGuardTunnel${NAME}")]).output() else {
            return State::Missing;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            State::Missing
        } else if text.contains("RUNNING") {
            State::Up
        } else {
            State::Down
        }
    }
    #[cfg(not(windows))]
    {
        let names = |active: bool| -> Option<bool> {
            let mut args = vec!["-t", "-f", "NAME", "connection", "show"];
            if active {
                args.push("--active");
            }
            Some(nmcli(&args).ok()?.lines().any(|l| l == NAME))
        };
        match (names(true), names(false)) {
            (Some(true), _) => State::Up,
            (_, Some(true)) => State::Down,
            _ => State::Missing,
        }
    }
}
