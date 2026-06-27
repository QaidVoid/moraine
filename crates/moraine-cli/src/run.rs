//! The top-level read-only run flow and its timing breakdown.
//!
//! The flow loads configuration, expands targets, resolves, renders, and reports
//! news, recording the elapsed time of each phase. This phase is strictly
//! read-only: it never builds, fetches, syncs, merges, or writes persisted
//! state. The end-to-end resolve over a real system is gated on the
//! `MORAINE_CORPUS` environment variable and no-ops when unset, so the default
//! test run stays hermetic.

use std::time::{Duration, Instant};

use crate::args::Cli;
use crate::config::{ConfigContext, Roots};
use crate::sets::{Modifiers, Request, expand};

/// The elapsed time of each resolution phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timing {
    /// Configuration and store loading.
    pub load: Duration,
    /// Target and set expansion.
    pub expand: Duration,
    /// Solving.
    pub solve: Duration,
    /// Merge-order serialization.
    pub serialize: Duration,
    /// Rendering.
    pub render: Duration,
}

impl Timing {
    /// Render the timing breakdown for display.
    pub fn report(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "Resolution timing:");
        let _ = writeln!(out, "  load:      {:?}", self.load);
        let _ = writeln!(out, "  expand:    {:?}", self.expand);
        let _ = writeln!(out, "  solve:     {:?}", self.solve);
        let _ = writeln!(out, "  serialize: {:?}", self.serialize);
        let _ = writeln!(out, "  render:    {:?}", self.render);
        out
    }
}

/// Build the resolver modifiers from the parsed command line.
pub fn modifiers_from(cli: &Cli) -> Modifiers {
    Modifiers {
        update: cli.update,
        deep: cli.is_deep(),
        deep_depth: cli.deep_depth(),
        newuse: cli.newuse,
        oneshot: cli.oneshot,
    }
}

/// Build the global root selection from the parsed command line.
pub fn roots_from(cli: &Cli) -> Roots {
    Roots {
        root: cli.root.clone(),
        config_root: cli.config_root.clone(),
        profile: cli.profile.clone(),
    }
}

/// Load the config and expand targets into a request, timing each phase.
///
/// This is the read-only front half of the flow that does not require a real
/// repository or installed store, so it is exercised in unit tests. The solver
/// half runs only over a real corpus.
pub fn load_and_expand(cli: &Cli) -> miette::Result<(ConfigContext, Request, Timing)> {
    let mut timing = Timing::default();

    let load_start = Instant::now();
    let ctx = ConfigContext::load(&roots_from(cli))?;
    timing.load = load_start.elapsed();

    let expand_start = Instant::now();
    let request = expand(&ctx, &cli.targets, &cli.exclude, modifiers_from(cli))?;
    timing.expand = expand_start.elapsed();

    Ok((ctx, request, timing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_track_flags() {
        let cli = Cli::parse_from_args(["-puDN", "@world"].map(String::from)).unwrap();
        let mods = modifiers_from(&cli);
        assert!(mods.update && mods.deep && mods.newuse);
    }

    #[test]
    fn timing_report_lists_phases() {
        let report = Timing::default().report();
        assert!(report.contains("load:"));
        assert!(report.contains("solve:"));
        assert!(report.contains("render:"));
    }

    #[test]
    fn load_and_expand_over_temp_root() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("var/lib/portage/world");
        std::fs::create_dir_all(world.parent().unwrap()).unwrap();
        std::fs::write(&world, "app/editor\n").unwrap();

        let cli = Cli::parse_from_args(
            [
                "-p",
                "--root",
                dir.path().to_str().unwrap(),
                "--config-root",
                dir.path().to_str().unwrap(),
                "@selected",
            ]
            .map(String::from),
        )
        .unwrap();

        let (_ctx, request, _timing) = load_and_expand(&cli).unwrap();
        assert_eq!(request.atoms, vec!["app/editor".to_owned()]);
    }
}
