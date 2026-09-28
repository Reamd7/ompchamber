//! 供 hunk id 使用的最小 SHA-1 实现：输出必须与 Node 的
//! `createHash('sha1')` 逐字节一致，否则 JS 服务写入的缓存条目在
//! Rust 服务下会全部 miss。
//! Minimal SHA-1 for hunk identity.
//!
//! JS precedent (`hunks.js`): ids are `<scope>:<path>:<sha1(header + body)[:8]>`.
//! The `sha1` crate is not a direct dependency of this crate (only a transitive
//! one), so the digest is implemented here to stay byte-for-byte identical to
//! Node's `crypto.createHash('sha1')` — the JS server's cached walkthrough
//! entries must keep hitting under the Rust server.

/// 计算 `data` 的 SHA-1 摘要并返回小写十六进制串（等价于
/// `createHash('sha1').digest('hex')`）。
/// SHA-1 digest as lowercase hex (matches `createHash('sha1').digest('hex')`).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let base = i * 4;
            *word = u32::from_be_bytes([
                chunk[base],
                chunk[base + 1],
                chunk[base + 2],
                chunk[base + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = String::with_capacity(40);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// 摘要与公开已知向量的对齐测试。
#[cfg(test)]
mod tests {
    use super::sha1_hex;

    // Vectors from RFC 3174 / NIST.
/// 断言 "abc"、空串与经典测试句命中 RFC 3174 公开摘要。
    #[test]
    fn matches_known_vectors() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

/// 断言 56/64 字节输入（补位跨越块边界）仍与 Node 的摘要一致。
    #[test]
    fn handles_multi_block_input() {
        // 56-byte input forces two blocks (padding spills over); 64-byte input
        // pads into a second block. Both must match Node's digest.
        let fifty_six = "a".repeat(56);
        assert_eq!(
            sha1_hex(fifty_six.as_bytes()),
            "c2db330f6083854c99d4b5bfb6e8f29f201be699"
        );
        let sixty_four = "0123456789abcdef".repeat(4);
        assert_eq!(
            sha1_hex(sixty_four.as_bytes()),
            "ce4303f6b22257d9c9cf314ef1dee4707c6e1c13"
        );
    }
}
