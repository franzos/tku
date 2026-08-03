//! Report rendering. Never prints an unmasked value.

use std::collections::BTreeMap;

use comfy_table::Table;

use crate::scrub::ScrubOutcome;

/// What actually removes the risk, per class. Scrubbing deletes local copies;
/// it does not un-send a key that already went to a model API, and it cannot
/// reach filesystem snapshots, backups, or the blocks `rename` merely unlinked.
pub(crate) fn rotation_hint(detector_id: &str) -> &'static str {
    match detector_id {
        "anthropic-key" => "revoke at console.anthropic.com",
        "openai-key" => "revoke at platform.openai.com/api-keys",
        "openrouter-key" => "revoke at openrouter.ai/keys",
        "github-pat" | "github-fine-grained" => "revoke in GitHub developer settings",
        "gitlab-pat" => "revoke in GitLab access tokens",
        "aws-access-key-id" => "deactivate and delete the key in IAM",
        "google-api-key" | "google-oauth-secret" => "rotate in Google Cloud credentials",
        "atlassian-token" => "revoke at id.atlassian.com API tokens",
        "stripe-key" => "roll the key in the Stripe dashboard",
        "slack-token" | "slack-webhook" => "revoke in the Slack app config",
        "npm-token" => "revoke with npm token revoke",
        "pypi-token" => "revoke in PyPI account settings",
        "hf-token" => "revoke in Hugging Face access tokens",
        "sendgrid-key" => "revoke in SendGrid API keys",
        "digitalocean-token" => "revoke in the DigitalOcean API panel",
        "linear-key" => "revoke in Linear API settings",
        "langfuse-key" => "rotate in Langfuse project settings",
        "polar-token" => "revoke in Polar settings",
        "private-key" | "age-secret-key" => "generate a new keypair and re-enrol",
        "jwt" => "expire the session and rotate the signing key",
        "url-password" | "env-assign" => "rotate the credential at its source",
        _ => "rotate this credential",
    }
}

/// Per-finding occurrence listing, shown instead of the summary table when
/// `--preview` is given. Neither side of the diff contains the secret.
fn print_preview(outcome: &ScrubOutcome) {
    for f in &outcome.findings {
        let tag = if f.wide { "  [wide - not swept]" } else { "" };
        println!(
            "\n{} {}  {} site(s) in {} file(s){tag}",
            f.detector_id, f.fp, f.occurrences, f.files
        );
        for s in &f.sites {
            println!(
                "  {}:{}  ({})",
                crate::accounts::redact(&s.path),
                s.line,
                s.label
            );
            println!("    - {}", s.before);
            println!("    + {}", s.after);
        }
        if f.sites.is_empty() {
            println!("  (no site captured)");
        }
    }
}

pub fn print_table(outcome: &ScrubOutcome) {
    let gb = outcome.bytes_scanned as f64 / 1024.0 / 1024.0 / 1024.0;

    let previewing = outcome.findings.iter().any(|f| !f.sites.is_empty());
    if previewing {
        print_preview(outcome);
        println!();
    }

    if outcome.findings.is_empty() {
        println!(
            "no matches in {} files ({gb:.2} GB) for the detectors that ran",
            outcome.files_scanned
        );
    } else {
        if !previewing {
            let mut table = Table::new();
            table.set_header(vec!["CLASS", "MASKED", "FINGERPRINT", "SITES", "FILES", ""]);
            for f in &outcome.findings {
                table.add_row(vec![
                    f.detector_id.to_string(),
                    f.masked.clone(),
                    f.fp.clone(),
                    f.occurrences.to_string(),
                    f.files.to_string(),
                    if f.wide {
                        "wide - not swept".to_string()
                    } else {
                        String::new()
                    },
                ]);
            }
            println!("{table}");
        }

        let mut per_class: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &outcome.findings {
            *per_class.entry(f.detector_id).or_insert(0) += 1;
        }
        println!("\nRotate these. Redaction removes local copies only:");
        for (class, n) in &per_class {
            println!("  {class}: {n} - {}", rotation_hint(class));
        }

        println!(
            "\n{} unique across {} files ({gb:.2} GB scanned)",
            outcome.findings.len(),
            outcome.files_scanned
        );
    }

    if !outcome.per_store.is_empty() {
        let stores: Vec<String> = outcome
            .per_store
            .iter()
            .map(|(label, n)| format!("{label} {n}"))
            .collect();
        println!("stores: {}", stores.join(", "));
    }

    let wide = outcome.findings.iter().filter(|f| f.wide).count();
    if wide > 0 {
        println!(
            "{wide} contextual match(es) appear in more than --max-spread files and were not \
             swept; values that common are usually not secrets. Pass --include-wide to sweep them."
        );
    }
    if !outcome.live_skipped.is_empty() {
        println!(
            "{} file(s) skipped as live (recently modified); re-run later to cover them",
            outcome.live_skipped.len()
        );
    }
    if !outcome.symlinks_skipped.is_empty() {
        println!("{} symlink(s) skipped", outcome.symlinks_skipped.len());
    }
    if !outcome.oversize_skipped.is_empty() {
        println!(
            "{} file(s) skipped as oversize",
            outcome.oversize_skipped.len()
        );
    }
    if outcome.unreadable > 0 {
        println!(
            "{} file(s) skipped as unreadable (not UTF-8)",
            outcome.unreadable
        );
    }

    for (path, why) in &outcome.failed {
        eprintln!("left untouched: {} ({why})", crate::accounts::redact(path));
    }

    if let Some((path, n)) = &outcome.exported {
        eprintln!(
            "wrote {n} secret(s) in plaintext to {} (mode 0600). Rotate them and delete it; \
             it is an undo map, not an archive.",
            path.display()
        );
    }

    if outcome.applied {
        println!("{} file(s) rewritten", outcome.files_rewritten);
    } else if !outcome.findings.is_empty() {
        println!(
            "run `tku scrub --apply` to redact; `tku scrub --allow <fingerprint>` to ignore one"
        );
    }
}

pub fn print_json(outcome: &ScrubOutcome) {
    let findings: Vec<serde_json::Value> = outcome
        .findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "detector": f.detector_id,
                "category": f.category.as_str(),
                "fingerprint": f.fp,
                "full_fingerprint": f.full_fp,
                "masked": f.masked,
                "occurrences": f.occurrences,
                "files": f.files,
                "wide": f.wide,
                "rotate": rotation_hint(f.detector_id),
                "sites": f.sites.iter().map(|s| serde_json::json!({
                    "path": s.path,
                    "store": s.label,
                    "line": s.line,
                    "before": s.before,
                    "after": s.after,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    let out = serde_json::json!({
        "findings": findings,
        "files_scanned": outcome.files_scanned,
        "bytes_scanned": outcome.bytes_scanned,
        "applied": outcome.applied,
        "files_rewritten": outcome.files_rewritten,
        "live_skipped": outcome.live_skipped.len(),
        "symlinks_skipped": outcome.symlinks_skipped.len(),
        "oversize_skipped": outcome.oversize_skipped.len(),
        "unreadable": outcome.unreadable,
        "failed": outcome.failed.len(),
        "per_store": outcome.per_store,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}
