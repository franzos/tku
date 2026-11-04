//! The serde-mode bitcode store payload, `(VERSION, LegacyCache)`, frozen
//! independently of [`UsageRecord`] so changes to it cannot shift this shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::bitcode_store::{CachedFile, ProviderCache};
use crate::types::{self, Provider, UsageRecord, AGG_MARKER};

/// Leading marker of every legacy payload.
const VERSION: u32 = 0x746b_7501;

#[derive(Serialize, Deserialize)]
struct LegacyCache {
    // bitcode's serde mode encodes this exactly like the `HashMap` it was written from.
    files: Vec<(String, LegacyFile)>,
}

#[derive(Serialize, Deserialize)]
struct LegacyFile {
    mtime_secs: i64,
    size: u64,
    records: Vec<LegacyRecord>,
}

#[derive(Serialize, Deserialize)]
struct LegacyRecord {
    provider: LegacyProvider,
    session_id: String,
    timestamp: DateTime<Utc>,
    project: String,
    model: String,
    message_id: String,
    request_id: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation_1h_input_tokens: u64,
    cache_read_input_tokens: u64,
    fast_mode: bool,
    account_uuid: Option<String>,
}

/// Encoded by variant index, so the order is part of the format.
#[derive(Serialize, Deserialize)]
enum LegacyProvider {
    Claude,
    Codex,
    Gemini,
    Pi,
    Amp,
    OpenCode,
    OpenClaw,
    Droid,
    Kimi,
}

impl From<LegacyProvider> for Provider {
    fn from(p: LegacyProvider) -> Self {
        match p {
            LegacyProvider::Claude => Self::Claude,
            LegacyProvider::Codex => Self::Codex,
            LegacyProvider::Gemini => Self::Gemini,
            LegacyProvider::Pi => Self::Pi,
            LegacyProvider::Amp => Self::Amp,
            LegacyProvider::OpenCode => Self::OpenCode,
            LegacyProvider::OpenClaw => Self::OpenClaw,
            LegacyProvider::Droid => Self::Droid,
            LegacyProvider::Kimi => Self::Kimi,
        }
    }
}

impl From<Provider> for LegacyProvider {
    fn from(p: Provider) -> Self {
        match p {
            Provider::Claude => Self::Claude,
            Provider::Codex => Self::Codex,
            Provider::Gemini => Self::Gemini,
            Provider::Pi => Self::Pi,
            Provider::Amp => Self::Amp,
            Provider::OpenCode => Self::OpenCode,
            Provider::OpenClaw => Self::OpenClaw,
            Provider::Droid => Self::Droid,
            Provider::Kimi => Self::Kimi,
        }
    }
}

/// Decode a legacy payload, flagging its `tku-agg:` records as aggregates.
pub(crate) fn decode(data: &[u8]) -> Option<ProviderCache> {
    let files = decode_raw(data)?
        .files
        .into_iter()
        .map(|(key, f)| (key, f.into_cached()))
        .collect();
    Some(ProviderCache {
        files,
        legacy: true,
        ..ProviderCache::default()
    })
}

/// Encode `pc` in the legacy shape, files sorted by key. `sealed` is dropped.
pub(crate) fn encode(pc: &ProviderCache) -> Result<Vec<u8>, bitcode::Error> {
    let mut keys: Vec<&String> = pc.files.keys().collect();
    keys.sort_unstable();
    let cache = LegacyCache {
        files: keys
            .into_iter()
            .map(|k| (k.clone(), LegacyFile::from_cached(&pc.files[k])))
            .collect(),
    };
    encode_raw(&cache)
}

fn decode_raw(data: &[u8]) -> Option<LegacyCache> {
    let (version, cache): (u32, LegacyCache) = bitcode::deserialize(data).ok()?;
    (version == VERSION).then_some(cache)
}

fn encode_raw(cache: &LegacyCache) -> Result<Vec<u8>, bitcode::Error> {
    bitcode::serialize(&(VERSION, cache))
}

impl LegacyFile {
    fn into_cached(self) -> CachedFile {
        CachedFile {
            mtime_secs: self.mtime_secs,
            size: self.size,
            records: self
                .records
                .into_iter()
                .map(LegacyRecord::into_record)
                .collect(),
            sealed: Vec::new(),
        }
    }

    fn from_cached(cf: &CachedFile) -> Self {
        Self {
            mtime_secs: cf.mtime_secs,
            size: cf.size,
            records: cf.records.iter().map(LegacyRecord::from_record).collect(),
        }
    }
}

impl LegacyRecord {
    fn into_record(self) -> UsageRecord {
        let mut r = UsageRecord {
            provider: self.provider.into(),
            session_id: self.session_id,
            timestamp: self.timestamp,
            project: self.project,
            model: self.model,
            message_id: self.message_id,
            request_id: self.request_id,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_creation_1h_input_tokens: self.cache_creation_1h_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            fast_mode: self.fast_mode,
            account_uuid: self.account_uuid,
            aggregate: false,
        };
        types::adopt_legacy_aggregate(&mut r);
        r
    }

    fn from_record(r: &UsageRecord) -> Self {
        let (message_id, request_id) = if r.aggregate {
            (legacy_aggregate_id(r), AGG_MARKER.to_string())
        } else {
            (r.message_id.clone(), r.request_id.clone())
        };
        Self {
            provider: r.provider.into(),
            session_id: r.session_id.clone(),
            timestamp: r.timestamp,
            project: r.project.clone(),
            model: r.model.clone(),
            message_id,
            request_id,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_creation_input_tokens: r.cache_creation_input_tokens,
            cache_creation_1h_input_tokens: r.cache_creation_1h_input_tokens,
            cache_read_input_tokens: r.cache_read_input_tokens,
            fast_mode: r.fast_mode,
            account_uuid: r.account_uuid.clone(),
        }
    }
}

/// The legacy aggregate message id: `tku-agg:{session}:{day}:{model}:{account}:{fast}`.
fn legacy_aggregate_id(r: &UsageRecord) -> String {
    format!(
        "{AGG_MARKER}:{}:{}:{}:{}:{}",
        r.session_id,
        r.timestamp.date_naive(),
        r.model,
        r.account_uuid.as_deref().unwrap_or(""),
        r.fast_mode
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/legacy-claude.bin");

    #[test]
    fn legacy_fixture_reencodes_byte_identically() {
        let raw = decode_raw(FIXTURE).unwrap();
        let rebuilt = LegacyCache {
            files: raw
                .files
                .into_iter()
                .map(|(k, f)| (k, LegacyFile::from_cached(&f.into_cached())))
                .collect(),
        };
        assert_eq!(encode_raw(&rebuilt).unwrap(), FIXTURE);
    }

    #[test]
    fn legacy_decode_flags_the_prefixed_aggregates() {
        let pc = decode(FIXTURE).unwrap();
        let records: Vec<&UsageRecord> = pc.files.values().flat_map(|cf| &cf.records).collect();
        let aggregates: Vec<&&UsageRecord> = records.iter().filter(|r| r.aggregate).collect();
        assert_eq!(aggregates.len(), 4);
        for r in aggregates {
            assert_eq!(r.message_id, types::aggregate_id(r));
            assert_eq!(r.request_id, AGG_MARKER);
        }
        assert!(records
            .iter()
            .all(|r| r.aggregate || !r.message_id.starts_with("tku-agg")));
    }

    #[test]
    fn legacy_encode_decode_keeps_content_and_drops_sealed() {
        let mut pc = decode(FIXTURE).unwrap();
        for cf in pc.files.values_mut() {
            cf.sealed = vec![1, 2, 3];
        }
        let data = encode(&pc).unwrap();
        assert_eq!(encode(&pc).unwrap(), data);
        let back = decode(&data).unwrap();
        assert!(back.files.values().all(|cf| cf.sealed.is_empty()));
        for cf in pc.files.values_mut() {
            cf.sealed.clear();
        }
        assert_eq!(back.snapshot(), pc.snapshot());
    }

    #[test]
    fn legacy_decode_rejects_another_marker() {
        let data = bitcode::serialize(&(VERSION + 1, LegacyCache { files: Vec::new() })).unwrap();
        assert!(decode(&data).is_none());
        assert!(decode(b"garbage").is_none());
    }
}
