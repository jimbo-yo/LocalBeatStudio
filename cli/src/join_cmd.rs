//! `localbeat join` and `localbeat invite` (docs/onboarding.md).

use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{self, Pending, Role, Status};
use localbeat_engine::join as engine_join;

pub struct JoinArgs {
    /// Either a `localbeat://join/...` invite URI, or (legacy) this computer's name.
    pub target: Option<String>,
    pub admin: bool,
    pub server: String,
    pub cancel: bool,
    pub user: Option<String>,
    /// Replace an existing server setup without asking.
    pub force: bool,
}

#[derive(Subcommand)]
pub enum InviteAction {
    /// Create a new 7-day single-use invite (shows a localbeat://join/... URI)
    Create {
        /// Whose device this is (user name on the server, e.g. sarah)
        #[arg(short, long)]
        user: String,
        /// Grant admin access (can rip CDs and send them to the server)
        #[arg(long)]
        admin: bool,
    },
    /// List active unclaimed invites on the server
    List,
    /// Revoke an unclaimed invite and remove its bootstrap WireGuard peer
    Revoke {
        /// Invite ID (from `localbeat invite list`)
        id: String,
    },
}

pub fn run(a: JoinArgs) -> Result<bool> {
    let s = Settings::load();

    // Invite-based onboarding (`localbeat join localbeat://join/...`).
    if let Some(text) = a.target.as_deref()
        && join::extract_invite_uri(text).is_some()
    {
        let mut prefs = Prefs::load(&s);
        let already_joined = prefs.role.is_some() && !prefs.server.login.is_empty();
        if already_joined && !a.force {
            bail!(
                "this computer is already joined as {} — pass --force to replace it with this invite",
                prefs.server.login
            );
        }
        println!("Connecting to the music server to claim invite...");
        let outcome = engine_join::claim(&s, text, &mut prefs)?;
        for line in &outcome.log {
            println!("  {line}");
        }
        return Ok(true);
    }

    // Legacy LAN request-and-approve flow (removed in cutover step 9).
    let mut p = match Pending::load(&s.config_dir) {
        Some(p) if a.cancel => {
            engine_join::cancel(&s, &p);
            println!("Request for {} withdrawn.", p.name);
            return Ok(true);
        }
        Some(p) => {
            println!("Carrying on with the request for {} ({}).", p.name, p.role.as_str());
            p
        }
        None if a.cancel => {
            println!("No request waiting.");
            return Ok(true);
        }
        None => {
            if a.server.trim().is_empty() {
                bail!("pass a localbeat://join/... invite link (or legacy --server <ip-or-host>)");
            }
            let name = a.target.unwrap_or_else(join::suggested_name);
            let role = if a.admin { Role::Admin } else { Role::Listener };
            engine_join::ask(&s, &a.server, &name, a.user.as_deref().unwrap_or(""), role)?
        }
    };
    println!();
    println!("    Code: {}", p.code);
    println!();
    println!("Approve {} ({}) on the server's admin page, {} , if the code there matches.", p.name, p.role.as_str(), p.admin_url);
    println!("Waiting (Ctrl+C to stop; run localbeat join again to carry on)...");
    let start = Instant::now();
    loop {
        match engine_join::check(&s, &mut p) {
            Ok(Status::Waiting) => {}
            Ok(Status::Denied) => {
                engine_join::cancel(&s, &p);
                bail!("the admin said no");
            }
            Ok(Status::Approved(_)) => break,
            Err(e) => {
                // Not found = expired: start over next time.
                if format!("{e:#}").contains("no longer knows") {
                    Pending::remove(&s.config_dir);
                }
                return Err(e);
            }
        }
        if start.elapsed() > Duration::from_secs(3600) {
            bail!("still waiting after an hour: the request has expired; run localbeat join again");
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    println!("Approved. Setting up (Windows asks for permission once, for the tunnel)...");
    let mut prefs = Prefs::load(&s);
    for line in engine_join::finish(&s, &p, &mut prefs)? {
        println!("  {line}");
    }
    Ok(true)
}

fn read_admin_password() -> Result<String> {
    if let Ok(pw) = std::env::var("LOCALBEAT_ADMIN_PASSWORD")
        && !pw.is_empty()
    {
        return Ok(pw);
    }
    eprint!("Admin password: ");
    io::stderr().flush().ok();
    let mut pw = String::new();
    io::stdin().read_line(&mut pw).context("reading admin password")?;
    let pw = pw.trim_end_matches(&['\r', '\n'][..]).to_string();
    if pw.is_empty() {
        bail!("admin password cannot be empty");
    }
    Ok(pw)
}

pub fn run_invite(action: Option<InviteAction>) -> Result<bool> {
    let s = Settings::load();
    let prefs = Prefs::load(&s);
    let pw = read_admin_password()?;
    let session = engine_join::admin_login(&s, &prefs, &pw)?;

    match action.unwrap_or(InviteAction::List) {
        InviteAction::Create { user, admin } => {
            let role = if admin { Role::Admin } else { Role::Listener };
            let inv = engine_join::create_invite(&s, &prefs, &session.session_token, &user, role)?;
            println!("Invite {} created for {} ({}), expires {}:", inv.id, inv.user, inv.role.as_str(), inv.expires_at);
            println!();
            println!("  {}", inv.uri);
            println!();
        }
        InviteAction::List => {
            let list = engine_join::list_invites(&s, &prefs, &session.session_token)?;
            if list.is_empty() {
                println!("No active unclaimed invites.");
            } else {
                for inv in list {
                    println!("{}  user={}  role={}  expires={}", inv.id, inv.user, inv.role.as_str(), inv.expires_at);
                    println!("  {}", inv.uri);
                }
            }
        }
        InviteAction::Revoke { id } => {
            engine_join::revoke_invite(&s, &prefs, &session.session_token, &id)?;
            println!("Invite {id} revoked.");
        }
    }
    Ok(true)
}
