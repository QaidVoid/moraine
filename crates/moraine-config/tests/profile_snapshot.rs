//! Snapshot test for a resolved profile chain.

use std::fs;

use moraine_config::profile::{ProfileContext, ProfileStack, RepoProfileInfo};

#[test]
fn profile_chain_orders_parents_first() {
    let dir = tempfile::tempdir().unwrap();
    for node in ["base", "intel", "desktop"] {
        fs::create_dir_all(dir.path().join(node)).unwrap();
    }
    fs::write(dir.path().join("intel/parent"), "../base\n").unwrap();
    fs::write(dir.path().join("desktop/parent"), "../intel\n").unwrap();

    let ctx = ProfileContext {
        repo_profiles: &|_| None,
        node_repo: &|_| RepoProfileInfo::default(),
    };
    let stack = ProfileStack::from_profile(&dir.path().join("desktop"), &ctx).unwrap();
    let names: Vec<String> = stack
        .nodes
        .iter()
        .map(|n| n.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    insta::assert_debug_snapshot!(names, @r###"
    [
        "base",
        "intel",
        "desktop",
    ]
    "###);
}
