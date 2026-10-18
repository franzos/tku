//! Shared `ureq::Agent` with conservative defaults.
//!
//! One agent gets reused across all HTTP callers (subscription, exchange,
//! pricing) — this shares the connection pool and TLS session cache, which
//! matters on cold runs that hit several endpoints. All our URLs are
//! hardcoded `https://`, so `https_only(true)` is a cheap defense-in-depth
//! belt. `max_redirects(2)` mirrors what the previous implicit agent
//! effectively allowed for our endpoints; we don't chase long chains.
//! Per-phase timeouts stop a stalled network from hanging a command; the
//! body allowance is generous so the multi-MB pricing download still
//! finishes on slow links.

use std::sync::OnceLock;
use std::time::Duration;

use ureq::Agent;

static AGENT: OnceLock<Agent> = OnceLock::new();

/// Returns a process-wide shared `ureq::Agent`.
pub fn agent() -> &'static Agent {
    AGENT.get_or_init(|| {
        let config = Agent::config_builder()
            .https_only(true)
            .max_redirects(2)
            .timeout_resolve(Some(Duration::from_secs(10)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(Duration::from_secs(120)))
            .build();
        Agent::new_with_config(config)
    })
}
