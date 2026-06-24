//! The synchronization backends.
//!
//! Each backend implements [`crate::backend::Backend`] for one `sync-type` by
//! shelling out to the corresponding external tool through the injectable
//! command runner. The rsync backend is the default; git and webrsync cover the
//! other primary transports. The `cvs`, `svn`, and `mercurial` backends are
//! recorded as lower priority and are left unimplemented in this change, so the
//! engine treats them as unknown backends.

pub mod git;
pub mod rsync;
pub mod webrsync;

pub use git::GitBackend;
pub use rsync::{Freshness, RsyncBackend, classify_freshness};
pub use webrsync::WebrsyncBackend;
