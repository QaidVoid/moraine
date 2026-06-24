# AGENTS.md

## Project Overview

Moraine is a from-scratch, clean-room reimplementation of Gentoo's Portage package manager in Rust. The goal is speed: stock Portage resolves dependencies with a large single-module backtracking resolver written in Python that parses text caches at runtime, and that is slow. This rewrite replaces it with a modern conflict-driven solver over greenfield on-disk formats, while remaining able to import existing Gentoo data.

Three decisions shape everything:

1. Full replacement, including the build and merge (write) path, not only resolution.
2. A modern conflict-driven solver (PubGrub / CDCL style), not a port of Portage's backtracking loop.
3. Greenfield optimized on-disk formats with importers from stock Gentoo data. Runtime reads use the greenfield stores; stock formats (md5-cache, `/var/db/pkg`, xpak/gpkg) are touched only by importers.

## Architecture at a Glance

The codebase is a Cargo workspace of layered crates. Each crate owns one subsystem and depends only on crates below it. No library crate depends on the CLI.

```
moraine-cli                         user-facing binary
  └─ moraine-resolve                Gentoo encoding + dependency graph + merge order
       ├─ moraine-solver            generic conflict-driven solver (no Gentoo knowledge)
       ├─ moraine-repo              available packages: metadata store, importer, query
       ├─ moraine-vdb               installed packages: store, importer, CONTENTS
       └─ moraine-config            make.conf, profiles, USE resolution, masking, sets
            └─ moraine-atom         atoms, USE-dep model, dependency-string AST
                 └─ moraine-version Gentoo version parse and compare
moraine-build, moraine-merge, moraine-binpkg, moraine-sync   write path, binaries, sync
moraine-eapi                        EAPI 0..9 feature-flag table
moraine-common                      shared primitives, error building blocks
```

## Crate Layout

| Crate        | Responsibility |
|--------------|----------------|
| `moraine-common`  | Filesystem helpers (atomic write, mmap), checksums (BLAKE2B/SHA512/MD5), string interning, id newtypes, shared error building blocks. No domain logic. |
| `moraine-eapi`    | The EAPI feature-flag table (EAPI 0 through 9). Pure data plus lookup. |
| `moraine-version` | Gentoo version parsing and comparison. |
| `moraine-atom`    | Package atoms, the USE-dependency model, and the dependency-string AST. |
| `moraine-config`  | make.conf, profile stacking, USE and USE_EXPAND resolution, masking and keywords, package sets. |
| `moraine-repo`    | Repository discovery, the greenfield metadata store, its importer, and the package query API. |
| `moraine-vdb`     | The greenfield installed-package store, its importer, and CONTENTS tracking. |
| `moraine-solver`  | The generic conflict-driven solver. No Gentoo knowledge. |
| `moraine-resolve` | Encodes Gentoo semantics into `moraine-solver`, builds the priority dependency graph, and serializes the merge order. |
| `moraine-binpkg`  | Binary package formats and binhost handling. |
| `moraine-sync`    | Repository sync backends. |
| `moraine-build`   | Ebuild phase execution and sandboxing. |
| `moraine-merge`   | Merging a built image into the live filesystem and recording installed state. |
| `moraine-cli`     | The binary crate, the user-facing frontend. |

The CLI is installed as `moraine`, deliberately not `emerge`, so development never shadows the system package manager.

## Rust Conventions

### General

- Edition 2024. The toolchain is pinned in `rust-toolchain.toml`.
- Format with `rustfmt` (default settings).
- No warnings. Treat clippy warnings as errors.
- Add and remove dependencies with `cargo add` and `cargo remove`. Never hand-edit dependency tables in `Cargo.toml`. Pin shared versions in `[workspace.dependencies]` (`cargo add --package <crate> ... ` or editing through cargo), and have member crates pull them in with `cargo add <dep> --workspace`-style references rather than manual entries.
- Prefer simple, readable, maintainable code. Do not add abstraction for something that is not yet used. When complexity is genuinely needed for performance, say so explicitly.

### Errors and diagnostics

- Library crates expose typed errors with `thiserror` and never print. They return errors; the caller decides what to show.
- Preserve cause chains when wrapping a lower-level error.
- The CLI boundary renders errors through a single diagnostic reporter that can show the message, the cause chain, and, where available, source location and help text. Solver conflicts and parse errors both benefit from located, explained messages.
- No `unwrap()` or `expect()` in library code. Use `?` or return a typed error. They are acceptable only in tests.

### Observability

- Use the `tracing` facade for structured spans and events. Never use `println!` for diagnostics.
- Instrument hot paths (metadata import, candidate selection, propagation, backtracking) with named spans so resolution can be profiled and a timing breakdown produced.

### Concurrency

- Data-parallel work (importing or loading metadata across thousands of packages) uses `rayon`.
- No async. The whole workspace uses synchronous code. Network concurrency (sync transfers, distfile and binary-package fetching in `moraine-sync`, `moraine-build`, `moraine-binpkg`) comes from a bounded thread pool with a blocking HTTP client, not an async runtime.

## Interop Strategy

Each data-owning crate defines its own optimized on-disk format and provides an importer from the matching stock Gentoo source: `moraine-repo` from md5-cache and ebuilds, `moraine-vdb` from `/var/db/pkg`, `moraine-binpkg` from xpak and gpkg. The runtime path reads the greenfield store and never parses stock text caches. Importers are first-class and tested against a real corpus.

## Testing

- Unit tests live in each crate's `src/`.
- Property tests cover parser round-trips and ordering invariants (version comparison laws, atom parse and format).
- Snapshot tests cover resolver and CLI output.
- Benchmarks cover hot paths.
- A git-ignored `corpus/` directory holds a real Gentoo data snapshot. The harness imports it to compare results and timings against stock Portage. This is how the performance win is demonstrated.

Before considering work done:

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy` passes with no warnings
- [ ] `cargo test` passes
- [ ] No `unwrap()`/`expect()` in library code, no `println!` for diagnostics
- [ ] Public modules and public APIs have `///` doc comments

## Code Style

- Add doc comments for public modules, public APIs, and similar exported items.
- Do not narrate code. Avoid obvious or redundant comments. Comment the why, not the what.
- Match the surrounding code's naming, structure, and idioms.

## Commits

- Commit messages use semantic format: `type(scope): imperative message`. Scope is the crate name without the `moraine-` prefix: `version`, `atom`, `config`, `repo`, `vdb`, `solver`, `resolve`, `build`, `merge`, `binpkg`, `sync`, `cli`, `common`, `eapi`.
- Prefixes: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `chore`, `ci`.
- One logical change per commit. Prefer many small commits over few large ones. A commit should compile.

## Performance Focus

The reason this project exists is speed, especially dependency resolution. Keep these in mind:

- Parse once into compact representations. Pre-parse dependency strings at import time so the resolver never re-parses.
- Prefer fast startup: mmap and indexed stores over thousands of small text files.
- The solver learns from conflicts instead of re-deriving them, and never deep-copies whole resolution state per attempt.
- Measure with the corpus harness and benchmarks rather than guessing.
