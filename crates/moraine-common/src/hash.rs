//! Content checksums.
//!
//! Two roles, deliberately separated:
//!
//! - **Compatibility hashes** for reading and verifying stock Gentoo data.
//!   [`blake2b`] and [`sha512`] are the Gentoo `Manifest` defaults, and [`md5`]
//!   validates legacy md5-cache eclass entries. These must match the digests
//!   Gentoo already wrote, so the algorithms are fixed.
//! - **Greenfield hash** for Moraine's own stores and content addressing.
//!   [`blake3`] is the default here: faster and parallelizable, with no need to
//!   match any external format.

use blake2::Blake2b512;
use digest::Digest as _;
use md5::Md5;
use sha2::{Sha256, Sha512};

/// Return the lowercase hex BLAKE3 digest of `data`.
///
/// This is the greenfield default for Moraine's own integrity needs.
pub fn blake3(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Return the lowercase hex BLAKE2b-512 digest of `data`.
///
/// A Gentoo `Manifest` default; used for compatibility with stock data.
pub fn blake2b(data: &[u8]) -> String {
    to_hex(&Blake2b512::digest(data))
}

/// Return the lowercase hex SHA-512 digest of `data`.
///
/// A Gentoo `Manifest` default; used for compatibility with stock data.
pub fn sha512(data: &[u8]) -> String {
    to_hex(&Sha512::digest(data))
}

/// Return the lowercase hex SHA-256 digest of `data`.
///
/// A Gentoo `Manifest` algorithm; computed so a `SHA256`-listed entry is
/// actually verified rather than silently skipped.
pub fn sha256(data: &[u8]) -> String {
    to_hex(&Sha256::digest(data))
}

/// Return the lowercase hex MD5 digest of `data`.
///
/// Retained only to validate stock Gentoo md5-cache entries during import.
pub fn md5(data: &[u8]) -> String {
    to_hex(&Md5::digest(data))
}

/// Return the lowercase hex SHA-1 digest of `data`.
///
/// A binhost `Packages` index records both `MD5` and `SHA1` per stanza. SHA-1 is
/// implemented inline (no external crate) since it is only used for that index
/// key; it is not used for any security decision.
pub fn sha1(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64).wrapping_mul(8);

    // Pad: 0x80, then zeros, then the 64-bit big-endian bit length.
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = String::with_capacity(40);
    for word in h {
        use std::fmt::Write as _;
        let _ = write!(out, "{word:08x}");
    }
    out
}

/// A streaming multi-algorithm hasher for verifying a file without reading it
/// whole into memory. Only the algorithms named at construction are computed.
///
/// Algorithm names are the uppercase Manifest names (`BLAKE2B`, `SHA512`,
/// `SHA256`, `MD5`); unknown names are ignored.
pub struct MultiHasher {
    blake2b: Option<Blake2b512>,
    sha512: Option<Sha512>,
    sha256: Option<Sha256>,
    md5: Option<Md5>,
}

impl MultiHasher {
    /// Build a hasher computing each named algorithm it understands.
    pub fn new<'a>(algos: impl IntoIterator<Item = &'a str>) -> Self {
        let mut h = MultiHasher {
            blake2b: None,
            sha512: None,
            sha256: None,
            md5: None,
        };
        for algo in algos {
            match algo {
                "BLAKE2B" => h.blake2b = Some(Blake2b512::new()),
                "SHA512" => h.sha512 = Some(Sha512::new()),
                "SHA256" => h.sha256 = Some(Sha256::new()),
                "MD5" => h.md5 = Some(Md5::new()),
                _ => {}
            }
        }
        h
    }

    /// Feed a chunk of data to every active hasher.
    pub fn update(&mut self, data: &[u8]) {
        if let Some(h) = &mut self.blake2b {
            h.update(data);
        }
        if let Some(h) = &mut self.sha512 {
            h.update(data);
        }
        if let Some(h) = &mut self.sha256 {
            h.update(data);
        }
        if let Some(h) = &mut self.md5 {
            h.update(data);
        }
    }

    /// Finalize, returning the lowercase hex digests keyed by uppercase algorithm
    /// name.
    pub fn finalize(self) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        if let Some(h) = self.blake2b {
            out.insert("BLAKE2B".to_string(), to_hex(&h.finalize()));
        }
        if let Some(h) = self.sha512 {
            out.insert("SHA512".to_string(), to_hex(&h.finalize()));
        }
        if let Some(h) = self.sha256 {
            out.insert("SHA256".to_string(), to_hex(&h.finalize()));
        }
        if let Some(h) = self.md5 {
            out.insert("MD5".to_string(), to_hex(&h.finalize()));
        }
        out
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_for_abc() {
        assert_eq!(md5(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha512(b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(
            blake2b(b"abc"),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d1\
             7d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        assert_eq!(
            blake3(b"abc"),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn empty_input_md5() {
        assert_eq!(md5(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
