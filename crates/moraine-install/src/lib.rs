//! The Moraine install orchestrator.
//!
//! `moraine-install` is the write-path orchestration layer: it takes a resolved,
//! ordered task list and drives it to completion through the existing build,
//! merge, binary-package, and sync engines. It owns the transaction loop,
//! locking, the resume journal, interruption recovery, removal planning
//! (unmerge, depclean, prune), binary-package creation, post-transaction commit
//! (world set, environment regeneration, news), and protected-config resolution.
//!
//! The dangerous live-filesystem writes stay in `moraine-merge`; this crate never
//! touches the live root directly. The transaction loop is generic over the
//! [`StepRunner`] and [`Applier`] traits so the ordering, locking, journal, and
//! resume logic are tested against fakes, with [`EngineApplier`] providing the
//! real merge-engine binding.

pub mod apply;
pub mod commit;
pub mod config_update;
pub mod engine;
pub mod error;
pub mod global_update;
pub mod journal;
pub mod lock;
pub mod quickpkg;
pub mod realize;
pub mod remove;
pub mod step;
pub mod task;

pub use apply::EngineApplier;
pub use commit::{WorldUpdate, env_update, mark_news_read};
pub use config_update::{PendingUpdate, Resolution, resolve_update};
pub use engine::{
    PackageElog, PackageFailure, TaskOutcome, TransactionEngine, TransactionReport, has_pending,
};
pub use error::{InstallError, Result};
pub use global_update::{GlobalUpdateReport, global_update, update_config_files};
pub use journal::Journal;
pub use lock::TransactionLock;
pub use quickpkg::{QuickpkgInput, package_image_dir};
pub use realize::{
    BinhostSource, BinpkgRunner, BinpkgSource, BuildOptions, BuildPlanner, LocalPkgdir,
    SourceRunner, StoreVersionQuery, locate_local_gpkg, realize_binpkg,
};
pub use remove::{
    InstalledPackage, RemovalSet, depclean_orphans, depclean_targeted, prune_superseded,
    would_break_retained,
};
pub use step::{Applier, StepRunner};
pub use task::{InstallTask, Realized, SourceKind, TaskKind, Transaction};
