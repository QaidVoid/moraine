//! Shared primitives for the Moraine package manager.
//!
//! This crate is the lowest layer of the workspace. It holds the building
//! blocks every other crate depends on and contains no package-manager domain
//! logic:
//!
//! - [`fs`]: atomic file writes and memory-mapped reads.
//! - [`glob`]: a minimal `fnmatch`-style glob matcher.
//! - [`hash`]: BLAKE3 for greenfield data, plus BLAKE2b, SHA-512, and MD5 for
//!   compatibility with stock Gentoo formats.
//! - [`intern`]: a thread-safe string interner for repeated tokens.
//! - [`id`]: the [`define_id`] macro for compact newtype identifiers.
//! - [`error`]: the building blocks for typed library errors.

pub mod error;
pub mod fs;
pub mod glob;
pub mod hash;
pub mod id;
pub mod intern;

pub use error::{CommonError, IoResultExt};
pub use intern::{Interner, Symbol};
