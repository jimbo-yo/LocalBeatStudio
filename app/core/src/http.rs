//! Shared HTTP client.

use std::sync::OnceLock;
use std::time::Duration;

use ureq::Agent;

pub const USER_AGENT: &str = concat!("ripshelf/", env!("CARGO_PKG_VERSION"));

pub fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .user_agent(USER_AGENT)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into()
    })
}
