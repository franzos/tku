//! Persistent scrub state: the fingerprint salt and the false-positive
//! allowlist. Only salted hashes are stored — never a secret value.

use std::collections::HashSet;
use std::fs;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::paths;
use crate::scrub::fingerprint;

const SALT_BYTES: usize = 32;

/// A detector supplied by the user in `scrub.toml`.
///
/// tku ships rules for the vendors it knows about, but every organisation has
/// internal key formats, and guessing at them from any one person's data is how
/// you end up shipping a pattern that is wrong for everyone else. This is the
/// supported way to add your own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDetector {
    pub id: String,
    pub pattern: String,
    /// Substrings every match must contain. Used as a prefilter — without one
    /// the pattern is run against every string in the corpus.
    #[serde(default)]
    pub literals: Vec<String>,
    /// Capture group holding the value to redact. 0 means the whole match.
    #[serde(default)]
    pub capture: usize,
    /// Keep this many leading bytes in the redaction, as `sk-or-v1-[REDACTED:…]`
    /// does. Omit to replace the whole value.
    #[serde(default)]
    pub mask_prefix: Option<usize>,
    /// Require the match to look like an opaque credential rather than code.
    /// Use for contextual patterns; leave off for a distinctive vendor prefix.
    #[serde(default)]
    pub shape: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScrubState {
    pub salt: String,
    #[serde(default)]
    pub allow: Vec<String>,
    /// Declared last so TOML serialisation emits these tables after the scalars.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<CustomDetector>,
}

impl ScrubState {
    pub fn new_random() -> Result<Self> {
        Ok(Self {
            salt: fingerprint::random_hex(SALT_BYTES)?,
            allow: Vec::new(),
            custom: Vec::new(),
        })
    }

    pub fn fingerprint(&self, value: &str) -> String {
        fingerprint::hash_hex(self.salt.as_bytes(), value)
    }

    /// Pre-built for the scan hot loop, which tests every hit.
    pub fn allowed_set(&self) -> HashSet<&str> {
        self.allow.iter().map(|s| s.as_str()).collect()
    }

    /// Accepts a full 64-hex hash or any unambiguous prefix of one as printed
    /// in the report. Returns false when it was already allowlisted.
    pub fn allow_hash(&mut self, hash: &str) -> bool {
        if self.allow.iter().any(|a| a == hash) {
            return false;
        }
        self.allow.push(hash.to_string());
        true
    }
}

pub fn load_or_init() -> Result<ScrubState> {
    let Some(path) = paths::scrub_state_file() else {
        return ScrubState::new_random();
    };

    match fs::read_to_string(&path) {
        Ok(data) => {
            let state: ScrubState = toml::from_str(&data).with_context(|| {
                format!(
                    "unreadable scrub state at {}. Move it aside and re-run; \
                     regenerating the salt would invalidate every fingerprint \
                     already written into your transcripts.",
                    path.display()
                )
            })?;
            if state.salt.is_empty() {
                bail!(
                    "scrub state at {} has an empty salt. Move it aside and re-run.",
                    path.display()
                );
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let state = ScrubState::new_random()?;
            save(&state)?;
            Ok(state)
        }
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

pub fn save(state: &ScrubState) -> Result<()> {
    let Some(path) = paths::scrub_state_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = toml::to_string_pretty(state)?;
    atomic_write(&path, data.as_bytes(), Some(0o600))
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_is_idempotent_and_hash_only() {
        let mut s = ScrubState {
            salt: "deadbeef".into(),
            allow: Vec::new(),
            custom: Vec::new(),
        };
        let h = s.fingerprint(concat!("AKIAIO", "SFODNN7EXAMPLE"));
        assert!(s.allow_hash(&h));
        assert!(!s.allow_hash(&h));

        let allowed = s.allowed_set();
        assert!(allowed.contains(s.fingerprint(concat!("AKIAIO", "SFODNN7EXAMPLE")).as_str()));
        assert!(!allowed.contains(s.fingerprint(concat!("AKIAQN", "PYDW3K2LMNOPQR")).as_str()));
        assert!(!s.allow.iter().any(|a| a.contains("AKIA")));
    }

    #[test]
    fn state_round_trips_through_toml() {
        let mut s = ScrubState {
            salt: "abc123".into(),
            allow: Vec::new(),
            custom: Vec::new(),
        };
        let h = s.fingerprint("value-one");
        s.allow_hash(&h);
        let text = toml::to_string_pretty(&s).unwrap();
        let back: ScrubState = toml::from_str(&text).unwrap();
        assert_eq!(back.salt, "abc123");
        assert_eq!(back.allow.len(), 1);
        assert!(back
            .allowed_set()
            .contains(back.fingerprint("value-one").as_str()));
    }

    #[test]
    fn a_new_state_has_a_full_width_salt() {
        let s = ScrubState::new_random().unwrap();
        assert_eq!(s.salt.len(), SALT_BYTES * 2);
    }
}
