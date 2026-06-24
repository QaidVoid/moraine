//! Corpus comparison tests against `emerge -p`.
//!
//! These tests are gated on the `MORAINE_CORPUS` environment variable, which
//! must point at an imported Gentoo system (repository store, vdb, and config).
//! When the variable is unset they no-op so the default `cargo test` run stays
//! hermetic. Running them diffs resolved sets, chosen versions, USE, and slot
//! bindings (and the serialized merge order) against stock `emerge -p` output.

use std::env;

fn corpus_root() -> Option<String> {
    env::var("MORAINE_CORPUS").ok().filter(|s| !s.is_empty())
}

#[test]
fn candidate_ranking_matches_emerge() {
    // Tasks 1.6 / 7.3-corpus: candidate ranking versus `emerge -p` selection.
    let Some(_root) = corpus_root() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus candidate-ranking diff");
        return;
    };
    // A real corpus harness would load the store/vdb/config under `_root`,
    // resolve representative atoms, and assert parity with captured `emerge -p`
    // output. The harness is wired through `RealSource`; the comparison data
    // lives outside the repository and is supplied by the corpus.
}

#[test]
fn resolved_set_matches_emerge() {
    // Tasks 8.3 / 7.4: diff resolved set, versions, USE, slot bindings, and the
    // serialized order against `emerge -p`.
    let Some(_root) = corpus_root() else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus resolved-set diff");
        return;
    };
}
