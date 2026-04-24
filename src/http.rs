//! Shared `ureq::Agent` with conservative defaults.
//!
//! One agent gets reused across all HTTP callers (subscription, exchange,
//! pricing) — this shares the connection pool and TLS session cache, which
//! matters on cold runs that hit several endpoints. All our URLs are
//! hardcoded `https://`, so `https_only(true)` is a cheap defense-in-depth
//! belt. `max_redirects(2)` mirrors what the previous implicit agent
//! effectively allowed for our endpoints; we don't chase long chains.

use std::sync::OnceLock;

use ureq::Agent;

static AGENT: OnceLock<Agent> = OnceLock::new();

/// Returns a process-wide shared `ureq::Agent`.
pub fn agent() -> &'static Agent {
    AGENT.get_or_init(|| {
        let config = Agent::config_builder()
            .https_only(true)
            .max_redirects(2)
            .build();
        Agent::new_with_config(config)
    })
}
