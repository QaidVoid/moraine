//! Corpus diff harness, gated on a real Gentoo corpus.
//!
//! Set `MORAINE_CORPUS` to a captured system root to exercise this. Full diffs
//! of effective USE, masking, keyword acceptance, and `@system`/`@world` against
//! stock Portage are wired once the repository and installed stores land; this
//! confirms the corpus `make.conf` parses today.

use std::path::PathBuf;

use moraine_config::makeconf::VarMap;

#[test]
fn corpus_make_conf_parses_when_present() {
    let Some(root) = std::env::var_os("MORAINE_CORPUS") else {
        return;
    };
    let path = PathBuf::from(root).join("etc/portage/make.conf");
    if !path.exists() {
        return;
    }
    let mut env = VarMap::new();
    env.merge_path(&path)
        .expect("corpus make.conf should parse");
}
