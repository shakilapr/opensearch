//! Stable, dependency-free shard checksums.
//!
//! FNV-1a (64-bit) is used instead of SHA-2 because the workspace does not guarantee `sha2` for
//! this crate, the intent is integrity/tamper detection (not cryptography), and a hand-rolled
//! hash keeps `search-dataset` free of a new dependency.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash of a byte slice.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hex-encoded, algorithm-tagged checksum string stored in a [`crate::ShardInfo`].
pub fn checksum_hex(bytes: &[u8]) -> String {
    format!("fnv1a64:{:016x}", fnv1a64(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_matches_offset_basis() {
        assert_eq!(checksum_hex(b""), "fnv1a64:cbf29ce484222325");
    }

    #[test]
    fn known_vector() {
        // FNV-1a 64 of "hello" is a published test vector.
        assert_eq!(fnv1a64(b"hello"), 0xa430_d846_80aa_bd0b);
    }

    #[test]
    fn single_bit_flip_changes_hash() {
        assert_ne!(checksum_hex(b"abc"), checksum_hex(b"abd"));
    }
}
