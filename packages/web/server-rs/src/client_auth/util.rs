//! Crypto and encoding helpers shared by the client-auth runtimes.
//!
//! JS precedent: the runtimes hash tokens with `crypto.createHash('sha256')`
//! and compare digests with `crypto.timingSafeEqual(Buffer.from(x, 'hex'))`.

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// `crypto.createHash('sha256').update(input).digest('hex')`.
pub fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `crypto.timingSafeEqual(Buffer.from(left, 'hex'), Buffer.from(right, 'hex'))`
/// with the JS length pre-check: unequal byte lengths short-circuit to false.
///
/// Node's `Buffer.from(str, 'hex')` decodes hex pairs left to right and stops
/// at the first non-hex pair (a dangling single nibble is dropped), so e.g.
/// `"abzz"` decodes to `[0xab]` — mirrored by [`node_hex_decode`].
pub fn constant_time_equal_hex(left: &str, right: &str) -> bool {
    let left_bytes = node_hex_decode(left);
    let right_bytes = node_hex_decode(right);
    constant_time_equal_bytes(&left_bytes, &right_bytes)
}

/// Byte-wise constant-time equality (length mismatch is decided up front,
/// exactly like the JS pre-check).
pub fn constant_time_equal_bytes(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// `Buffer.from(str, 'hex')`: consume hex digit pairs; stop at the first
/// invalid pair; ignore a trailing lone nibble.
fn node_hex_decode(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(hi), Some(lo)) => out.push(((hi << 4) | lo) as u8),
            _ => break,
        }
        i += 2;
    }
    out
}

/// `crypto.randomBytes(n).toString('hex')`.
pub fn random_hex(count: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; count];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `crypto.randomBytes(n).toString('base64url')` (unpadded).
pub fn random_base64url(count: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; count];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `crypto.randomUUID()` (RFC 4122 v4, dashed like the fs_routes helper).
pub fn random_uuid() -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_matches_reference_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn constant_time_equal_hex_mirrors_node_buffer_semantics() {
        assert!(constant_time_equal_hex(
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
        // Same hash with different case still decodes to equal bytes.
        assert!(constant_time_equal_hex("AB", "ab"));
        // Unequal digests and length mismatches.
        assert!(!constant_time_equal_hex("ab", "cd"));
        assert!(!constant_time_equal_hex("abcd", "abc"));
        // Node truncation: an invalid pair ends decoding, a lone nibble drops.
        assert!(constant_time_equal_hex("abzz", "ab"));
        assert!(constant_time_equal_hex("abz", "ab"));
        assert!(constant_time_equal_hex("ab", "abz"));
        assert!(constant_time_equal_hex("zz", ""));
    }

    #[test]
    fn random_generators_shape() {
        assert_eq!(random_hex(12).len(), 24);
        assert!(random_hex(12).bytes().all(|b| b.is_ascii_hexdigit()));
        let token = random_base64url(32);
        assert_eq!(token.len(), 43);
        assert!(!token.contains(['+', '/', '=']));
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(uuid.starts_with(|c: char| c.is_ascii_hexdigit()));
    }
}
