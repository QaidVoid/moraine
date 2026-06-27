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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use moraine_atom::Atom;
use moraine_common::{Interner, Symbol};
use tracing::instrument;

use crate::error::{BuildError, IoExt as _, Result};

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

    /// Render the response as a fixed two-line wire frame: a code line followed
    /// by a value line that is empty when there is no value.
    ///
    /// The framing is fixed so the ebuild-side client can read both lines in a
    /// single open of the response FIFO and never block waiting for a second
    /// line that the variable [`Response::render`] form may not emit.
    pub fn render_framed(&self) -> String {
        format!("{}\n{}\n", self.code, self.value.as_deref().unwrap_or(""))
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

    /// Whether `atom` is malformed and cannot be parsed as a dependency atom.
    ///
    /// The handler answers an invalid atom with the exit-code `2` (invalid atom)
    /// contract the bash `has_version`/`best_version` wrapper expects, mirroring
    /// the `InvalidAtom` branch of the stock `QueryCommand`. The default returns
    /// `false`, so a backend that does not validate atoms never produces the
    /// invalid case.
    fn invalid_atom(&self, atom: &str) -> bool {
        let _ = atom;
        false
    }
}

/// Parse a request line into a [`Query`].
///
/// The grammar is `op root atom use...`, where any trailing tokens are the
/// caller's resolved `USE`. The bash wrapper resolves the root flag to a
/// concrete path before relaying it, so a root token that is not a recognized
/// selector falls back to the host store. Returns `None` only when the op or
/// atom is missing.
pub fn parse_request(line: &str) -> Option<Query> {
    let mut parts = line.split_whitespace();
    let op = parts.next()?;
    let root = QueryRoot::parse(parts.next()?).unwrap_or(QueryRoot::Host);
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
pub struct IpcHandler<'a, Q: VersionQuery + ?Sized> {
    backend: &'a Q,
}

impl<'a, Q: VersionQuery + ?Sized> IpcHandler<'a, Q> {
    /// Construct a handler over a query backend.
    pub fn new(backend: &'a Q) -> Self {
        IpcHandler { backend }
    }

    /// Answer one parsed query.
    ///
    /// USE-conditional dependencies in the atom are evaluated against the
    /// caller's `USE` before matching. A malformed atom answers with code `2`
    /// (invalid atom). `best_version` returns success with empty output on no
    /// match, matching the stock `QueryCommand`.
    #[instrument(name = "ipc_query", skip(self))]
    pub fn answer(&self, query: &Query) -> Response {
        let (root, atom, caller_use) = match query {
            Query::HasVersion {
                root,
                atom,
                caller_use,
            }
            | Query::BestVersion {
                root,
                atom,
                caller_use,
            } => (root, atom, caller_use),
        };
        let resolved = resolve_atom_use(atom, caller_use);
        if self.backend.invalid_atom(&resolved) {
            return Response {
                code: 2,
                value: None,
            };
        }
        match query {
            Query::HasVersion { .. } => {
                let code = if self.backend.has_version(*root, &resolved, caller_use) {
                    0
                } else {
                    1
                };
                Response { code, value: None }
            }
            Query::BestVersion { .. } => Response {
                code: 0,
                value: self.backend.best_version(*root, &resolved, caller_use),
            },
        }
    }

    /// Parse and answer a raw request line, returning the rendered response.
    pub fn handle_line(&self, line: &str) -> Option<Response> {
        parse_request(line).map(|q| self.answer(&q))
    }
}

/// The phases for which the IPC channel is enabled: every build phase that runs
/// in the build directory, namely the source-build phases together with
/// `pkg_setup` and `pkg_pretend`.
///
/// This mirrors `AbstractEbuildProcess._enable_ipc_daemon`, which starts the
/// daemon for every phase that has a build directory rather than only `pkg_setup`
/// and `pkg_pretend`. `pkg_nofetch` runs outside the normal schedule and is the
/// only build phase left out.
pub fn ipc_enabled_phase(phase: crate::error::PhaseKind) -> bool {
    use crate::error::PhaseKind::*;
    matches!(
        phase,
        PkgPretend
            | PkgSetup
            | SrcUnpack
            | SrcPrepare
            | SrcConfigure
            | SrcCompile
            | SrcTest
            | SrcInstall
    )
}

/// The sentinel request line [`IpcEndpoint::shutdown`] writes to wake the blocked
/// responder so it can stop.
const SHUTDOWN_LINE: &str = "__moraine_ipc_shutdown__";

/// A live ebuild IPC endpoint: a request FIFO, a response FIFO, and the
/// ebuild-side client script the driver exports as `MORAINE_IPC_HELPER`.
///
/// This mirrors Portage's split of a manager-side daemon
/// (`lib/_emerge/EbuildIpcDaemon.py`) and a thin ebuild-side client
/// (`bin/ebuild-ipc.py`), kept minimal. [`IpcEndpoint::serve`] answers one
/// request at a time from a [`VersionQuery`] backend; the generated client
/// relays the `op root atom use...` request to the FIFOs and exits with the
/// answer's code, printing the `best_version` `cpv` on stdout.
pub struct IpcEndpoint {
    request: PathBuf,
    response: PathBuf,
    helper: PathBuf,
}

impl IpcEndpoint {
    /// Create the request/response FIFOs and the client script under `ipc_dir`
    /// (the build's `.ipc` directory).
    ///
    /// The client is written executable; its path is what the driver exports as
    /// `MORAINE_IPC_HELPER`.
    pub fn create(ipc_dir: &Path) -> Result<Self> {
        let request = ipc_dir.join("request");
        let response = ipc_dir.join("response");
        let helper = ipc_dir.join("helper");
        make_fifo(&request)?;
        make_fifo(&response)?;
        write_client(&helper, &request, &response)?;
        Ok(IpcEndpoint {
            request,
            response,
            helper,
        })
    }

    /// The client path to export into the phase environment as
    /// `MORAINE_IPC_HELPER`.
    pub fn helper_path(&self) -> &Path {
        &self.helper
    }

    /// Answer queries from `backend` until [`IpcEndpoint::shutdown`] is called.
    ///
    /// Blocks the calling thread, so run it on a dedicated thread for the
    /// lifetime of the phases. Each request is read as one line, answered through
    /// [`IpcHandler::handle_line`], and written back as a fixed two-line frame.
    /// A request that cannot be parsed answers with code `2` (invalid atom).
    pub fn serve(&self, backend: &dyn VersionQuery) {
        let handler = IpcHandler::new(backend);
        loop {
            let Some(line) = read_line(&self.request) else {
                continue;
            };
            if line == SHUTDOWN_LINE {
                break;
            }
            let response = handler.handle_line(&line).unwrap_or(Response {
                code: 2,
                value: None,
            });
            write_frame(&self.response, &response.render_framed());
        }
    }

    /// Signal the [`IpcEndpoint::serve`] loop to stop, unblocking it if it is
    /// waiting for a request.
    pub fn shutdown(&self) {
        write_frame(&self.request, &format!("{SHUTDOWN_LINE}\n"));
    }
}

/// Create a FIFO at `path`, removing any stale node from a previous build first.
fn make_fifo(path: &Path) -> Result<()> {
    use rustix::fs::{CWD, Mode, mkfifoat};
    let _ = std::fs::remove_file(path);
    mkfifoat(CWD, path, Mode::RUSR | Mode::WUSR).map_err(|e| BuildError::Ipc {
        reason: format!("could not create IPC fifo {}: {e}", path.display()),
    })
}

/// Write the ebuild-side client script, relaying its arguments to the request
/// FIFO and reading the two-line answer frame from the response FIFO. The FIFO
/// paths are baked in so only `MORAINE_IPC_HELPER` needs exporting.
fn write_client(helper: &Path, request: &Path, response: &Path) -> Result<()> {
    let script = format!(
        "#!/usr/bin/env bash\n\
         # moraine ebuild IPC client: relay has_version/best_version to the\n\
         # manager responder over the build FIFOs and exit with its code.\n\
         printf '%s\\n' \"$*\" > {req}\n\
         {{ IFS= read -r __moraine_code; IFS= read -r __moraine_value; }} < {resp}\n\
         [[ -n ${{__moraine_value}} ]] && printf '%s\\n' \"${{__moraine_value}}\"\n\
         exit \"${{__moraine_code:-2}}\"\n",
        req = sh_quote(&request.to_string_lossy()),
        resp = sh_quote(&response.to_string_lossy()),
    );
    std::fs::write(helper, script).at(helper)?;
    std::fs::set_permissions(helper, std::fs::Permissions::from_mode(0o755)).at(helper)?;
    Ok(())
}

/// Single-quote a string for safe inclusion in the generated bash client.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// Read one request line from a FIFO, blocking until a writer connects. Returns
/// `None` on an empty read (a writer that opened and closed without data) or an
/// I/O error, so the caller can loop and reopen.
fn read_line(fifo: &Path) -> Option<String> {
    let file = std::fs::File::open(fifo).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Write a response frame to a FIFO, blocking until a reader connects. A failure
/// to open or write is dropped: the client side reports the missing answer.
fn write_frame(fifo: &Path, frame: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(fifo) {
        let _ = file.write_all(frame.as_bytes());
    }
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
    fn ipc_enabled_for_build_phases() {
        use crate::error::PhaseKind;
        // pkg_setup/pkg_pretend and every source-build phase are enabled.
        assert!(ipc_enabled_phase(PhaseKind::PkgPretend));
        assert!(ipc_enabled_phase(PhaseKind::PkgSetup));
        assert!(ipc_enabled_phase(PhaseKind::SrcConfigure));
        assert!(ipc_enabled_phase(PhaseKind::SrcCompile));
        assert!(ipc_enabled_phase(PhaseKind::SrcInstall));
        // pkg_nofetch runs outside the schedule and stays disabled.
        assert!(!ipc_enabled_phase(PhaseKind::PkgNofetch));
    }

    #[test]
    fn invalid_atom_answers_code_two() {
        struct Invalid;
        impl VersionQuery for Invalid {
            fn has_version(&self, _root: QueryRoot, _atom: &str, _use: &[String]) -> bool {
                true
            }
            fn best_version(
                &self,
                _root: QueryRoot,
                _atom: &str,
                _use: &[String],
            ) -> Option<String> {
                Some("dev-libs/foo-1".into())
            }
            fn invalid_atom(&self, atom: &str) -> bool {
                atom.contains('[')
            }
        }
        let handler = IpcHandler::new(&Invalid);
        let r = handler
            .handle_line("has_version host dev-libs/foo[")
            .unwrap();
        assert_eq!(r.code, 2);
        assert_eq!(r.value, None);
        // A valid atom still answers normally.
        let ok = handler
            .handle_line("has_version host dev-libs/foo")
            .unwrap();
        assert_eq!(ok.code, 0);
    }

    #[test]
    fn framed_response_is_two_lines() {
        let with = Response {
            code: 0,
            value: Some("dev-libs/foo-2.0".into()),
        };
        assert_eq!(with.render_framed(), "0\ndev-libs/foo-2.0\n");
        let without = Response {
            code: 1,
            value: None,
        };
        assert_eq!(without.render_framed(), "1\n\n");
    }
}
