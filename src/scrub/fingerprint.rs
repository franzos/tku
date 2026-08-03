//! Salted fingerprints. The same secret reads identically everywhere it was
//! redacted; different secrets stay distinguishable. 8 hex (32 bits) keeps the
//! collision probability at ~0.002% for 450 distinct values — 4 hex would be
//! ~79%, which would falsify that property.

use sha2::{Digest, Sha256};

pub const SHORT_LEN: usize = 8;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn hash_hex(salt: &[u8], value: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(value.as_bytes());
    to_hex(&h.finalize())
}

pub fn short(hash_hex: &str) -> &str {
    &hash_hex[..SHORT_LEN.min(hash_hex.len())]
}

pub fn random_hex(len_bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; len_bytes];
    getrandom::getrandom(&mut buf)?;
    Ok(to_hex(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_salt_dependent() {
        let a = hash_hex(b"salt-a", "sk-or-v1-abc");
        let b = hash_hex(b"salt-a", "sk-or-v1-abc");
        let c = hash_hex(b"salt-b", "sk-or-v1-abc");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn short_is_eight_hex() {
        let h = hash_hex(b"s", "v");
        assert_eq!(short(&h).len(), 8);
        assert!(short(&h).bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn random_hex_is_the_right_width_and_varies() {
        let a = random_hex(32).unwrap();
        let b = random_hex(32).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
