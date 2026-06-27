//! The ebuild-to-manager IPC channel.
//!
//! A running phase can issue `has_version` and `best_version` queries; the engine
//! answers from the installed-store and repository query APIs. This module models
//! the manager side as a Rust handler over an injectable [`VersionQuery`] trait,
//! plus a small line-oriented request framing both ends own. The phase driver
//! enables the channel only for the phases stock Portage enables it for.
//!
//! The framing is a single request line `op root atom use...`, where `op` is
//! `has_version` or `best_version`, `root` is one of `host`, `target`, or
//! `build` selecting `ROOT`, `ESYSROOT`, or `BROOT`, `atom` is the dependency
//! atom, and any trailing tokens are the calling package's resolved `USE`.
//! `has_version` answers `0` on a match and `1` otherwise. `best_version`
//! answers `0` in both cases, with the best matching `cpv` on a value line when
//! it matches and empty output when it does not, mirroring the stock
//! `QueryCommand` exit codes.

use std::collections::HashSet;

use moraine_atom::Atom;
use moraine_common::{Interner, Symbol};
use tracing::instrument;

/// Which root an IPC query targets, mapping to a path variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRoot {
    /// `ROOT`, selected by `-r`/no flag in the stock helper.
    Host,
    /// `ESYSROOT`, selected by `-d`.
    Target,
    /// `BROOT`, selected by `-b`/`--host-root`.
    Build,
}

impl QueryRoot {
    /// Parse the root selector token from a request line.
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "host" | "r" | "-r" => Some(QueryRoot::Host),
            "target" | "d" | "-d" => Some(QueryRoot::Target),
            "build" | "b" | "-b" | "--host-root" => Some(QueryRoot::Build),
            _ => None,
        }
    }
}

/// A parsed IPC query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `has_version <atom>` against a root.
    HasVersion {
        /// The root to query.
        root: QueryRoot,
        /// The dependency atom.
        atom: String,
        /// The calling package's resolved `USE`, for evaluating USE-conditional
        /// dependencies in the atom.
        caller_use: Vec<String>,
    },
    /// `best_version <atom>` against a root.
    BestVersion {
        /// The root to query.
        root: QueryRoot,
        /// The dependency atom.
        atom: String,
        /// The calling package's resolved `USE`, for evaluating USE-conditional
        /// dependencies in the atom.
        caller_use: Vec<String>,
    },
}

/// An IPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The exit code: 0 on a match, 1 otherwise.
    pub code: i32,
    /// The result value (the best version's `cpv`), if any.
    pub value: Option<String>,
}

impl Response {
    /// Render the response as the wire line(s): a code line and an optional value
    /// line.
    pub fn render(&self) -> String {
        match &self.value {
            Some(v) => format!("{}\n{}\n", self.code, v),
            None => format!("{}\n", self.code),
        }
    }
}

/// The query backend the IPC handler answers from. The orchestrator implements
/// this over `moraine-repo` and the installed store; tests substitute a fake.
///
/// `atom` reaches the backend with its USE-conditional dependencies already
/// evaluated against `caller_use`; the caller's `USE` is also passed so a
/// USE-aware store can honor concrete USE requirements.
pub trait VersionQuery: Send + Sync {
    /// Whether any package matching `atom` is installed under `root`.
    fn has_version(&self, root: QueryRoot, atom: &str, caller_use: &[String]) -> bool;

    /// The best matching installed `cpv` under `root`, if any.
    fn best_version(&self, root: QueryRoot, atom: &str, caller_use: &[String]) -> Option<String>;
}

/// Parse a request line into a [`Query`].
///
/// The grammar is `op root atom use...`, where any trailing tokens are the
/// caller's resolved `USE`. Returns `None` for a malformed line.
pub fn parse_request(line: &str) -> Option<Query> {
    let mut parts = line.split_whitespace();
    let op = parts.next()?;
    let root = QueryRoot::parse(parts.next()?)?;
    let atom = parts.next()?.to_string();
    let caller_use: Vec<String> = parts.map(str::to_string).collect();
    match op {
        "has_version" => Some(Query::HasVersion {
            root,
            atom,
            caller_use,
        }),
        "best_version" => Some(Query::BestVersion {
            root,
            atom,
            caller_use,
        }),
        _ => None,
    }
}

/// Resolve an atom's USE-conditional dependencies against the caller's `USE`,
/// returning the atom with its conditional `[...]` group rewritten to concrete
/// USE requirements, reproducing `Atom.evaluate_conditionals(use)`.
///
/// The atom is returned unchanged when it has no USE dependencies or cannot be
/// parsed, leaving the backend to handle it.
fn resolve_atom_use(atom: &str, caller_use: &[String]) -> String {
    let interner = Interner::new();
    let Ok(parsed) = Atom::parse(atom, moraine_eapi::PERMISSIVE, &interner) else {
        return atom.to_string();
    };
    if parsed.use_deps().is_empty() {
        return atom.to_string();
    }
    let parent: HashSet<Symbol> = caller_use.iter().map(|u| interner.intern(u)).collect();
    let reqs = parsed.evaluate_use(&parent);

    let (Some(open), Some(close)) = (atom.find('['), atom.rfind(']')) else {
        return atom.to_string();
    };
    if close < open {
        return atom.to_string();
    }
    let base = &atom[..open];
    let tail = &atom[close + 1..];
    if reqs.is_empty() {
        return format!("{base}{tail}");
    }
    let mut deps = String::new();
    for (i, req) in reqs.iter().enumerate() {
        if i > 0 {
            deps.push(',');
        }
        if !req.enabled {
            deps.push('-');
        }
        if let Some(flag) = interner.resolve(req.flag) {
            deps.push_str(&flag);
        }
    }
    format!("{base}[{deps}]{tail}")
}

/// The manager-side IPC handler.
pub struct IpcHandler<'a, Q: VersionQuery> {
    backend: &'a Q,
}

impl<'a, Q: VersionQuery> IpcHandler<'a, Q> {
    /// Construct a handler over a query backend.
    pub fn new(backend: &'a Q) -> Self {
        IpcHandler { backend }
    }

    /// Answer one parsed query.
    ///
    /// USE-conditional dependencies in the atom are evaluated against the
    /// caller's `USE` before matching. `best_version` returns success with empty
    /// output on no match, matching the stock `QueryCommand`.
    #[instrument(name = "ipc_query", skip(self))]
    pub fn answer(&self, query: &Query) -> Response {
        match query {
            Query::HasVersion {
                root,
                atom,
                caller_use,
            } => {
                let resolved = resolve_atom_use(atom, caller_use);
                let code = if self.backend.has_version(*root, &resolved, caller_use) {
                    0
                } else {
                    1
                };
                Response { code, value: None }
            }
            Query::BestVersion {
                root,
                atom,
                caller_use,
            } => {
                let resolved = resolve_atom_use(atom, caller_use);
                match self.backend.best_version(*root, &resolved, caller_use) {
                    Some(cpv) => Response {
                        code: 0,
                        value: Some(cpv),
                    },
                    None => Response {
                        code: 0,
                        value: None,
                    },
                }
            }
        }
    }

    /// Parse and answer a raw request line, returning the rendered response.
    pub fn handle_line(&self, line: &str) -> Option<Response> {
        parse_request(line).map(|q| self.answer(&q))
    }
}

/// The phases for which the IPC channel is enabled, mirroring the stock enabled
/// set. The merge-time phases are out of scope for this crate.
pub fn ipc_enabled_phase(phase: crate::error::PhaseKind) -> bool {
    use crate::error::PhaseKind::*;
    matches!(phase, PkgSetup | PkgPretend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct FakeStore {
        installed: BTreeMap<QueryRootKey, Vec<String>>,
    }

    type QueryRootKey = u8;

    fn key(root: QueryRoot) -> QueryRootKey {
        match root {
            QueryRoot::Host => 0,
            QueryRoot::Target => 1,
            QueryRoot::Build => 2,
        }
    }

    impl FakeStore {
        fn new(host: &[&str]) -> Self {
            let mut installed = BTreeMap::new();
            installed.insert(
                key(QueryRoot::Host),
                host.iter().map(|s| s.to_string()).collect(),
            );
            FakeStore { installed }
        }
    }

    impl VersionQuery for FakeStore {
        fn has_version(&self, root: QueryRoot, atom: &str, caller_use: &[String]) -> bool {
            self.best_version(root, atom, caller_use).is_some()
        }

        fn best_version(
            &self,
            root: QueryRoot,
            atom: &str,
            _caller_use: &[String],
        ) -> Option<String> {
            // Trivial: match by cp prefix, return the lexically greatest cpv.
            let cp = atom.trim_start_matches(['>', '<', '=', '~', '!']);
            self.installed
                .get(&key(root))
                .into_iter()
                .flatten()
                .filter(|cpv| cpv.starts_with(cp) || cpv.contains(cp))
                .max()
                .cloned()
        }
    }

    /// A backend that records the resolved atom it was asked about, so a test can
    /// assert the USE-conditional evaluation happened against caller USE.
    #[derive(Default)]
    struct RecordingStore {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl VersionQuery for RecordingStore {
        fn has_version(&self, _root: QueryRoot, atom: &str, _caller_use: &[String]) -> bool {
            self.seen.lock().unwrap().push(atom.to_string());
            atom.contains("[bar]")
        }

        fn best_version(
            &self,
            _root: QueryRoot,
            atom: &str,
            _caller_use: &[String],
        ) -> Option<String> {
            self.seen.lock().unwrap().push(atom.to_string());
            None
        }
    }

    #[test]
    fn parses_has_version_request() {
        let q = parse_request("has_version host dev-libs/foo").unwrap();
        assert_eq!(
            q,
            Query::HasVersion {
                root: QueryRoot::Host,
                atom: "dev-libs/foo".into(),
                caller_use: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_caller_use_after_atom() {
        let q = parse_request("has_version host dev-libs/foo ssl threads").unwrap();
        assert_eq!(
            q,
            Query::HasVersion {
                root: QueryRoot::Host,
                atom: "dev-libs/foo".into(),
                caller_use: vec!["ssl".into(), "threads".into()],
            }
        );
    }

    #[test]
    fn use_conditional_atom_evaluated_against_caller_use() {
        // `dev-libs/foo[bar?]` resolves to `[bar]` when the caller has `bar`, so
        // the recording backend (which matches only `[bar]`) succeeds; without
        // `bar` the conditional drops and the backend sees the bare atom.
        let backend = RecordingStore::default();
        let handler = IpcHandler::new(&backend);

        let r = handler
            .handle_line("has_version host dev-libs/foo[bar?] bar")
            .unwrap();
        assert_eq!(r.code, 0);

        let r2 = handler
            .handle_line("has_version host dev-libs/foo[bar?]")
            .unwrap();
        assert_eq!(r2.code, 1);

        let seen = backend.seen.lock().unwrap();
        assert!(seen.iter().any(|a| a == "dev-libs/foo[bar]"));
        assert!(seen.iter().any(|a| a == "dev-libs/foo"));
    }

    #[test]
    fn best_version_no_match_returns_zero_empty() {
        let store = FakeStore::new(&["dev-libs/foo-1.0"]);
        let handler = IpcHandler::new(&store);
        let r = handler
            .handle_line("best_version host dev-libs/absent")
            .unwrap();
        assert_eq!(r.code, 0);
        assert_eq!(r.value, None);
        assert_eq!(r.render(), "0\n");
    }

    #[test]
    fn root_flag_mapping() {
        assert_eq!(QueryRoot::parse("-d"), Some(QueryRoot::Target));
        assert_eq!(QueryRoot::parse("--host-root"), Some(QueryRoot::Build));
        assert_eq!(QueryRoot::parse("host"), Some(QueryRoot::Host));
        assert_eq!(QueryRoot::parse("nope"), None);
    }

    #[test]
    fn has_version_answered() {
        let store = FakeStore::new(&["dev-libs/foo-1.0"]);
        let handler = IpcHandler::new(&store);
        let r = handler
            .handle_line("has_version host dev-libs/foo")
            .unwrap();
        assert_eq!(r.code, 0);
        let r2 = handler
            .handle_line("has_version host dev-libs/absent")
            .unwrap();
        assert_eq!(r2.code, 1);
    }

    #[test]
    fn best_version_returns_match() {
        let store = FakeStore::new(&["dev-libs/foo-1.0", "dev-libs/foo-2.0"]);
        let handler = IpcHandler::new(&store);
        let r = handler
            .handle_line("best_version host dev-libs/foo")
            .unwrap();
        assert_eq!(r.code, 0);
        assert_eq!(r.value.as_deref(), Some("dev-libs/foo-2.0"));
        assert!(r.render().contains("dev-libs/foo-2.0"));
    }

    #[test]
    fn malformed_request_ignored() {
        let store = FakeStore::new(&[]);
        let handler = IpcHandler::new(&store);
        assert!(handler.handle_line("garbage").is_none());
        assert!(handler.handle_line("has_version").is_none());
    }

    #[test]
    fn ipc_enabled_only_for_setup_phases() {
        use crate::error::PhaseKind;
        assert!(ipc_enabled_phase(PhaseKind::PkgSetup));
        assert!(!ipc_enabled_phase(PhaseKind::SrcCompile));
    }
}
