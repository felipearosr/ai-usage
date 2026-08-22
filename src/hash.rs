//! Stable 64-bit FNV-1a hashing.
//!
//! Used for session-id pseudonymization and deterministic event identity.
//! Hand-rolled instead of pulling in a crypto crate: these hashes are for
//! deduplication and privacy pseudonymization only, never for security, and
//! the dependency budget stays minimal. The algorithm is fixed, so values
//! are stable across runs, machines, and aiu versions.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hex-encoded FNV-1a64 of `input`. Session ids are stored only in this
/// form — raw session identifiers never touch the database (privacy rule).
pub fn short_hash_hex(input: &str) -> String {
    format!("{:016x}", fnv1a64(input.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn hex_form_is_stable_and_fixed_width() {
        let h = short_hash_hex("session-abc");
        assert_eq!(h.len(), 16);
        assert_eq!(h, short_hash_hex("session-abc"));
        assert_ne!(h, short_hash_hex("session-abd"));
    }
}
