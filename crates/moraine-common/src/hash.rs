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
use sha2::Sha512;

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

/// Return the lowercase hex MD5 digest of `data`.
///
/// Retained only to validate stock Gentoo md5-cache entries during import.
pub fn md5(data: &[u8]) -> String {
    to_hex(&Md5::digest(data))
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
