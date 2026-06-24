//! Snapshot tests for the merge-list and tree renderers.
//!
//! These build a merge plan from constructed `moraine-resolve` fixtures, so they
//! exercise the full plan-building and rendering path without a real Gentoo
//! system.

use std::collections::BTreeSet;

use moraine_cli::plan::build_plan;
use moraine_cli::render::{render_merge_list, render_tree};
use moraine_resolve::{
    DepClass, DepEdge, InstalledMeta, PackageMeta, ResolveSource, ResolvedPackage,
    ResolvedSolution, Root, Task, TaskKind,
};
use moraine_version::Version;

/// A source carrying only installed state, enough to classify operations and
/// compute USE diffs in the plan builder.
#[derive(Default)]
struct FixtureSource {
    installed: Vec<InstalledMeta>,
}

impl ResolveSource for FixtureSource {
    fn versions_of(&self, _cp: &str) -> Vec<PackageMeta> {
        Vec::new()
    }
    fn is_visible(&self, _meta: &PackageMeta) -> bool {
        true
    }
    fn resolved_use(&self, _meta: &PackageMeta) -> BTreeSet<String> {
        BTreeSet::new()
    }
    fn is_provided(&self, _cp: &str, _version: &Version) -> bool {
        false
    }
    fn installed(&self, cp: &str) -> Vec<InstalledMeta> {
        self.installed
            .iter()
            .filter(|i| i.cp == cp)
            .cloned()
            .collect()
    }
}

fn installed(cp: &str, version: &str, slot: &str, use_enabled: &[&str]) -> InstalledMeta {
    InstalledMeta {
        cp: cp.to_owned(),
        version: Version::parse(version).unwrap(),
        slot: slot.to_owned(),
        subslot: None,
        use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
        slot_bindings: Vec::new(),
    }
}

fn resolved(cp: &str, version: &str, slot: &str, use_enabled: &[&str]) -> ResolvedPackage {
    ResolvedPackage {
        cp: cp.to_owned(),
        version: Version::parse(version).unwrap(),
        slot: slot.to_owned(),
        subslot: None,
        use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
        slot_bindings: Vec::new(),
        already_installed: false,
        subslot_rebuild: false,
    }
}

fn task(cp: &str, version: &str, slot: &str, use_enabled: &[&str]) -> Task {
    Task {
        kind: TaskKind::Merge,
        cp: cp.to_owned(),
        version: version.to_owned(),
        slot: slot.to_owned(),
        use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
    }
}

fn fixture() -> (Vec<Task>, ResolvedSolution, FixtureSource) {
    let tasks = vec![
        task("dev-libs/openssl", "3.1.4", "0", &["asm", "zlib"]),
        task("app-misc/tool", "2.0", "0", &["ssl"]),
        task("dev-lang/python", "3.12.1", "3.12", &[]),
    ];
    let solution = ResolvedSolution {
        packages: vec![
            resolved("dev-libs/openssl", "3.1.4", "0", &["asm", "zlib"]),
            resolved("app-misc/tool", "2.0", "0", &["ssl"]),
            resolved("dev-lang/python", "3.12.1", "3.12", &[]),
        ],
        edges: vec![
            DepEdge {
                from: "app-misc/tool".to_owned(),
                to: "dev-libs/openssl".to_owned(),
                class: DepClass::Rdepend,
                root: Root::Target,
                build_time: false,
                slot_op: false,
                optional: false,
            },
            DepEdge {
                from: "app-misc/tool".to_owned(),
                to: "dev-lang/python".to_owned(),
                class: DepClass::Depend,
                root: Root::Target,
                build_time: true,
                slot_op: false,
                optional: false,
            },
        ],
        blockers: Vec::new(),
    };
    let source = FixtureSource {
        installed: vec![
            installed("dev-libs/openssl", "3.0.9", "0", &["zlib"]),
            installed("app-misc/tool", "2.0", "0", &["ssl"]),
        ],
    };
    (tasks, solution, source)
}

#[test]
fn merge_list_matches_snapshot() {
    let (tasks, solution, source) = fixture();
    let mut plan = build_plan(&tasks, &solution, &source);
    // Attach deterministic fetch sizes and repositories for the snapshot.
    plan.entries[0].fetch_size = Some(9_500_000);
    plan.entries[0].repository = Some("gentoo".to_owned());
    plan.entries[2].fetch_size = Some(25_000_000);
    plan.entries[2].repository = Some("gentoo".to_owned());
    insta::assert_snapshot!("merge_list", render_merge_list(&plan, false));
}

#[test]
fn merge_list_verbose_matches_snapshot() {
    let (tasks, solution, source) = fixture();
    let mut plan = build_plan(&tasks, &solution, &source);
    plan.entries[0].fetch_size = Some(9_500_000);
    plan.entries[0].repository = Some("gentoo".to_owned());
    plan.entries[2].repository = Some("gentoo".to_owned());
    insta::assert_snapshot!("merge_list_verbose", render_merge_list(&plan, true));
}

#[test]
fn tree_matches_snapshot() {
    let (tasks, solution, source) = fixture();
    let plan = build_plan(&tasks, &solution, &source);
    insta::assert_snapshot!("tree", render_tree(&plan, false));
}
