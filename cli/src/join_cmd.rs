//! `localbeat join`: Studio's "join the server" from a terminal (docs/joining.md).

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use localbeat_core::config::{Prefs, Settings};
use localbeat_core::join::{self, Pending, Role, Status};
use localbeat_engine::join as engine_join;

pub struct JoinArgs {
    pub name: Option<String>,
    pub admin: bool,
    pub server: String,
    pub cancel: bool,
    pub user: Option<String>,
}

pub fn run(a: JoinArgs) -> Result<bool> {
    let s = Settings::load();
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
            let name = a.name.unwrap_or_else(join::suggested_name);
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
