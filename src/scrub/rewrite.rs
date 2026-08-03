//! Byte-level rewrite with semantic verification.
//!
//! Nothing is ever re-serialized: `serde_json` without `preserve_order`
//! reorders object keys, which would turn a three-byte redaction into a
//! whole-file diff. Instead the raw bytes are spliced and the result is checked
//! against the original *as parsed trees* — comparing raw bytes only would miss
//! both over-redaction and `\uXXXX`-escaped survivors.

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{bail, Result};
use serde_json::Value;

use crate::scrub::detect::Detector;
use crate::scrub::redact;

pub struct Secret {
    pub value: String,
    pub detector: &'static Detector,
    pub fp: String,
}

/// Every on-disk spelling of `value`. Inside a `.jsonl` line a PEM body's
/// newlines are the two bytes `\` `n`, so the escaped form is the one that
/// actually appears.
pub fn encoded_forms(value: &str) -> Vec<String> {
    let mut forms = vec![value.to_string()];
    if let Ok(quoted) = serde_json::to_string(value) {
        let escaped = quoted[1..quoted.len() - 1].to_string();
        if escaped != value {
            forms.push(escaped);
        }
    }
    forms
}

/// One automaton over every encoded form of every secret, built once and reused
/// across the whole corpus.
pub struct Matcher {
    ac: AhoCorasick,
    replacements: Vec<String>,
    values: Vec<String>,
}

impl Matcher {
    pub fn new(secrets: &[Secret]) -> Result<Self> {
        let mut patterns = Vec::new();
        let mut replacements = Vec::new();
        let mut values = Vec::new();

        for s in secrets {
            let with = redact::mask(s.detector, &s.value, &s.fp);
            for form in encoded_forms(&s.value) {
                if form.is_empty() {
                    continue;
                }
                patterns.push(form);
                replacements.push(with.clone());
            }
            values.push(s.value.clone());
        }

        // Leftmost-longest so a JWT nested inside a Bearer header is replaced
        // once, by the longer pattern.
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)?;

        Ok(Self {
            ac,
            replacements,
            values,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn has_match(&self, text: &str) -> bool {
        self.ac.find(text).is_some()
    }

    pub fn replace(&self, text: &str) -> String {
        self.ac.replace_all(text, &self.replacements)
    }

    /// Any secret value present in `text` in any encoded form.
    fn survivors(&self, text: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|v| encoded_forms(v).iter().any(|f| text.contains(f.as_str())))
            .map(|v| v.as_str())
    }
}

/// Walk two parsed trees in lockstep. Every difference must be explainable as a
/// redaction of a string leaf; anything else — a changed object key, a changed
/// number, a lost array element — is a corrupted rewrite.
///
/// One blind spot worth knowing about: `serde_json` is built without
/// `preserve_order`, so a duplicate object key collapses to its last occurrence
/// on both sides and the earlier span is invisible here. The byte splice still
/// redacts it correctly; it simply isn't covered by this second check.
fn verify_tree(before: &Value, after: &Value, m: &Matcher) -> Result<()> {
    match (before, after) {
        (Value::String(b), Value::String(a)) => {
            if b == a {
                return Ok(());
            }
            if m.replace(b) != *a {
                bail!("string leaf changed by something other than a redaction");
            }
            Ok(())
        }
        (Value::Array(b), Value::Array(a)) => {
            if b.len() != a.len() {
                bail!("array length changed: {} -> {}", b.len(), a.len());
            }
            for (x, y) in b.iter().zip(a.iter()) {
                verify_tree(x, y, m)?;
            }
            Ok(())
        }
        (Value::Object(b), Value::Object(a)) => {
            if b.len() != a.len() {
                bail!("object size changed: {} -> {}", b.len(), a.len());
            }
            for ((bk, bv), (ak, av)) in b.iter().zip(a.iter()) {
                if bk != ak {
                    bail!("object key was rewritten: {bk:?} -> {ak:?}");
                }
                verify_tree(bv, av, m)?;
            }
            Ok(())
        }
        (b, a) if b == a => Ok(()),
        _ => bail!("value type or content changed outside a string leaf"),
    }
}

/// Assert no secret survives in any *decoded* string leaf. Checking raw bytes
/// would miss `{"k":"AKIA…"}`, which is valid JSON carrying the secret.
fn verify_absence(v: &Value, m: &Matcher) -> Result<()> {
    match v {
        Value::String(s) => {
            if let Some(found) = m.survivors(s) {
                bail!("secret survived the rewrite (len {})", found.len());
            }
            Ok(())
        }
        Value::Array(a) => a.iter().try_for_each(|x| verify_absence(x, m)),
        Value::Object(o) => o.values().try_for_each(|x| verify_absence(x, m)),
        _ => Ok(()),
    }
}

/// Rewrite `input`, returning `None` when nothing matched.
///
/// On `Err` the caller must leave the file untouched.
pub fn rewrite_text(input: &str, m: &Matcher, jsonl: bool) -> Result<Option<String>> {
    if m.is_empty() || !m.has_match(input) {
        return Ok(None);
    }

    let out = m.replace(input);
    if out == input {
        return Ok(None);
    }

    if jsonl {
        if out.lines().count() != input.lines().count() {
            bail!(
                "line count changed: {} -> {}",
                input.lines().count(),
                out.lines().count()
            );
        }
        for (i, (before, after)) in input.lines().zip(out.lines()).enumerate() {
            let Ok(bv) = serde_json::from_str::<Value>(before) else {
                // Line was not JSON to begin with; raw absence is all we can assert.
                if let Some(found) = m.survivors(after) {
                    bail!("line {}: secret survived (len {})", i + 1, found.len());
                }
                continue;
            };
            let av = serde_json::from_str::<Value>(after)
                .map_err(|e| anyhow::anyhow!("line {} no longer parses as JSON: {e}", i + 1))?;
            verify_tree(&bv, &av, m).map_err(|e| anyhow::anyhow!("line {}: {e}", i + 1))?;
            verify_absence(&av, m).map_err(|e| anyhow::anyhow!("line {}: {e}", i + 1))?;
        }
    } else {
        // Plain text: bytes outside the replaced spans are unchanged by
        // construction, so absence is the meaningful assertion. If the file
        // happened to be JSON, hold it to the same structural bar.
        if let Some(found) = m.survivors(&out) {
            bail!("secret survived the rewrite (len {})", found.len());
        }
        if let Ok(bv) = serde_json::from_str::<Value>(input) {
            let av = serde_json::from_str::<Value>(&out)
                .map_err(|e| anyhow::anyhow!("file no longer parses as JSON: {e}"))?;
            verify_tree(&bv, &av, m)?;
        }
    }

    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrub::detect::DETECTORS;

    fn det(id: &str) -> &'static Detector {
        DETECTORS.iter().find(|d| d.id == id).unwrap()
    }

    fn matcher(pairs: &[(&str, &str, &str)]) -> Matcher {
        let secrets: Vec<Secret> = pairs
            .iter()
            .map(|(id, v, fp)| Secret {
                value: (*v).to_string(),
                detector: det(id),
                fp: (*fp).to_string(),
            })
            .collect();
        Matcher::new(&secrets).unwrap()
    }

    const KEY: &str = "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c";
    const FP: &str = "a3f91c04";

    fn key_matcher() -> Matcher {
        matcher(&[("openrouter-key", KEY, FP)])
    }

    #[test]
    fn returns_none_when_nothing_matches() {
        assert!(rewrite_text("nothing to see", &key_matcher(), true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn replaces_every_occurrence_across_paths() {
        let line = format!(r#"{{"a":"export K={KEY}","b":"Bearer {KEY}","c":"unrelated"}}"#);
        let out = rewrite_text(&line, &key_matcher(), true).unwrap().unwrap();
        assert_eq!(out.matches(&format!("sk-or-v1-[REDACTED:{FP}]")).count(), 2);
        assert!(!out.contains(KEY));
        assert!(out.contains("unrelated"));
    }

    #[test]
    fn preserves_key_order_and_untouched_bytes() {
        let line = format!(r#"{{"z":1,"a":"{KEY}","m":{{"nested":true}}}}"#);
        let out = rewrite_text(&line, &key_matcher(), true).unwrap().unwrap();
        assert!(out.starts_with(r#"{"z":1,"a":"#));
        assert!(out.ends_with(r#""m":{"nested":true}}"#));
    }

    #[test]
    fn output_still_parses_as_json() {
        let line = format!(r#"{{"cmd":"curl -H \"Authorization: Bearer {KEY}\""}}"#);
        let out = rewrite_text(&line, &key_matcher(), true).unwrap().unwrap();
        serde_json::from_str::<Value>(&out).unwrap();
    }

    #[test]
    fn matches_the_json_escaped_form_of_a_pem_block() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----";
        let line = serde_json::json!({ "file": pem }).to_string();
        assert!(!line.contains(pem));
        let m = matcher(&[("private-key", pem, "1a2b3c4d")]);
        let out = rewrite_text(&line, &m, true).unwrap().unwrap();
        assert!(out.contains("[REDACTED:private-key:1a2b3c4d]"));
        serde_json::from_str::<Value>(&out).unwrap();
    }

    #[test]
    fn a_pem_block_in_plain_text_is_redacted() {
        // Regression: an unconditional line-boundary bail made multi-line
        // secrets in file-history/ and debug/ permanently unredactable.
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----";
        let text = format!("key:\n{pem}\ntrailing\n");
        let m = matcher(&[("private-key", pem, "1a2b3c4d")]);
        let out = rewrite_text(&text, &m, false).unwrap().unwrap();
        assert!(!out.contains("MIIEvQIBADANBg"));
        assert!(out.contains("trailing"));
    }

    #[test]
    fn a_unicode_escaped_survivor_is_rejected() {
        // `\u0041` is 'A': valid JSON that carries the secret past any
        // raw-byte absence check. The file must be refused, not written.
        let secret = concat!("AKIAQN", "PYDW3K2LMNOPQR");
        // Built rather than written out, so the literal never appears in source
        // for a secret scanner to trip over.
        let line = format!(r#"{{"a":"{secret}","b":"\u0041KIAQNPYDW3K2LMNOPQR"}}"#);
        let line = line.as_str();
        assert_eq!(
            serde_json::from_str::<Value>(line).unwrap()["b"]
                .as_str()
                .unwrap(),
            secret
        );
        let m = matcher(&[("aws-access-key-id", secret, "deadbeef")]);
        let err = rewrite_text(line, &m, true).unwrap_err().to_string();
        assert!(err.contains("survived"), "unexpected error: {err}");
    }

    #[test]
    fn a_rewritten_object_key_is_rejected() {
        let secret = concat!("AKIAQN", "PYDW3K2LMNOPQR");
        let line = format!(r#"{{"{secret}":1}}"#);
        let m = matcher(&[("aws-access-key-id", secret, "deadbeef")]);
        let err = rewrite_text(&line, &m, true).unwrap_err().to_string();
        assert!(err.contains("key was rewritten"), "unexpected error: {err}");
    }

    #[test]
    fn line_count_is_preserved() {
        let input = format!("{{\"a\":\"{KEY}\"}}\n{{\"b\":1}}\n{{\"c\":\"{KEY}\"}}\n");
        let out = rewrite_text(&input, &key_matcher(), true).unwrap().unwrap();
        assert_eq!(out.lines().count(), 3);
    }

    #[test]
    fn overlapping_matches_are_applied_once() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmno";
        let line = format!(r#"{{"h":"Authorization: Bearer {jwt}"}}"#);
        let m = matcher(&[("jwt", jwt, "7c02ab19")]);
        let out = rewrite_text(&line, &m, true).unwrap().unwrap();
        assert_eq!(out.matches("[REDACTED:jwt:7c02ab19]").count(), 1);
        serde_json::from_str::<Value>(&out).unwrap();
    }

    #[test]
    fn rewrite_is_idempotent() {
        let line = format!(r#"{{"a":"{KEY}"}}"#);
        let m = key_matcher();
        let once = rewrite_text(&line, &m, true).unwrap().unwrap();
        assert!(rewrite_text(&once, &m, true).unwrap().is_none());
    }

    #[test]
    fn plain_text_targets_are_rewritten() {
        let text = format!("OPENROUTER_API_KEY={KEY}\n");
        let out = rewrite_text(&text, &key_matcher(), false).unwrap().unwrap();
        assert!(out.starts_with(&format!("OPENROUTER_API_KEY=sk-or-v1-[REDACTED:{FP}]")));
    }

    #[test]
    fn a_non_json_line_in_a_jsonl_file_is_still_swept() {
        let input = format!("not json at all {KEY}\n{{\"a\":1}}\n");
        let out = rewrite_text(&input, &key_matcher(), true).unwrap().unwrap();
        assert!(!out.contains(KEY));
        assert_eq!(out.lines().count(), 2);
    }
}
