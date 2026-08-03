//! Detector registry and the two confidence gates.
//!
//! Gate choice is load-bearing. On the reference corpus the contextual
//! `KEY=VALUE` pattern produced 33,009 raw candidates, of which 36 were
//! credential-shaped. The difference is `CredentialShape`, not entropy —
//! source code is high-entropy too.

use std::collections::HashSet;

use aho_corasick::AhoCorasick;
use anyhow::{bail, Context, Result};
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Secrets,
    Pii,
    Ip,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Secrets => "secrets",
            Category::Pii => "pii",
            Category::Ip => "ip",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A vendor-identifying prefix carries the confidence.
    Prefixed,
    /// The captured value must look like an opaque credential, not code.
    CredentialShape,
    /// Looser than `CredentialShape`, for values whose *position* already
    /// carries the confidence: a URL's userinfo field. Real passwords are often
    /// too short for the credential gate, but a bare lowercase word is a
    /// dev-compose placeholder, not a secret.
    PasswordLike,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskStyle {
    /// Keep the first N bytes so the class stays readable in the transcript.
    KeepPrefix(usize),
    Whole,
}

pub struct Detector {
    pub id: &'static str,
    pub category: Category,
    pub default_on: bool,
    pub pattern: &'static str,
    /// Substrings, at least one of which every match must contain. Used as a
    /// SIMD prefilter so the regex only runs on text that could possibly match.
    /// Empty means "no prefilter, always run".
    pub literals: &'static [&'static str],
    /// Capture group holding the value to redact. 0 means the whole match.
    pub capture: usize,
    pub gate: Gate,
    pub mask: MaskStyle,
}

impl Detector {
    /// Whether a wide file spread should disqualify a match from the sweep.
    ///
    /// A vendor-prefixed token is self-identifying, so appearing in many files
    /// just means the key is used a lot. A contextual match is not: on the
    /// reference corpus the widest were `grant_type…` (69 files), `kSecAttr…`
    /// (20), and `postgres://postgres:postgres@` (222) — boilerplate, every one.
    pub fn spread_limited(&self) -> bool {
        matches!(self.id, "env-assign" | "url-password" | "header-secret")
    }

    /// Whether the prefilter must match this detector's literals regardless of
    /// case. True exactly when the pattern itself is case-insensitive.
    ///
    /// Keeping this per-detector matters: matching every literal
    /// case-insensitively makes short vendor prefixes like Twilio's `AC` fire on
    /// any occurrence of "ac", which drags the regex across the whole corpus and
    /// makes a perfectly good detector look too expensive to ship.
    pub fn case_insensitive(&self) -> bool {
        self.pattern.starts_with("(?i)")
    }
}

/// PEM bodies are matched by header literal only and expanded by
/// [`expand_pem`]: a `[\s\S]{0,20000}` body blows past the regex crate's
/// compiled-size limit, and even at `{0,2000}` it costs ~190x throughput.
const PEM_BEGIN: &str = r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY(?: BLOCK)?-----";
const PEM_MAX_BODY: usize = 8192;

pub static DETECTORS: &[Detector] = &[
    Detector {
        id: "anthropic-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bsk-ant-[A-Za-z0-9_-]{20,}",
        literals: &["sk-ant-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(7),
    },
    // Alnum-only tail plus \b: a looser `sk-[A-Za-z0-9_-]{40,}` matched English
    // prose such as "risk-of-cardiovascular-events" 28 times on the reference corpus.
    Detector {
        id: "openai-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9]{32,}\b",
        literals: &["sk-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "openrouter-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bsk-or-v1-[a-f0-9]{48,}",
        literals: &["sk-or-v1-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(9),
    },
    Detector {
        id: "github-pat",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bgh[pousr]_[A-Za-z0-9]{36,}",
        literals: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "github-fine-grained",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bgithub_pat_[A-Za-z0-9_]{60,}",
        literals: &["github_pat_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(11),
    },
    Detector {
        id: "gitlab-pat",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bglpat-[A-Za-z0-9_-]{20,}",
        literals: &["glpat-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "slack-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bxox[abpsroe]-[A-Za-z0-9-]{10,}",
        literals: &["xox"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "slack-webhook",
        category: Category::Secrets,
        default_on: true,
        pattern: r"https://hooks\.slack\.com/services/[A-Za-z0-9/]{20,}",
        literals: &["hooks.slack.com"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "aws-access-key-id",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b",
        literals: &["AKIA", "ASIA", "ABIA", "ACCA"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "google-api-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bAIza[0-9A-Za-z_-]{35}\b",
        literals: &["AIza"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "google-oauth-secret",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bGOCSPX-[A-Za-z0-9_-]{20,}",
        literals: &["GOCSPX-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(7),
    },
    Detector {
        id: "atlassian-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bATATT3[A-Za-z0-9_=+/-]{100,}",
        literals: &["ATATT3"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "langfuse-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\b(?:pk|sk)-lf-[A-Za-z0-9-]{20,}",
        literals: &["pk-lf-", "sk-lf-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "polar-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bpolar_(?:oat|at|pat)_[A-Za-z0-9_-]{20,}",
        literals: &["polar_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "stripe-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\b[rs]k_(?:live|test)_[A-Za-z0-9]{20,}",
        literals: &["sk_live_", "sk_test_", "rk_live_", "rk_test_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(8),
    },
    Detector {
        id: "npm-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bnpm_[A-Za-z0-9]{36}\b",
        literals: &["npm_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "pypi-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{50,}",
        literals: &["pypi-AgEIcHlwaS5vcmc"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "hf-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bhf_[A-Za-z0-9]{34,}\b",
        literals: &["hf_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "sendgrid-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b",
        literals: &["SG."],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "digitalocean-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bdop_v1_[a-f0-9]{64}\b",
        literals: &["dop_v1_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(7),
    },
    Detector {
        id: "linear-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\blin_api_[A-Za-z0-9]{40,}",
        literals: &["lin_api_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(8),
    },
    Detector {
        id: "age-secret-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\bAGE-SECRET-KEY-1[A-Z0-9]{50,}",
        literals: &["AGE-SECRET-KEY-1"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "jwt",
        category: Category::Secrets,
        default_on: true,
        pattern: r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        literals: &["eyJ"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "private-key",
        category: Category::Secrets,
        default_on: true,
        pattern: PEM_BEGIN,
        literals: &["PRIVATE KEY"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    // Position in a URL's userinfo is itself the confidence signal, so this is
    // Prefixed: a real URL password is often too short for CredentialShape.
    Detector {
        id: "url-password",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"[a-z][a-z0-9+.-]*://[^\s:/@"]{1,64}:([^\s:/@"]{3,128})@"#,
        literals: &["://"],
        capture: 1,
        gate: Gate::PasswordLike,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "env-assign",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"(?i)\b[A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PASSPHRASE|APIKEY|API_KEY|ACCESS_KEY|PRIVATE_KEY|CLIENT_SECRET|CREDENTIAL)[A-Z0-9_]*\s*[=:]\s*["']?([^\s"',}]{20,200})"#,
        literals: &[
            "SECRET",
            "TOKEN",
            "PASSWORD",
            "PASSWD",
            "PASSPHRASE",
            "APIKEY",
            "API_KEY",
            "ACCESS_KEY",
            "PRIVATE_KEY",
            "CLIENT_SECRET",
            "CREDENTIAL",
        ],
        capture: 1,
        gate: Gate::CredentialShape,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "shopify-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bshp(?:at|ss|ca|pa)_[0-9a-fA-F]{32}\b"#,
        literals: &["shpat_", "shpss_", "shpca_", "shppa_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "square-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bsq0(?:atp|csp|idp)-[A-Za-z0-9_-]{22,}"#,
        literals: &["sq0atp-", "sq0csp-", "sq0idp-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(7),
    },
    Detector {
        id: "new-relic-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bNR(?:AK|JS|II|AA|SP)-[A-Za-z0-9]{27}\b"#,
        literals: &["NRAK-", "NRJS-", "NRII-", "NRAA-", "NRSP-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "sentry-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bsntry[su]_[A-Za-z0-9_]{40,}"#,
        literals: &["sntrys_", "sntryu_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(7),
    },
    Detector {
        id: "sentry-dsn",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"https://[0-9a-f]{32}@[A-Za-z0-9.-]*sentry\.io/\d+"#,
        literals: &["sentry.io"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "doppler-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bdp\.(?:pt|st|ct|sa|scim|audit)\.[A-Za-z0-9]{40,}"#,
        literals: &[
            "dp.pt.",
            "dp.st.",
            "dp.ct.",
            "dp.sa.",
            "dp.scim.",
            "dp.audit.",
        ],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "vault-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bhv[sbr]\.[A-Za-z0-9_-]{24,}"#,
        literals: &["hvs.", "hvb.", "hvr."],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "planetscale-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bpscale_(?:tkn|pw|oauth)_[A-Za-z0-9_-]{32,}"#,
        literals: &["pscale_tkn_", "pscale_pw_", "pscale_oauth_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(11),
    },
    Detector {
        id: "fly-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bfm[12][ar]?_[A-Za-z0-9+/=_-]{40,}"#,
        literals: &["fm1_", "fm2_", "fm1r_", "fm1a_", "fm2r_", "fm2a_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "netlify-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bnfp_[A-Za-z0-9]{36,}"#,
        literals: &["nfp_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "supabase-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bsbp_[a-f0-9]{40}\b"#,
        literals: &["sbp_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "docker-pat",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bdckr_pat_[A-Za-z0-9_-]{27,}"#,
        literals: &["dckr_pat_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(9),
    },
    Detector {
        id: "telegram-bot-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\b\d{8,10}:AA[A-Za-z0-9_-]{33}\b"#,
        literals: &[":AA"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "groq-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bgsk_[A-Za-z0-9]{52}\b"#,
        literals: &["gsk_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "replicate-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\br8_[A-Za-z0-9]{37,}"#,
        literals: &["r8_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "perplexity-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bpplx-[A-Za-z0-9]{40,}"#,
        literals: &["pplx-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "xai-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bxai-[A-Za-z0-9]{60,}"#,
        literals: &["xai-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "fireworks-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bfw_[A-Za-z0-9]{24,}"#,
        literals: &["fw_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "notion-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\b(?:ntn_|secret_)[A-Za-z0-9]{40,}"#,
        literals: &["ntn_", "secret_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "airtable-pat",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bpat[A-Za-z0-9]{14}\.[0-9a-f]{64}\b"#,
        literals: &["pat"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "figma-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bfigd_[A-Za-z0-9_-]{40,}"#,
        literals: &["figd_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "grafana-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bgl(?:sa|c)_[A-Za-z0-9]{32,}"#,
        literals: &["glsa_", "glc_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "rubygems-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\brubygems_[a-f0-9]{48}\b"#,
        literals: &["rubygems_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(9),
    },
    Detector {
        id: "crates-io-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bcio[A-Za-z0-9]{32}\b"#,
        literals: &["cio"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "mailgun-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bkey-[0-9a-f]{32}\b"#,
        literals: &["key-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "azure-storage-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"AccountKey=[A-Za-z0-9+/]{86}=="#,
        literals: &["AccountKey="],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "terraform-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\b[A-Za-z0-9]{14}\.atlasv1\.[A-Za-z0-9_-]{60,}"#,
        literals: &["atlasv1."],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "onepassword-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bops_[A-Za-z0-9]{40,}"#,
        literals: &["ops_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "firebase-fcm-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bAAAA[A-Za-z0-9_-]{7}:APA91b[A-Za-z0-9_-]{130,}"#,
        literals: &["APA91b"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    // Position in an auth header is the confidence signal, as with
    // `url-password`: the header name is not a keyword `env-assign` matches, so
    // `Authorization: Bearer <opaque>` was previously invisible.
    Detector {
        id: "header-secret",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"(?i)\b(?:authorization|proxy-authorization|x-api-key|x-auth-token|x-access-token|api-key|apikey)\s*:\s*(?:bearer|token|basic)?\s*["']?([A-Za-z0-9+/=_.~-]{20,})"#,
        literals: &[
            "authorization",
            "x-api-key",
            "x-auth-token",
            "x-access-token",
            "apikey",
            "api-key",
        ],
        capture: 1,
        gate: Gate::CredentialShape,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "twilio-sid",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\b(?:AC|SK|VA|YK|IS|MG|PN)[0-9a-fA-F]{32}\b"#,
        literals: &["AC", "SK", "VA", "YK", "IS", "MG", "PN"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(2),
    },
    Detector {
        id: "dropbox-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bsl\.[A-Za-z0-9_-]{130,}"#,
        literals: &["sl."],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "contentful-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bCFPAT-[A-Za-z0-9_-]{43}\b"#,
        literals: &["CFPAT-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "resend-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bre_[A-Za-z0-9]{24,}"#,
        literals: &["re_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(3),
    },
    Detector {
        id: "sonarqube-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bsq[pau]_[0-9a-f]{40}\b"#,
        literals: &["sqp_", "sqa_", "squ_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "databricks-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bdapi[0-9a-f]{32}\b"#,
        literals: &["dapi"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "pulumi-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bpul-[0-9a-f]{40}\b"#,
        literals: &["pul-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "jetbrains-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bperm:[A-Za-z0-9+/=]{40,}"#,
        literals: &["perm:"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "tailscale-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\btskey-(?:api|auth|client|scim|webhook)-[A-Za-z0-9]{20,}"#,
        literals: &["tskey-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(6),
    },
    Detector {
        id: "buildkite-token",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\bbkua_[a-z0-9]{40}\b"#,
        literals: &["bkua_"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(5),
    },
    Detector {
        id: "launchdarkly-key",
        category: Category::Secrets,
        default_on: true,
        pattern: r#"\b(?:api|sdk|mob)-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"#,
        literals: &["api-", "sdk-", "mob-"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "email",
        category: Category::Pii,
        default_on: false,
        pattern: r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        literals: &["@"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "iban",
        category: Category::Pii,
        default_on: false,
        pattern: r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b",
        literals: &[],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::KeepPrefix(4),
    },
    Detector {
        id: "ipv4",
        category: Category::Ip,
        default_on: false,
        pattern: r"\b(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\b",
        literals: &["."],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
    Detector {
        id: "ipv6",
        category: Category::Ip,
        default_on: false,
        pattern: r"\b(?:[A-Fa-f0-9]{1,4}:){7}[A-Fa-f0-9]{1,4}\b",
        literals: &[":"],
        capture: 0,
        gate: Gate::Prefixed,
        mask: MaskStyle::Whole,
    },
];

/// Documentation placeholders that appear verbatim in tutorials and fixtures.
static KNOWN_PLACEHOLDERS: &[&str] = &[
    concat!("AKIAIO", "SFODNN7EXAMPLE"),
    concat!("ASIAIO", "SFODNN7EXAMPLE"),
    concat!("wJalrX", "UtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
    "AIzaSyDOCAbC123dEf456GhI789jKl01-MnO",
    "4242424242424242",
    // Dev-compose boilerplate, overwhelmingly common in connection strings.
    "postgres",
    "password",
    "changeme",
    "secret",
    "mysql",
    "redis",
    "root",
    "admin",
    "example",
    "test",
];

pub fn is_known_placeholder(value: &str) -> bool {
    KNOWN_PLACEHOLDERS.contains(&value)
}

// --- CredentialShape gate ---

fn entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = *c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

fn char_classes(s: &str) -> usize {
    let lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let upper = s.bytes().any(|b| b.is_ascii_uppercase());
    let digit = s.bytes().any(|b| b.is_ascii_digit());
    let sym = s
        .bytes()
        .any(|b| matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'));
    [lower, upper, digit, sym]
        .into_iter()
        .filter(|x| *x)
        .count()
}

/// `foo.bar.baz` — a member-access expression, never an opaque credential.
fn is_member_access(s: &str) -> bool {
    s.contains('.')
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn is_snake_case(s: &str) -> bool {
    s.contains('_')
        && s.split('_').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// True camelCase: lowercase run, then one or more Capitalised runs, no digits.
/// Deliberately narrow — "all alphanumeric starting with a letter" describes
/// nearly every API token, and rejecting on it made the gate reject real keys.
fn is_camel_case(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphabetic()) {
        return false;
    }
    let bytes = s.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut transitions = 0;
    for w in bytes.windows(2) {
        if w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase() {
            transitions += 1;
        }
    }
    transitions > 0
}

/// `mock-issuer-secret`, `dev-nextauth-key` — hand-written test values.
fn is_word_slug(s: &str) -> bool {
    let segs: Vec<&str> = s.split(['-', '_']).collect();
    segs.len() >= 3
        && segs.iter().all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn is_date_or_number(s: &str) -> bool {
    let numeric = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b',');
    let datey = s.len() >= 10
        && s.as_bytes()[..10].iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    numeric || datey
}

const CODE_MARKERS: &[char] = &[
    ':', '(', ')', '[', ']', '<', '>', '{', '}', ';', '!', '?', '@', '#', '&', '|', '*', '`', '~',
    '%', '^', '"', '\'', ',', '\\', '$',
];

/// A URL userinfo password worth redacting.
///
/// The position says "this is a password", but plenty of connection strings
/// carry the project name or a placeholder there — `postgres://forseti:forseti@`.
/// Sweeping one of those replaces an ordinary English word across the whole
/// corpus, so require something a dictionary word doesn't have: eight or more
/// characters, and at least one that isn't a lowercase letter.
pub fn passes_password_shape(v: &str) -> bool {
    v.len() >= 8
        && v.len() <= 128
        && !v.bytes().all(|b| b.is_ascii_lowercase())
        && !is_known_placeholder(v)
}

/// Calibrated against the reference corpus: 33,009 candidates -> 36 unique.
pub fn passes_credential_shape(v: &str) -> bool {
    if v.len() < 20 || v.len() > 200 {
        return false;
    }
    // Path-like. `/` and `-` are legal in base64url, so this is a prefix test,
    // not a charset test.
    if v.starts_with(['/', '~', '.', '-', '_']) || v.contains("//") {
        return false;
    }
    if !v
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'))
    {
        return false;
    }
    if v.contains(CODE_MARKERS) {
        return false;
    }
    if is_member_access(v) || is_snake_case(v) || is_camel_case(v) || is_word_slug(v) {
        return false;
    }
    if is_date_or_number(v) {
        return false;
    }
    if entropy(v) < 3.5 {
        return false;
    }
    let classes = char_classes(v);
    if classes < 2 {
        return false;
    }
    // Two-class values (lowercase hex, for instance) need length and density.
    if classes == 2 && (v.len() < 24 || entropy(v) < 3.6) {
        return false;
    }
    true
}

/// Object keys that make their string value worth a second look.
///
/// In structured config the label is the key and the value is a bare opaque
/// string — `{"Authorization": "Bearer …"}`, `{"ACME_API_KEY": "…"}`. Scanning
/// leaves alone gives the contextual detectors no text to match, so an MCP
/// server's auth header or a stdio server's `env` block slips straight through.
static KEYED_HINTS: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "passphrase",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "private_key",
    "client_secret",
    "credential",
    "authorization",
    "auth",
];

/// Cheap enough for the hot path: keys are short and almost never hit.
pub fn key_hints_at_credential(key: &str) -> bool {
    static AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    AC.get_or_init(|| {
        AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(KEYED_HINTS)
            .expect("static literals")
    })
    .is_match(key)
}

/// Mask every run in `s` that would itself qualify as a credential.
///
/// Belt-and-braces for preview context only. Detectors miss things — a
/// percent-encoded token, a vendor prefix nobody has written a rule for, a
/// fragment shorter than its detector's minimum — and context exists to show
/// *where* a match sits, not to reproduce the data around it. Runs that read as
/// identifiers (`AWS_SECRET_ACCESS_KEY`) fail the shape gate and stay visible.
pub fn mask_credential_runs(s: &str) -> String {
    static OPAQUE_RUN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    // `=` only as trailing base64 padding: allowing it inside the run would
    // swallow `KEY=value` whole and mask the key name along with the value.
    let rx = OPAQUE_RUN.get_or_init(|| Regex::new(r"[A-Za-z0-9+/_-]{20,}={0,2}").unwrap());

    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in rx.find_iter(s) {
        out.push_str(&s[last..m.start()]);
        if passes_credential_shape(m.as_str()) {
            out.push_str("[…]");
        } else {
            out.push_str(m.as_str());
        }
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

/// Grow a `-----BEGIN … PRIVATE KEY-----` header match out to its END marker.
/// Returns the header alone if no terminator is within [`PEM_MAX_BODY`].
fn expand_pem(haystack: &str, start: usize, header_end: usize) -> usize {
    let window_end = (header_end + PEM_MAX_BODY).min(haystack.len());
    let window = &haystack[header_end..window_end];
    match window.find("PRIVATE KEY") {
        Some(rel) => {
            let after = header_end + rel;
            haystack[after..window_end]
                .find("-----")
                .map(|d| after + d + 5)
                .unwrap_or(header_end)
        }
        None => {
            let _ = start;
            header_end
        }
    }
}

pub struct Hit {
    pub detector: &'static Detector,
    pub value: String,
}

pub struct Engine {
    /// One automaton over every detector's required literals. A merged
    /// `RegexSet` was 3x slower on a real corpus: combining the patterns into a
    /// single automaton discards the per-pattern literal prefilter that makes
    /// each one fast in isolation, so every byte gets inspected by the DFA.
    prefilter: AhoCorasick,
    /// Parallel to `prefilter` pattern ids: which detector each literal belongs to.
    literal_owner: Vec<usize>,
    /// Second automaton for the handful of `(?i)` detectors.
    prefilter_ci: AhoCorasick,
    literal_owner_ci: Vec<usize>,
    /// Detectors with no literal at all; these must always run.
    unfiltered: Vec<usize>,
    regexes: Vec<Regex>,
    dets: Vec<&'static Detector>,
}

impl Engine {
    pub fn new(enabled: &[&'static Detector]) -> Result<Self> {
        if enabled.is_empty() {
            bail!("no detectors enabled");
        }

        let mut literals = Vec::new();
        let mut literal_owner = Vec::new();
        let mut literals_ci = Vec::new();
        let mut literal_owner_ci = Vec::new();
        let mut unfiltered = Vec::new();
        let mut regexes = Vec::with_capacity(enabled.len());

        for (i, det) in enabled.iter().enumerate() {
            regexes.push(Regex::new(det.pattern)?);
            if det.literals.is_empty() {
                unfiltered.push(i);
            }
            let (lits, owners) = if det.case_insensitive() {
                (&mut literals_ci, &mut literal_owner_ci)
            } else {
                (&mut literals, &mut literal_owner)
            };
            for lit in det.literals {
                lits.push(*lit);
                owners.push(i);
            }
        }

        let prefilter = AhoCorasick::new(&literals)?;
        let prefilter_ci = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(&literals_ci)?;

        Ok(Self {
            prefilter,
            literal_owner,
            prefilter_ci,
            literal_owner_ci,
            unfiltered,
            regexes,
            dets: enabled.to_vec(),
        })
    }

    /// Detector indices worth running against `haystack`.
    fn candidates(&self, haystack: &str) -> Vec<usize> {
        let mut hit = vec![false; self.dets.len()];
        for m in self.prefilter.find_overlapping_iter(haystack) {
            hit[self.literal_owner[m.pattern().as_usize()]] = true;
        }
        for m in self.prefilter_ci.find_overlapping_iter(haystack) {
            hit[self.literal_owner_ci[m.pattern().as_usize()]] = true;
        }
        for i in &self.unfiltered {
            hit[*i] = true;
        }
        hit.iter()
            .enumerate()
            .filter_map(|(i, h)| h.then_some(i))
            .collect()
    }

    /// Every detector, ignoring the prefilter. Reference implementation the
    /// prefilter is checked against.
    #[cfg(test)]
    fn scan_unfiltered(&self, haystack: &str, out: &mut Vec<Hit>) {
        for idx in 0..self.dets.len() {
            self.scan_with(idx, haystack, out);
        }
    }

    pub fn scan(&self, haystack: &str, out: &mut Vec<Hit>) {
        for idx in self.candidates(haystack) {
            self.scan_with(idx, haystack, out);
        }
    }

    fn scan_with(&self, idx: usize, haystack: &str, out: &mut Vec<Hit>) {
        let det = self.dets[idx];
        let rx = &self.regexes[idx];

        if det.id == "private-key" {
            for m in rx.find_iter(haystack) {
                let end = expand_pem(haystack, m.start(), m.end());
                out.push(Hit {
                    detector: det,
                    value: haystack[m.start()..end].to_string(),
                });
            }
            return;
        }

        if det.capture == 0 {
            for m in rx.find_iter(haystack) {
                self.push_gated(det, m.as_str(), out);
            }
        } else {
            for caps in rx.captures_iter(haystack) {
                if let Some(m) = caps.get(det.capture) {
                    self.push_gated(det, m.as_str(), out);
                }
            }
        }
    }

    fn push_gated(&self, det: &'static Detector, value: &str, out: &mut Vec<Hit>) {
        if is_known_placeholder(value) {
            return;
        }
        let gated = match det.gate {
            Gate::Prefixed => true,
            Gate::CredentialShape => passes_credential_shape(value),
            Gate::PasswordLike => passes_password_shape(value),
        };
        if !gated {
            return;
        }
        out.push(Hit {
            detector: det,
            value: value.to_string(),
        });
    }
}

/// Turn `scrub.toml`'s `[[custom]]` entries into detectors.
///
/// The results are leaked deliberately. Detectors are built once at startup and
/// referenced for the life of the process — including from `Hit` and `Secret`,
/// which are `&'static` throughout — so a bounded, one-time leak of a handful of
/// entries buys a uniform type everywhere instead of threading a lifetime or an
/// `Arc` through the whole module.
pub fn build_custom(
    custom: &[crate::scrub::state::CustomDetector],
) -> Result<Vec<&'static Detector>> {
    let mut out = Vec::with_capacity(custom.len());

    for c in custom {
        if c.id.trim().is_empty() {
            bail!("a [[custom]] entry has an empty id");
        }
        if DETECTORS.iter().any(|d| d.id == c.id) {
            bail!(
                "custom detector `{}` shadows a built-in one; pick another id",
                c.id
            );
        }
        if out.iter().any(|d: &&Detector| d.id == c.id) {
            bail!("custom detector `{}` is defined twice", c.id);
        }
        let rx = Regex::new(&c.pattern)
            .with_context(|| format!("custom detector `{}` has an invalid pattern", c.id))?;
        if c.capture > rx.captures_len().saturating_sub(1) && c.capture != 0 {
            bail!(
                "custom detector `{}` asks for capture group {} but the pattern has {}",
                c.id,
                c.capture,
                rx.captures_len().saturating_sub(1)
            );
        }
        if c.literals.is_empty() {
            eprintln!(
                "note: custom detector `{}` has no `literals`, so it runs against every string scanned; \
                 add the fixed prefix of the key format to keep scans fast",
                c.id
            );
        }

        let literals: Vec<&'static str> = c
            .literals
            .iter()
            .map(|l| &*Box::leak(l.clone().into_boxed_str()))
            .collect();

        out.push(&*Box::leak(Box::new(Detector {
            id: Box::leak(c.id.clone().into_boxed_str()),
            category: Category::Secrets,
            default_on: true,
            pattern: Box::leak(c.pattern.clone().into_boxed_str()),
            literals: Box::leak(literals.into_boxed_slice()),
            capture: c.capture,
            gate: if c.shape {
                Gate::CredentialShape
            } else {
                Gate::Prefixed
            },
            mask: match c.mask_prefix {
                Some(n) => MaskStyle::KeepPrefix(n),
                None => MaskStyle::Whole,
            },
        })));
    }

    Ok(out)
}

/// Resolve `--only` / `--skip` into the enabled detector set. Both accept
/// category names and detector ids. `extra` carries the user's `[[custom]]`
/// detectors, which participate in selection exactly like built-ins.
pub fn resolve(
    extra: &[&'static Detector],
    only: Option<&[String]>,
    skip: &[String],
) -> Result<Vec<&'static Detector>> {
    let known: HashSet<&str> = DETECTORS
        .iter()
        .map(|d| d.id)
        .chain(extra.iter().map(|d| d.id))
        .chain(["secrets", "pii", "ip"])
        .collect();
    for name in only.unwrap_or(&[]).iter().chain(skip.iter()) {
        if !known.contains(name.as_str()) {
            bail!("unknown detector or category: {name}");
        }
    }

    let matches = |d: &Detector, name: &str| d.id == name || d.category.as_str() == name;

    let selected: Vec<&'static Detector> = DETECTORS
        .iter()
        .chain(extra.iter().copied())
        .filter(|d| match only {
            Some(list) => list.iter().any(|n| matches(d, n)),
            None => d.default_on,
        })
        .filter(|d| !skip.iter().any(|n| matches(d, n)))
        .collect();

    if selected.is_empty() {
        bail!("selection left no detectors enabled");
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_one(id: &str, text: &str) -> Vec<String> {
        let d = DETECTORS.iter().find(|d| d.id == id).unwrap();
        let mut out = Vec::new();
        Engine::new(&[d]).unwrap().scan(text, &mut out);
        out.into_iter().map(|h| h.value).collect()
    }

    #[test]
    fn every_pattern_and_the_default_set_compile() {
        for d in DETECTORS {
            Regex::new(d.pattern).unwrap_or_else(|e| panic!("{}: {e}", d.id));
        }
        // RegexSet is where the compiled-size limit actually bites.
        Engine::new(&resolve(&[], None, &[]).unwrap()).unwrap();
        Engine::new(&DETECTORS.iter().collect::<Vec<_>>()).unwrap();
    }

    #[test]
    fn openai_pattern_does_not_match_english_prose() {
        let prose = "the risk-of-cardiovascular-events-in-patients-with-chronic-kidney-disease";
        assert!(scan_one("openai-key", prose).is_empty());
    }

    #[test]
    fn openai_pattern_matches_a_real_shaped_key() {
        let text = "OPENAI_API_KEY=sk-svcacct-YabcdefghijklmnopqrstuvwxyzABCDEF0123456789";
        assert_eq!(scan_one("openai-key", text).len(), 1);
    }

    #[test]
    fn known_placeholders_are_dropped() {
        assert!(scan_one(
            "aws-access-key-id",
            concat!("id = AKIAIO", "SFODNN7EXAMPLE")
        )
        .is_empty());
        assert_eq!(
            scan_one(
                "aws-access-key-id",
                concat!("id = AKIAQN", "PYDW3K2LMNOPQR")
            )
            .len(),
            1
        );
    }

    #[test]
    fn credential_shape_rejects_member_access_and_paths() {
        assert!(!passes_credential_shape("process.env.OIDC_CLIENT_SECRET"));
        assert!(!passes_credential_shape("config.auth.clientSecret"));
        assert!(!passes_credential_shape(
            "/gnu/store/abcdefghijklmnop-openssl-3.0.8"
        ));
        assert!(!passes_credential_shape("/home/franz/.config/secrets.toml"));
        assert!(!passes_credential_shape("~/.ansible/vault_password_file"));
    }

    #[test]
    fn credential_shape_rejects_source_code_and_identifiers() {
        assert!(!passes_credential_shape("this.extractToken(request)"));
        assert!(!passes_credential_shape("refresh_token_expires_at"));
        assert!(!passes_credential_shape("mock-issuer-secret-value"));
        assert!(!passes_credential_shape("dev-nextauth-secret-here"));
        assert!(!passes_credential_shape("2026-08-02T13:40:00Z"));
    }

    #[test]
    fn credential_shape_accepts_opaque_tokens() {
        assert!(passes_credential_shape("vHGiPOq3Xk9dLm2FwZa7Tn4B"));
        assert!(passes_credential_shape(
            "l8QNPUxKd0RtYbEwCzQ3mHaJ5VgS7fNp2LkTrX9d"
        ));
        assert!(passes_credential_shape("Pe0iF6vHGiPOq3Xk9dLm2FwZa7"));
        // 64 hex: only two character classes, so it must be long and dense.
        assert!(passes_credential_shape(
            "c495fe3a7b18d02e6f4a9c1b83d75e206fa4b9c8d1e7305f2a6b8c4d9e1f70a35"
        ));
    }

    #[test]
    fn env_assign_is_gated_by_shape() {
        assert!(scan_one("env-assign", "let token = this.extractToken(req);").is_empty());
        assert!(scan_one("env-assign", "const s = process.env.OIDC_CLIENT_SECRET;").is_empty());
        assert_eq!(
            scan_one(
                "env-assign",
                "OIDC_CLIENT_SECRET=Pe0iF6vHGiPOq3Xk9dLm2FwZa7"
            )
            .len(),
            1
        );
    }

    #[test]
    fn private_key_block_expands_to_the_end_marker() {
        let line =
            r#"{"k":"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----"}"#;
        let hits = scan_one("private-key", line);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("-----END PRIVATE KEY-----"));
    }

    #[test]
    fn private_key_without_a_terminator_matches_only_the_header() {
        let hits = scan_one("private-key", "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("-----"));
        assert!(!hits[0].contains("MIIEv"));
    }

    #[test]
    fn url_password_is_captured_without_the_userinfo() {
        let hits = scan_one(
            "url-password",
            "postgres://admin:hunter2pass@db.internal:5432/x",
        );
        assert_eq!(hits, vec!["hunter2pass".to_string()]);
    }

    #[test]
    fn credential_runs_are_masked_but_identifiers_survive() {
        let s = "AWS_SECRET_ACCESS_KEY=JO0DBFC29JNAi30cP3MbOjtYnUkcHfQx and AWS_ACCESS_KEY_ID";
        let out = mask_credential_runs(s);
        assert!(!out.contains("JO0DBFC29JNAi30c"), "{out}");
        assert!(out.contains("AWS_SECRET_ACCESS_KEY"), "{out}");
        assert!(out.contains("AWS_ACCESS_KEY_ID"), "{out}");
    }

    #[test]
    fn credential_runs_catch_what_detectors_miss() {
        // Too short for the atlassian-token rule, still not something to print.
        let fragment = "ATATT3xFfGF0T9kzP2mQ7wYbXc4RnJ8aLd5EvHs6UiOp1";
        assert!(mask_credential_runs(fragment).contains("[…]"));
        assert!(!mask_credential_runs(fragment).contains("xFfGF0T9kz"));
    }

    /// One string per detector that its pattern genuinely matches, so the
    /// prefilter is exercised against real positives rather than only misses.
    const POSITIVES: &[&str] = &[
        concat!("ANTHROPI", "C_API_KEY=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"),
        concat!("OPENAI_A", "PI_KEY=sk-svcacct-YabcdefghijklmnopqrstuvwxyzABCDEF0123456789"),
        concat!("sk-or-v1", "-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c1d3e5f7a9b0c2d4"),
        concat!("token ghp_Ab", "CdEfGhIjKlMnOpQrStUvWxYz0123456789"),
        concat!("github", "_pat_11ABCDEFG0abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"),
        concat!("glpat-", "J2ZyAbCdEfGhIjKlMnOp"),
        concat!("xoxb-1", "234567890-abcdefghijkl"),
        concat!("https://", "hooks.slack.com/services/T00000000B00000000XXXXXXXXXXXXXXXXXXXX"),
        concat!("AWS_ACCESS_KEY_ID=AKIAQN", "PYDW3K2LMNOPQR"),
        concat!("AIzaSy", "ByFrAbCdEfGhIjKlMnOpQrStUvWxYz012"),
        concat!("GOCSPX-A", "bCdEfGhIjKlMnOpQrStUv"),
        concat!("ATATT3xF", "fGF0AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMnOpQrStUvWxYz01="),
        concat!("pypi-A", "gEIcHlwaS5vcmcAbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMn"),
        concat!("polar_oa", "t_AbCdEfGhIjKlMnOpQrStUvWx"),
        concat!("lsk_AbCd", "EfGhIjKlMnOpQrStUvWx"),
        concat!("STRIPE=sk_tes", "t_51TGAbCdEfGhIjKlMnOpQrStUv"),
        concat!("npm_Ab", "CdEfGhIjKlMnOpQrStUvWxYz0123456789"),
        concat!("hf_AbC", "dEfGhIjKlMnOpQrStUvWxYz01234567"),
        concat!("SG.abc", "defghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ"),
        concat!("dop_v1", "_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        concat!("lin_ap", "i_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789ABCD"),
        concat!("AGE-SECR", "ET-KEY-1ABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQR"),
        concat!("eyJhbGci", "OiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmno"),
        concat!("-----BEG", "IN PRIVATE KEY-----\nMIIEvQIBADANBg\n-----END PRIVATE KEY-----"),
        concat!("postgres", "://admin:hunter2pass@db.internal:5432/x"),
        concat!("OIDC_CLI", "ENT_SECRET=Pe0iF6vHGiPOq3Xk9dLm2FwZa7"),
        concat!("shpat_01", "23456789abcdef0123456789abcdef"),
        concat!("sq0atp-a", "bcdefghijklmnopqrstuv"),
        concat!("NRAK-abc", "defghijklmnopqrstuvwxyzA"),
        concat!("sntrys_a", "bcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("https://", "0123456789abcdef0123456789abcdef@o123456.ingest.sentry.io/7654321"),
        concat!("dp.pt.ab", "cdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("hvs.abcd", "efghijklmnopqrstuvwx"),
        concat!("pscale_t", "kn_abcdefghijklmnopqrstuvwxyzABCDEF"),
        concat!("fm2_abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("nfp_abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJ"),
        concat!("sbp_0123", "456789abcdef0123456789abcdef01234567"),
        concat!("dckr_pat", "_abcdefghijklmnopqrstuvwxyzA"),
        concat!("12345678", "9:AAabcdefghijklmnopqrstuvwxyzABCDEFG"),
        concat!("gsk_abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"),
        concat!("r8_abcde", "fghijklmnopqrstuvwxyzABCDEFGHIJK"),
        concat!("pplx-abc", "defghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("xai-abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ01234567"),
        concat!("fw_abcde", "fghijklmnopqrstuvwx"),
        concat!("ntn_abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("patabcde", "fghijklmn.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        concat!("figd_abc", "defghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("glsa_abc", "defghijklmnopqrstuvwxyzABCDEF"),
        concat!("rubygems", "_0123456789abcdef0123456789abcdef0123456789abcdef"),
        concat!("cioabcde", "fghijklmnopqrstuvwxyzABCDEF"),
        concat!("key-0123", "456789abcdef0123456789abcdef"),
        concat!("AccountK", "ey=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwx=="),
        concat!("abcdefgh", "ijklmn.atlasv1.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ01234567"),
        concat!("ops_abcd", "efghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("AAAAabcd", "efg:APA91babcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef"),
        concat!("Authoriz", "ation: Bearer Qq9Rr8Ss7Tt6Uu5Vv4Ww3Xx2Yy1Zz0"),
        concat!("AC012345", "6789abcdef0123456789abcdef"),
        concat!("sl.abcde", "fghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdef"),
        concat!("CFPAT-ab", "cdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ"),
        concat!("re_abcde", "fghijklmnopqrstuvwx"),
        concat!("sqp_0123", "456789abcdef0123456789abcdef01234567"),
        concat!("dapi0123", "456789abcdef0123456789abcdef"),
        concat!("pul-0123", "456789abcdef0123456789abcdef01234567"),
        concat!("perm:abc", "defghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
        concat!("tskey-ap", "i-abcdefghijklmnopqrst"),
        concat!("bkua_abc", "defghijklmnopqrstuvwxyz0123456789abcd"),
        concat!("sdk-0123", "4567-0123-0123-0123-0123456789ab"),
        concat!("LANGFUSE", "_SECRET_KEY=sk-lf-1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d"),
        concat!("contact ", "franz@example.com please"),
        concat!("server a", "t 192.168.1.100 responded"),
        concat!("addr fe8", "0:0000:0000:0000:0202:b3ff:fe1e:8329 up"),
        concat!("DE893704", "00440532013000"),
        // Negatives, to catch a prefilter that is too eager as well as one
        // that is too lax.
        concat!("the risk", "-of-cardiovascular-events-in-patients"),
        concat!("let toke", "n = this.extractToken(request);"),
        concat!("const s ", "= process.env.OIDC_CLIENT_SECRET;"),
        concat!("just som", "e ordinary prose with no credentials in it at all"),
    ];

    #[test]
    fn the_literal_prefilter_never_changes_what_is_found() {
        // Verified separately against 10,134 real files; this pins the property
        // so a new detector with missing or wrong `literals` fails here.
        let all: Vec<&'static Detector> = DETECTORS.iter().collect();
        let engine = Engine::new(&all).unwrap();

        for text in POSITIVES {
            let mut filtered = Vec::new();
            let mut unfiltered = Vec::new();
            engine.scan(text, &mut filtered);
            engine.scan_unfiltered(text, &mut unfiltered);

            let f: Vec<_> = filtered.iter().map(|h| (h.detector.id, &h.value)).collect();
            let u: Vec<_> = unfiltered
                .iter()
                .map(|h| (h.detector.id, &h.value))
                .collect();
            assert_eq!(f, u, "prefilter changed the result for {text:?}");
        }
    }

    #[test]
    fn every_detector_has_a_positive_sample_that_matches() {
        for d in DETECTORS {
            let engine = Engine::new(&[d]).unwrap();
            let matched = POSITIVES.iter().any(|t| {
                let mut out = Vec::new();
                engine.scan(t, &mut out);
                !out.is_empty()
            });
            assert!(matched, "{}: no POSITIVES sample matches it", d.id);
        }
    }

    #[test]
    fn a_credential_labelled_by_a_json_key_is_found() {
        // MCP servers in ~/.claude.json put the header name in the key and the
        // token in the value, which a leaf-only scan cannot see.
        let engine = Engine::new(&resolve(&[], None, &[]).unwrap()).unwrap();

        for (key, value) in [
            ("Authorization", "Bearer Qq9Rr8Ss7Tt6Uu5Vv4Ww3Xx2Yy1Zz0"),
            ("ACME_API_KEY", "Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0KkLl1Mm"),
            ("client_secret", "Pe0iF6vHGiPOq3Xk9dLm2FwZa7"),
        ] {
            assert!(key_hints_at_credential(key), "{key} should hint");
            let mut out = Vec::new();
            engine.scan(&format!("{key}: {value}"), &mut out);
            assert!(!out.is_empty(), "{key} carried no detection");
        }

        // An ordinary key must not drag its value in.
        assert!(!key_hints_at_credential("message"));
        assert!(!key_hints_at_credential("type"));
        assert!(!key_hints_at_credential("content"));
    }

    #[test]
    fn resolve_defaults_to_secrets_only() {
        let sel = resolve(&[], None, &[]).unwrap();
        assert!(sel.iter().all(|d| d.category == Category::Secrets));
        assert!(sel.iter().any(|d| d.id == "jwt"));
    }

    #[test]
    fn resolve_honours_only_and_skip() {
        let only = vec!["secrets".to_string()];
        let skip = vec!["jwt".to_string()];
        let sel = resolve(&[], Some(&only), &skip).unwrap();
        assert!(!sel.iter().any(|d| d.id == "jwt"));

        let pii = vec!["pii".to_string()];
        let sel = resolve(&[], Some(&pii), &[]).unwrap();
        assert!(sel.iter().all(|d| d.category == Category::Pii));
    }

    fn custom(id: &str, pattern: &str, literals: &[&str]) -> crate::scrub::state::CustomDetector {
        crate::scrub::state::CustomDetector {
            id: id.to_string(),
            pattern: pattern.to_string(),
            literals: literals.iter().map(|s| s.to_string()).collect(),
            capture: 0,
            mask_prefix: Some(5),
            shape: false,
        }
    }

    #[test]
    fn a_custom_detector_finds_an_in_house_key_format() {
        let c = vec![custom("acme-key", r"\bacme_[A-Za-z0-9]{24,}", &["acme_"])];
        let built = build_custom(&c).unwrap();
        let engine = Engine::new(&built).unwrap();

        let mut out = Vec::new();
        engine.scan("ACME_TOKEN=acme_Xy9kL2mQ8rT4vW7nB3sD6fG1", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].detector.id, "acme-key");
        assert!(out[0].value.starts_with("acme_"));
    }

    #[test]
    fn custom_detectors_participate_in_only_and_skip() {
        let c = vec![custom("acme-key", r"\bacme_[A-Za-z0-9]{24,}", &["acme_"])];
        let built = build_custom(&c).unwrap();

        let sel = resolve(&built, None, &[]).unwrap();
        assert!(sel.iter().any(|d| d.id == "acme-key"));

        let only = vec!["acme-key".to_string()];
        let sel = resolve(&built, Some(&only), &[]).unwrap();
        assert_eq!(sel.len(), 1);

        let skip = vec!["acme-key".to_string()];
        let sel = resolve(&built, None, &skip).unwrap();
        assert!(!sel.iter().any(|d| d.id == "acme-key"));
    }

    #[test]
    fn custom_detectors_are_validated() {
        assert!(build_custom(&[custom("", r"x", &[])]).is_err(), "empty id");
        assert!(
            build_custom(&[custom("jwt", r"x{20,}", &[])]).is_err(),
            "shadowing a built-in"
        );
        assert!(
            build_custom(&[custom("a", r"[unclosed", &[])]).is_err(),
            "invalid pattern"
        );

        let dup = vec![
            custom("acme", r"\bacme_[A-Za-z0-9]{24,}", &["acme_"]),
            custom("acme", r"\bacme_[A-Za-z0-9]{24,}", &["acme_"]),
        ];
        assert!(build_custom(&dup).is_err(), "duplicate id");
    }

    #[test]
    fn a_custom_detector_can_use_a_capture_group_and_shape_gate() {
        let c = vec![crate::scrub::state::CustomDetector {
            id: "acme-header".to_string(),
            pattern: r"X-Acme-Key:\s*([A-Za-z0-9]{20,})".to_string(),
            literals: vec!["X-Acme-Key".to_string()],
            capture: 1,
            mask_prefix: None,
            shape: true,
        }];
        let built = build_custom(&c).unwrap();
        let engine = Engine::new(&built).unwrap();

        let mut out = Vec::new();
        engine.scan("X-Acme-Key: Xy9kL2mQ8rT4vW7nB3sD6fG1", &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, "Xy9kL2mQ8rT4vW7nB3sD6fG1");

        // The shape gate must still reject an identifier.
        out.clear();
        engine.scan("X-Acme-Key: someVariableNameHere", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn resolve_rejects_unknown_names() {
        let bad = vec!["nope".to_string()];
        assert!(resolve(&[], Some(&bad), &[]).is_err());
    }
}
