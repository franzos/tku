//! The native store format: [`MAGIC`], a little-endian u32 [`VERSION`], then a
//! bitcode-encoded [`NativeStore`].

use std::collections::HashMap;

use bitcode::{Decode, Encode};
use chrono::DateTime;

use super::bitcode_store::{CachedFile, ProviderCache};
use crate::types::{self, Provider, UsageRecord, AGG_MARKER};

const MAGIC: &[u8; 8] = b"TKUSTORE";
const VERSION: u32 = 2;
pub(crate) const HEADER_LEN: usize = MAGIC.len() + 4;

const FLAG_FAST: u8 = 1;
const FLAG_AGGREGATE: u8 = 1 << 1;

#[derive(Encode, Decode)]
struct NativeStore {
    /// Project, model and account strings. Index 0 is reserved and never referenced.
    strings: Vec<String>,
    files: Vec<NativeFile>,
}

#[derive(Encode, Decode)]
struct NativeFile {
    key: String,
    mtime_secs: i64,
    size: u64,
    sessions: Vec<String>,
    sealed: Vec<u64>,
    records: Vec<NativeRecord>,
}

#[derive(Encode, Decode)]
struct NativeRecord {
    provider: u8,
    /// Index into the file's `sessions`.
    session: u32,
    ts_ms: i64,
    project: u32,
    model: u32,
    /// 0 is `None`.
    account: u32,
    /// Empty for aggregates, which get theirs back from [`types::aggregate_id`].
    message_id: String,
    request_id: String,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_creation_1h: u64,
    cache_read: u64,
    flags: u8,
}

/// Whether `data` starts with the native header. Anything else non-empty is legacy or garbage.
pub(crate) fn is_native(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

/// Whether `header` starts with the magic and this build's version, so [`decode`] can read the body.
pub(crate) fn is_current(header: &[u8]) -> bool {
    header
        .strip_prefix(MAGIC)
        .and_then(<[u8]>::first_chunk::<4>)
        .is_some_and(|v| u32::from_le_bytes(*v) == VERSION)
}

/// Encode `pc` deterministically: files sorted by key, strings in first-use order.
pub(crate) fn encode(pc: &ProviderCache) -> Vec<u8> {
    let mut keys: Vec<&String> = pc.files.keys().collect();
    keys.sort_unstable();
    let mut strings = Interner::with_reserved_zero();
    let files = keys
        .into_iter()
        .map(|key| encode_file(key, &pc.files[key], &mut strings))
        .collect();
    let payload = bitcode::encode(&NativeStore {
        strings: strings.strings,
        files,
    });
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decode a native payload. `None` for any other header or an invalid body.
pub(crate) fn decode(data: &[u8]) -> Option<ProviderCache> {
    let (version, body) = data.strip_prefix(MAGIC)?.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != VERSION {
        return None;
    }
    let store: NativeStore = bitcode::decode(body).ok()?;
    let mut pc = ProviderCache::default();
    for f in store.files {
        let records = f
            .records
            .into_iter()
            .map(|r| decode_record(r, &store.strings, &f.sessions))
            .collect::<Option<Vec<_>>>()?;
        let cf = CachedFile {
            mtime_secs: f.mtime_secs,
            size: f.size,
            records,
            sealed: f.sealed,
        };
        if pc.files.insert(f.key, cf).is_some() {
            return None;
        }
    }
    Some(pc)
}

fn encode_file<'a>(key: &str, cf: &'a CachedFile, strings: &mut Interner<'a>) -> NativeFile {
    let mut sessions = Interner::default();
    let records = cf
        .records
        .iter()
        .map(|r| {
            let mut flags = 0;
            if r.fast_mode {
                flags |= FLAG_FAST;
            }
            let (message_id, request_id) = if r.aggregate {
                flags |= FLAG_AGGREGATE;
                (String::new(), String::new())
            } else {
                (r.message_id.clone(), r.request_id.clone())
            };
            NativeRecord {
                provider: provider_code(r.provider),
                session: sessions.intern(&r.session_id),
                ts_ms: r.timestamp.timestamp_millis(),
                project: strings.intern(&r.project),
                model: strings.intern(&r.model),
                account: r.account_uuid.as_deref().map_or(0, |a| strings.intern(a)),
                message_id,
                request_id,
                input: r.input_tokens,
                output: r.output_tokens,
                cache_creation: r.cache_creation_input_tokens,
                cache_creation_1h: r.cache_creation_1h_input_tokens,
                cache_read: r.cache_read_input_tokens,
                flags,
            }
        })
        .collect();
    NativeFile {
        key: key.to_string(),
        mtime_secs: cf.mtime_secs,
        size: cf.size,
        sessions: sessions.strings,
        sealed: cf.sealed.clone(),
        records,
    }
}

fn decode_record(r: NativeRecord, strings: &[String], sessions: &[String]) -> Option<UsageRecord> {
    let string = |i: u32| (i != 0).then(|| strings.get(i as usize)).flatten().cloned();
    if r.flags & !(FLAG_FAST | FLAG_AGGREGATE) != 0 {
        return None;
    }
    let mut rec = UsageRecord {
        provider: provider_from_code(r.provider)?,
        session_id: sessions.get(r.session as usize)?.clone(),
        timestamp: DateTime::from_timestamp_millis(r.ts_ms)?,
        project: string(r.project)?,
        model: string(r.model)?,
        message_id: r.message_id,
        request_id: r.request_id,
        input_tokens: r.input,
        output_tokens: r.output,
        cache_creation_input_tokens: r.cache_creation,
        cache_creation_1h_input_tokens: r.cache_creation_1h,
        cache_read_input_tokens: r.cache_read,
        fast_mode: r.flags & FLAG_FAST != 0,
        account_uuid: match r.account {
            0 => None,
            i => Some(string(i)?),
        },
        aggregate: r.flags & FLAG_AGGREGATE != 0,
    };
    if rec.aggregate {
        rec.message_id = types::aggregate_id(&rec);
        rec.request_id = AGG_MARKER.to_string();
    }
    Some(rec)
}

/// Frozen wire codes; a new provider takes the next unused one.
fn provider_code(p: Provider) -> u8 {
    match p {
        Provider::Claude => 0,
        Provider::Codex => 1,
        Provider::Gemini => 2,
        Provider::Pi => 3,
        Provider::Amp => 4,
        Provider::OpenCode => 5,
        Provider::OpenClaw => 6,
        Provider::Droid => 7,
        Provider::Kimi => 8,
    }
}

fn provider_from_code(code: u8) -> Option<Provider> {
    Some(match code {
        0 => Provider::Claude,
        1 => Provider::Codex,
        2 => Provider::Gemini,
        3 => Provider::Pi,
        4 => Provider::Amp,
        5 => Provider::OpenCode,
        6 => Provider::OpenClaw,
        7 => Provider::Droid,
        8 => Provider::Kimi,
        _ => return None,
    })
}

#[derive(Default)]
struct Interner<'a> {
    strings: Vec<String>,
    index: HashMap<&'a str, u32>,
}

impl<'a> Interner<'a> {
    fn with_reserved_zero() -> Self {
        Self {
            strings: vec![String::new()],
            index: HashMap::new(),
        }
    }

    fn intern(&mut self, s: &'a str) -> u32 {
        let next = &mut self.strings;
        *self.index.entry(s).or_insert_with(|| {
            // A store with 2^32 distinct strings could not be held in memory.
            let i = u32::try_from(next.len()).expect("string table index fits in u32");
            next.push(s.to_string());
            i
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/legacy-claude.bin");
    const NATIVE_FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native-claude.bin"
    );

    #[allow(clippy::too_many_arguments)]
    fn rec(
        provider: Provider,
        session: &str,
        ts: &str,
        project: &str,
        msg: &str,
        out: u64,
        account: Option<&str>,
        fast: bool,
    ) -> UsageRecord {
        UsageRecord {
            provider,
            session_id: session.into(),
            timestamp: ts.parse().unwrap(),
            project: project.into(),
            model: "claude-sonnet-4-5".into(),
            message_id: msg.into(),
            request_id: format!("req-{msg}"),
            input_tokens: 11,
            output_tokens: out,
            cache_creation_input_tokens: 300,
            cache_creation_1h_input_tokens: 100,
            cache_read_input_tokens: 9000,
            fast_mode: fast,
            account_uuid: account.map(str::to_string),
            aggregate: false,
        }
    }

    fn aggregate(mut r: UsageRecord) -> UsageRecord {
        r.aggregate = true;
        r.message_id = types::aggregate_id(&r);
        r.request_id = AGG_MARKER.to_string();
        r
    }

    fn cached(records: Vec<UsageRecord>, sealed: Vec<u64>) -> CachedFile {
        CachedFile {
            mtime_secs: 1_790_000_000,
            size: 512 * records.len() as u64,
            records,
            sealed,
        }
    }

    fn sample() -> ProviderCache {
        let mut pc = ProviderCache::default();
        pc.files.insert(
            "/b.jsonl".into(),
            cached(
                vec![
                    rec(
                        Provider::Claude,
                        "s1",
                        "2026-10-01T09:00:00.123Z",
                        "app",
                        "m1",
                        6,
                        Some("acct"),
                        false,
                    ),
                    rec(
                        Provider::Claude,
                        "s1",
                        "2026-10-01T09:00:01.999Z",
                        "",
                        "m2",
                        410,
                        None,
                        true,
                    ),
                    rec(
                        Provider::Claude,
                        "s1",
                        "2026-10-01T09:00:02Z",
                        "a:b",
                        "",
                        1,
                        Some("acct"),
                        false,
                    ),
                ],
                Vec::new(),
            ),
        );
        pc.files.insert(
            "/a.jsonl".into(),
            cached(
                vec![
                    aggregate(rec(
                        Provider::Claude,
                        "s0",
                        "2025-01-10T08:00:00Z",
                        "app",
                        "",
                        300,
                        Some("acct"),
                        false,
                    )),
                    aggregate(rec(
                        Provider::Claude,
                        "s0",
                        "2025-01-11T08:00:00Z",
                        "app",
                        "",
                        30,
                        None,
                        true,
                    )),
                ],
                vec![3, 17, 99],
            ),
        );
        pc.files.insert(
            "/opencode.db".into(),
            cached(
                vec![
                    rec(
                        Provider::OpenCode,
                        "ses-1",
                        "2026-10-02T10:00:00.001Z",
                        "web",
                        "o1",
                        5,
                        None,
                        false,
                    ),
                    rec(
                        Provider::OpenCode,
                        "ses-2",
                        "2026-10-02T11:00:00.002Z",
                        "web",
                        "o2",
                        6,
                        None,
                        false,
                    ),
                    rec(
                        Provider::OpenCode,
                        "ses-1",
                        "2026-10-02T12:00:00.003Z",
                        "api",
                        "o3",
                        7,
                        None,
                        false,
                    ),
                    rec(
                        Provider::OpenCode,
                        "ses-3",
                        "2026-10-02T13:00:00.004Z",
                        "web",
                        "o4",
                        8,
                        None,
                        false,
                    ),
                ],
                Vec::new(),
            ),
        );
        pc.files
            .insert("/empty.jsonl".into(), cached(Vec::new(), Vec::new()));
        pc
    }

    fn raw(data: &[u8]) -> NativeStore {
        bitcode::decode(&data[HEADER_LEN..]).unwrap()
    }

    fn with_header(store: &NativeStore) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&bitcode::encode(store));
        out
    }

    #[test]
    fn native_round_trip_is_lossless_and_byte_stable() {
        let pc = sample();
        let data = encode(&pc);
        assert!(is_native(&data));
        let back = decode(&data).unwrap();
        assert_eq!(back.snapshot(), pc.snapshot());
        assert_eq!(encode(&back), data);
        assert_eq!(encode(&sample()), data);
    }

    #[test]
    fn native_files_are_sorted_by_key() {
        let keys: Vec<String> = raw(&encode(&sample()))
            .files
            .into_iter()
            .map(|f| f.key)
            .collect();
        assert_eq!(
            keys,
            ["/a.jsonl", "/b.jsonl", "/empty.jsonl", "/opencode.db"]
        );
    }

    #[test]
    fn native_file_with_several_sessions_round_trips() {
        let pc = sample();
        let store = raw(&encode(&pc));
        let f = store
            .files
            .iter()
            .find(|f| f.key == "/opencode.db")
            .unwrap();
        assert_eq!(f.sessions, ["ses-1", "ses-2", "ses-3"]);
        let sessions: Vec<u32> = f.records.iter().map(|r| r.session).collect();
        assert_eq!(sessions, [0, 1, 0, 2]);
        let back = decode(&encode(&pc)).unwrap();
        assert_eq!(
            back.files["/opencode.db"].records,
            pc.files["/opencode.db"].records
        );
    }

    #[test]
    fn native_string_index_zero_is_reserved() {
        let data = encode(&sample());
        let mut store = raw(&data);
        assert_eq!(store.strings[0], "");
        assert!(store.strings[1..].contains(&String::new()));
        for r in store.files.iter().flat_map(|f| &f.records) {
            assert_ne!(r.project, 0);
            assert_ne!(r.model, 0);
        }
        store.files[0].records[0].project = 0;
        assert!(decode(&with_header(&store)).is_none());
    }

    #[test]
    fn native_aggregates_store_no_ids() {
        let store = raw(&encode(&sample()));
        let a = store.files.iter().find(|f| f.key == "/a.jsonl").unwrap();
        assert!(a.records.iter().all(|r| r.flags & FLAG_AGGREGATE != 0
            && r.message_id.is_empty()
            && r.request_id.is_empty()));
    }

    #[test]
    fn native_decode_rejects_invalid_payloads() {
        assert!(!is_native(LEGACY_FIXTURE));
        assert!(decode(LEGACY_FIXTURE).is_none());
        assert!(decode(b"TKUSTOR").is_none());

        let data = encode(&sample());
        let mut other_version = data.clone();
        other_version[MAGIC.len()] = 3;
        assert!(is_native(&other_version));
        assert!(decode(&other_version).is_none());
        assert!(decode(&data[..data.len() - 1]).is_none());

        let mut store = raw(&data);
        store.files[0].records[0].provider = 9;
        assert!(decode(&with_header(&store)).is_none());

        let mut store = raw(&data);
        store.files[0].records[0].flags |= 1 << 2;
        assert!(decode(&with_header(&store)).is_none());

        let mut store = raw(&data);
        store.files[0].records[0].session = 5;
        assert!(decode(&with_header(&store)).is_none());

        let mut store = raw(&data);
        store.files[1].key = store.files[0].key.clone();
        assert!(decode(&with_header(&store)).is_none());
    }

    #[test]
    fn native_provider_codes_round_trip() {
        for p in Provider::iter() {
            assert_eq!(provider_from_code(provider_code(p)), Some(p));
        }
    }

    /// The store pinned by `tests/fixtures/native-claude.bin`: the legacy
    /// fixture's content, with sealed fingerprints on its rolled-up files.
    fn native_fixture_cache() -> ProviderCache {
        let mut pc = super::super::legacy::decode(LEGACY_FIXTURE).unwrap();
        for cf in pc.files.values_mut() {
            if cf.records.iter().any(|r| r.aggregate) {
                cf.sealed = vec![0x0123_4567_89ab_cdef, u64::MAX];
            }
        }
        pc
    }

    /// Writes the fixture once; regenerating it is never part of a normal run.
    #[test]
    #[ignore]
    fn generate_native_fixture() {
        std::fs::write(NATIVE_FIXTURE_PATH, encode(&native_fixture_cache())).unwrap();
    }

    #[test]
    fn native_fixture_decodes_and_reencodes_identically() {
        let fixture = include_bytes!("../../tests/fixtures/native-claude.bin");
        let pc = decode(fixture).unwrap();
        assert_eq!(pc.snapshot(), native_fixture_cache().snapshot());
        assert_eq!(encode(&pc), fixture);

        let records: Vec<&UsageRecord> = pc.files.values().flat_map(|cf| &cf.records).collect();
        assert_eq!(records.len(), 13);
        assert_eq!(records.iter().filter(|r| r.aggregate).count(), 4);
        assert_eq!(
            pc.files.values().filter(|cf| !cf.sealed.is_empty()).count(),
            2
        );
        assert!(records
            .iter()
            .any(|r| r.timestamp.to_rfc3339() == "2026-10-01T09:00:02.789+00:00"));
    }

    /// Decode timing on a synthetic 650k-record store:
    /// `cargo test --release decode_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn decode_timing_650k() {
        use std::time::Instant;

        let models = ["claude-opus-4-1", "claude-sonnet-4-5", "claude-haiku-4-5"];
        let mut pc = ProviderCache::default();
        for f in 0..6_500u32 {
            let session = format!("{f:08x}-7b4d-4e8a-9c21-5d6e7f8a9b0c");
            let records = (0..100u32)
                .map(|i| UsageRecord {
                    provider: Provider::Claude,
                    session_id: session.clone(),
                    timestamp: DateTime::from_timestamp_millis(
                        1_760_000_000_000 + i64::from(f) * 600_000 + i64::from(i) * 1_234,
                    )
                    .unwrap(),
                    project: format!("project-{}", f % 40),
                    model: models[(i % 3) as usize].into(),
                    message_id: format!("msg_01{f:08}{i:014}"),
                    request_id: format!("req_011{f:08}{i:013}"),
                    input_tokens: u64::from(i),
                    output_tokens: u64::from(i * 7),
                    cache_creation_input_tokens: 1_200,
                    cache_creation_1h_input_tokens: 0,
                    cache_read_input_tokens: 18_000 + u64::from(i),
                    fast_mode: i % 50 == 0,
                    account_uuid: (f % 3 != 0).then(|| format!("acct-{}", f % 2)),
                    aggregate: false,
                })
                .collect();
            pc.files.insert(
                format!("/home/user/.claude/projects/-home-user-git-app/{session}.jsonl"),
                cached(records, Vec::new()),
            );
        }
        let legacy = super::super::legacy::encode(&pc).unwrap();
        let native = encode(&pc);

        fn best<T>(f: impl Fn() -> T) -> std::time::Duration {
            (0..3)
                .map(|_| {
                    let t = Instant::now();
                    let decoded = f();
                    let elapsed = t.elapsed();
                    drop(decoded);
                    elapsed
                })
                .min()
                .unwrap()
        }
        let legacy_t = best(|| super::super::legacy::decode(&legacy).unwrap());
        let native_t = best(|| decode(&native).unwrap());
        let native_raw_t = best(|| raw(&native));
        assert_eq!(
            decode(&native).unwrap().snapshot(),
            super::super::legacy::decode(&legacy).unwrap().snapshot()
        );
        println!(
            "legacy: {} bytes, decode {legacy_t:?}; native: {} bytes, decode {native_t:?} \
             ({native_raw_t:?} before conversion to UsageRecord)",
            legacy.len(),
            native.len()
        );
    }
}
