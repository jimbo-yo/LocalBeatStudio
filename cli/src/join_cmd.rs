//! `localbeat join` and `localbeat invite` (docs/onboarding.md).

use std::io::{self, Write};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::Role;
use localbeat_engine::join as engine_join;

pub struct JoinArgs {
    /// A `localbeat://join/...` invite link (or pasted message containing one).
    pub invite: String,
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
    let mut prefs = Prefs::load(&s);
    let already_joined = prefs.role.is_some() && !prefs.server.login.is_empty();
    if already_joined && !a.force {
        bail!(
            "this computer is already joined as {} — pass --force to replace it with this invite",
            prefs.server.login
        );
    }
    println!("Connecting to the music server to claim invite...");
    let outcome = engine_join::claim(&s, &a.invite, &mut prefs)?;
    for line in &outcome.log {
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
