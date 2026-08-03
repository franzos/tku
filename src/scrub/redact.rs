//! Build the replacement text for a detected value.

use crate::scrub::detect::{Detector, MaskStyle};

pub fn mask(det: &Detector, value: &str, fp: &str) -> String {
    match det.mask {
        MaskStyle::KeepPrefix(n) => {
            // Never keep more than half the value, and never split a char.
            let mut keep = n.min(value.len() / 2);
            while keep > 0 && !value.is_char_boundary(keep) {
                keep -= 1;
            }
            format!("{}[REDACTED:{fp}]", &value[..keep])
        }
        MaskStyle::Whole => format!("[REDACTED:{}:{fp}]", det.id),
    }
}

/// How the value is rendered on the "before" side of a preview. Reveals
/// exactly what [`mask`] would leave behind and not one byte more, so showing
/// a preview never discloses more than applying it would.
pub fn preview(det: &Detector, value: &str) -> String {
    let keep = match det.mask {
        MaskStyle::KeepPrefix(n) => {
            let mut k = n.min(value.len() / 2);
            while k > 0 && !value.is_char_boundary(k) {
                k -= 1;
            }
            k
        }
        MaskStyle::Whole => 0,
    };
    let stars = (value.len() - keep).min(24);
    format!("{}{}", &value[..keep], "*".repeat(stars))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrub::detect::DETECTORS;

    fn det(id: &str) -> &'static Detector {
        DETECTORS.iter().find(|d| d.id == id).unwrap()
    }

    const KEY: &str = "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c";

    #[test]
    fn keeps_the_vendor_prefix() {
        assert_eq!(
            mask(det("openrouter-key"), KEY, "a3f91c04"),
            "sk-or-v1-[REDACTED:a3f91c04]"
        );
    }

    #[test]
    fn whole_style_names_the_class() {
        let v = "eyJhbGciOi.eyJzdWIiOi.abcdefghij";
        assert_eq!(mask(det("jwt"), v, "7c02ab19"), "[REDACTED:jwt:7c02ab19]");
    }

    #[test]
    fn never_keeps_more_than_half_a_short_value() {
        let out = mask(det("github-pat"), "ghp_abcd", "00000000");
        assert!(out.starts_with("ghp"));
        assert!(!out.contains("abcd"));
    }

    #[test]
    fn the_mask_never_contains_a_newline() {
        for d in DETECTORS {
            assert!(!mask(d, "abcdefghijklmnopqrstuvwxyz", "deadbeef").contains('\n'));
        }
    }

    #[test]
    fn preview_reveals_no_more_than_the_mask_would() {
        for d in DETECTORS {
            let p = preview(d, KEY);
            let m = mask(d, KEY, "a3f91c04");
            let kept: String = p.chars().take_while(|c| *c != '*').collect();
            assert!(
                m.starts_with(&kept),
                "{}: preview leaks past the mask",
                d.id
            );
            assert!(
                !p.contains("cdb75f9a2e1b4c8d"),
                "{}: preview leaks the body",
                d.id
            );
        }
    }

    #[test]
    fn whole_masked_classes_preview_as_stars_only() {
        assert!(preview(det("jwt"), KEY).chars().all(|c| c == '*'));
    }

    #[test]
    fn distinct_values_stay_distinguishable() {
        let d = det("openrouter-key");
        assert_ne!(mask(d, KEY, "a3f91c04"), mask(d, KEY, "7c02ab19"));
    }
}
