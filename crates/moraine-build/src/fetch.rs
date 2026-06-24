//! Source acquisition: mirror resolution, distfile fetch, and verification.
//!
//! The engine fetches each distfile by shelling out to a configurable
//! `FETCHCOMMAND`/`RESUMECOMMAND` through the injectable [`CommandRunner`], never
//! through an in-process HTTP client. It resolves `mirror://` URIs against the
//! configured mirror lists, prepends `GENTOO_MIRRORS` for mirrorable files,
//! skips already-valid files, resumes partial downloads above a threshold,
//! verifies every fetched file against the repository `Manifest`, and honors
//! `RESTRICT=fetch`/`mirror`/`nofetch`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::{instrument, warn};

use crate::error::{BuildError, IoExt as _, Result};
use crate::manifest::{self, Manifest, VerifyOutcome};
use crate::runner::{CommandRunner, CommandSpec};
use crate::srcuri::DistFile;

/// The package-level fetch restrictions parsed from `RESTRICT`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestrictFlags {
    /// `RESTRICT=fetch`: no public-mirror fetch for the package.
    pub fetch: bool,
    /// `RESTRICT=mirror`: do not use the Gentoo mirror network.
    pub mirror: bool,
}

impl RestrictFlags {
    /// Parse the relevant tokens out of a `RESTRICT` token list.
    pub fn from_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut flags = RestrictFlags::default();
        for tok in tokens {
            match tok {
                "fetch" => flags.fetch = true,
                "mirror" => flags.mirror = true,
                _ => {}
            }
        }
        flags
    }
}

/// Fetch configuration resolved from `moraine-config`.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// The distfile directory.
    pub distdir: PathBuf,
    /// The `FETCHCOMMAND` template, tokenized into program and arguments.
    /// `${URI}` and `${DISTDIR}` and `${FILE}` placeholders are substituted.
    pub fetchcommand: Vec<String>,
    /// The `RESUMECOMMAND` template, used for partial downloads.
    pub resumecommand: Vec<String>,
    /// The configured `GENTOO_MIRRORS` base URIs.
    pub mirrors: Vec<String>,
    /// Named third-party mirror groups (`mirror://name/...`) to their base URIs.
    pub thirdparty: BTreeMap<String, Vec<String>>,
    /// The minimum partial-file size to resume rather than restart, in bytes.
    pub resume_min_size: u64,
    /// The maximum number of fetch attempts per distfile across all sources.
    pub max_attempts: u32,
}

impl FetchConfig {
    /// A config that downloads to `distdir` with a wget-style fetch command and
    /// stock defaults.
    pub fn new(distdir: impl Into<PathBuf>) -> Self {
        FetchConfig {
            distdir: distdir.into(),
            fetchcommand: vec![
                "wget".into(),
                "-O".into(),
                "${DISTDIR}/${FILE}".into(),
                "${URI}".into(),
            ],
            resumecommand: vec![
                "wget".into(),
                "-c".into(),
                "-O".into(),
                "${DISTDIR}/${FILE}".into(),
                "${URI}".into(),
            ],
            mirrors: Vec::new(),
            thirdparty: BTreeMap::new(),
            resume_min_size: 350_000,
            max_attempts: 3,
        }
    }
}

/// Whether a distfile was fetched, already present, or restricted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchStatus {
    /// Already present in the distdir and verified; not re-fetched.
    AlreadyPresent,
    /// Fetched and verified.
    Fetched,
    /// Fetch-restricted and present (manually provided), verified.
    RestrictedPresent,
}

/// The result of fetching one distfile.
#[derive(Debug, Clone)]
pub struct FetchedFile {
    /// The distfile name.
    pub name: String,
    /// The final path in the distdir.
    pub path: PathBuf,
    /// How it was obtained.
    pub status: FetchStatus,
}

/// Fetches and verifies distfiles through an injected command runner.
pub struct Fetcher<'a, R: CommandRunner> {
    runner: &'a R,
    config: &'a FetchConfig,
    manifest: &'a Manifest,
    require_digest: bool,
}

impl<'a, R: CommandRunner> Fetcher<'a, R> {
    /// Construct a fetcher. When `require_digest` is set, a missing Manifest DIST
    /// entry is a hard error; otherwise a file without a digest is accepted as
    /// long as it is non-empty (matching `FEATURES=digest` tolerance).
    pub fn new(
        runner: &'a R,
        config: &'a FetchConfig,
        manifest: &'a Manifest,
        require_digest: bool,
    ) -> Self {
        Fetcher {
            runner,
            config,
            manifest,
            require_digest,
        }
    }

    /// Fetch and verify every distfile in `files`, honoring the package restrict
    /// flags. Returns the fetched files on success, or the first failure.
    #[instrument(name = "fetch_all", skip_all, fields(count = files.len()))]
    pub fn fetch_all(
        &self,
        files: &[&DistFile],
        restrict: RestrictFlags,
    ) -> Result<Vec<FetchedFile>> {
        let mut out = Vec::with_capacity(files.len());
        for file in files {
            out.push(self.fetch_one(file, restrict)?);
        }
        Ok(out)
    }

    /// Fetch and verify a single distfile.
    #[instrument(name = "fetch_one", skip(self, restrict), fields(distfile = %file.name))]
    pub fn fetch_one(&self, file: &DistFile, restrict: RestrictFlags) -> Result<FetchedFile> {
        let dest = self.config.distdir.join(&file.name);
        let entry = self.manifest.dist(&file.name);

        if entry.is_none() && self.require_digest {
            return Err(BuildError::MissingDigest {
                distfile: file.name.clone(),
            });
        }

        // An already-present, valid file is never re-fetched.
        if dest.exists() {
            if let Some(entry) = entry {
                match manifest::verify_file(entry, &dest)? {
                    VerifyOutcome::Ok => {
                        return Ok(FetchedFile {
                            name: file.name.clone(),
                            path: dest,
                            status: FetchStatus::AlreadyPresent,
                        });
                    }
                    other => {
                        warn!(reason = %other.reason(), "present distfile failed verification; refetching");
                        self.move_aside(&dest)?;
                    }
                }
            } else if file_len(&dest)? > 0 {
                return Ok(FetchedFile {
                    name: file.name.clone(),
                    path: dest,
                    status: FetchStatus::AlreadyPresent,
                });
            }
        }

        // Fetch-restricted: never fetch from a public mirror.
        let restricted = restrict.fetch || file.fetch_restricted;
        if restricted {
            if dest.exists() && self.verify_or_ok(entry, &dest)? {
                return Ok(FetchedFile {
                    name: file.name.clone(),
                    path: dest,
                    status: FetchStatus::RestrictedPresent,
                });
            }
            return Err(BuildError::RestrictedFetch {
                distfile: file.name.clone(),
            });
        }

        self.fetch_from_sources(file, entry, restrict)
    }

    fn fetch_from_sources(
        &self,
        file: &DistFile,
        entry: Option<&manifest::DistEntry>,
        restrict: RestrictFlags,
    ) -> Result<FetchedFile> {
        let dest = self.config.distdir.join(&file.name);
        let sources = self.resolve_sources(file, restrict);
        let mut attempts = 0u32;

        for uri in &sources {
            if attempts >= self.config.max_attempts {
                break;
            }
            attempts += 1;
            self.run_fetch(uri, &file.name, &dest)?;

            if !dest.exists() {
                continue;
            }
            match entry {
                Some(entry) => match manifest::verify_file(entry, &dest)? {
                    VerifyOutcome::Ok => {
                        return Ok(FetchedFile {
                            name: file.name.clone(),
                            path: dest,
                            status: FetchStatus::Fetched,
                        });
                    }
                    outcome => {
                        warn!(uri, reason = %outcome.reason(), "verification failed; trying next source");
                        self.move_aside(&dest)?;
                    }
                },
                None => {
                    if file_len(&dest)? > 0 {
                        return Ok(FetchedFile {
                            name: file.name.clone(),
                            path: dest,
                            status: FetchStatus::Fetched,
                        });
                    }
                    self.move_aside(&dest)?;
                }
            }
        }

        // If we exhausted sources but did get a file that has no digest to check
        // and digests are not required, accept it.
        if entry.is_none() && dest.exists() && file_len(&dest)? > 0 {
            return Ok(FetchedFile {
                name: file.name.clone(),
                path: dest,
                status: FetchStatus::Fetched,
            });
        }

        if let Some(entry) = entry
            && dest.exists()
        {
            let outcome = manifest::verify_file(entry, &dest)?;
            if !outcome.is_ok() {
                return Err(BuildError::Verification {
                    distfile: file.name.clone(),
                    reason: outcome.reason(),
                });
            }
        }

        Err(BuildError::Fetch {
            distfile: file.name.clone(),
            attempts,
        })
    }

    /// Resolve the ordered source URIs for a distfile: explicit `mirror://`
    /// resolution, then the Gentoo mirror network for a mirrorable file, then the
    /// upstream URIs.
    fn resolve_sources(&self, file: &DistFile, restrict: RestrictFlags) -> Vec<String> {
        let mut out = Vec::new();
        let mirrorable = !restrict.mirror && !file.mirror_restricted;

        for uri in &file.uris {
            if let Some(rest) = uri.strip_prefix("mirror://") {
                out.extend(self.expand_mirror(rest));
            } else {
                out.push(uri.clone());
            }
        }

        if mirrorable {
            // Prepend the configured Gentoo mirrors as candidate sources for the
            // file's basename.
            let mut mirrored = Vec::new();
            for base in &self.config.mirrors {
                mirrored.push(format!(
                    "{}/distfiles/{}",
                    base.trim_end_matches('/'),
                    file.name
                ));
            }
            mirrored.extend(out);
            mirrored
        } else {
            out
        }
    }

    /// Expand a `mirror://group/path` reference against the named third-party
    /// mirror list.
    fn expand_mirror(&self, rest: &str) -> Vec<String> {
        let (group, path) = match rest.split_once('/') {
            Some((g, p)) => (g, p),
            None => (rest, ""),
        };
        match self.config.thirdparty.get(group) {
            Some(bases) => bases
                .iter()
                .map(|b| format!("{}/{}", b.trim_end_matches('/'), path))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Run the fetch (or resume) command for a single URI.
    fn run_fetch(&self, uri: &str, file: &str, dest: &Path) -> Result<()> {
        let resume = dest.exists() && file_len(dest)? >= self.config.resume_min_size;
        let template = if resume {
            &self.config.resumecommand
        } else {
            &self.config.fetchcommand
        };
        let spec = self.fetch_command(template, uri, file);
        // A launch failure is treated as a failed attempt so other sources can
        // still be tried, rather than aborting the whole fetch.
        match self.runner.run(&spec) {
            Ok(output) if !output.success() => {
                warn!(
                    uri,
                    status = output.status,
                    "fetch command returned non-zero"
                );
            }
            Ok(_) => {}
            Err(err) => {
                warn!(uri, reason = %err.reason, "could not launch fetch command");
            }
        }
        Ok(())
    }

    /// Substitute the `${URI}`, `${FILE}`, and `${DISTDIR}` placeholders in a
    /// command template.
    fn fetch_command(&self, template: &[String], uri: &str, file: &str) -> CommandSpec {
        let distdir = self.config.distdir.to_string_lossy().to_string();
        let subst = |s: &str| -> String {
            s.replace("${URI}", uri)
                .replace("${FILE}", file)
                .replace("${DISTDIR}", &distdir)
        };
        let program = template.first().map(|s| subst(s)).unwrap_or_default();
        let args = template
            .iter()
            .skip(1)
            .map(|s| subst(s))
            .collect::<Vec<_>>();
        CommandSpec::new(program, &self.config.distdir).args(args)
    }

    fn verify_or_ok(&self, entry: Option<&manifest::DistEntry>, path: &Path) -> Result<bool> {
        match entry {
            Some(entry) => Ok(manifest::verify_file(entry, path)?.is_ok()),
            None => Ok(file_len(path)? > 0),
        }
    }

    /// Rename a bad distfile aside so a retry starts fresh.
    fn move_aside(&self, path: &Path) -> Result<()> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push("._bad_");
        let aside = path.with_file_name(name);
        let _ = std::fs::remove_file(&aside);
        std::fs::rename(path, &aside).at(path)?;
        Ok(())
    }
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path).at(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;
    use crate::runner::testing::{FakeRunner, Response};
    use crate::srcuri::DistFile;
    use std::collections::BTreeMap;

    fn distfile(name: &str, uris: &[&str]) -> DistFile {
        DistFile {
            name: name.to_string(),
            uris: uris.iter().map(|s| s.to_string()).collect(),
            fetch_restricted: false,
            mirror_restricted: false,
        }
    }

    fn manifest_for(name: &str, data: &[u8]) -> Manifest {
        let mut hashes = BTreeMap::new();
        hashes.insert("BLAKE2B".to_string(), moraine_common::hash::blake2b(data));
        hashes.insert("SHA512".to_string(), moraine_common::hash::sha512(data));
        let text = format!(
            "DIST {name} {} BLAKE2B {} SHA512 {}\n",
            data.len(),
            hashes["BLAKE2B"],
            hashes["SHA512"],
        );
        Manifest::parse(&text)
    }

    #[test]
    fn already_present_valid_not_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"hello world";
        std::fs::write(dir.path().join("f.tar.gz"), data).unwrap();
        let cfg = FetchConfig::new(dir.path());
        let mani = manifest_for("f.tar.gz", data);
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let f = fetcher
            .fetch_one(
                &distfile("f.tar.gz", &["https://x/f.tar.gz"]),
                RestrictFlags::default(),
            )
            .unwrap();
        assert_eq!(f.status, FetchStatus::AlreadyPresent);
        assert_eq!(runner.call_count(), 0);
    }

    #[test]
    fn fetch_writes_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"downloaded bytes";
        let mani = manifest_for("f.tar.gz", data);
        let cfg = FetchConfig::new(dir.path());
        let dest = dir.path().join("f.tar.gz");
        let runner = FakeRunner::default();
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: data.to_vec(),
        });
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let f = fetcher
            .fetch_one(
                &distfile("f.tar.gz", &["https://x/f.tar.gz"]),
                RestrictFlags::default(),
            )
            .unwrap();
        assert_eq!(f.status, FetchStatus::Fetched);
        assert_eq!(runner.call_count(), 1);
    }

    #[test]
    fn bad_digest_retries_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"the correct bytes";
        let mani = manifest_for("f.tar.gz", good);
        let cfg = FetchConfig::new(dir.path());
        let dest = dir.path().join("f.tar.gz");
        let runner = FakeRunner::default();
        // First source writes corrupt bytes, second writes the correct file.
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: b"wrong".to_vec(),
        });
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: good.to_vec(),
        });
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let file = distfile("f.tar.gz", &["https://a/f.tar.gz", "https://b/f.tar.gz"]);
        let f = fetcher.fetch_one(&file, RestrictFlags::default()).unwrap();
        assert_eq!(f.status, FetchStatus::Fetched);
        assert_eq!(runner.call_count(), 2);
        // The bad file was moved aside.
        assert!(dir.path().join("f.tar.gz._bad_").exists());
    }

    #[test]
    fn launch_failure_falls_through_to_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"correct payload";
        let mani = manifest_for("f.tar.gz", good);
        let cfg = FetchConfig::new(dir.path());
        let dest = dir.path().join("f.tar.gz");
        let runner = FakeRunner::default();
        // First source's fetch command cannot launch; the second succeeds.
        runner.push(Response::Fail("wget not found".into()));
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: good.to_vec(),
        });
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let file = distfile("f.tar.gz", &["https://a/f.tar.gz", "https://b/f.tar.gz"]);
        let f = fetcher.fetch_one(&file, RestrictFlags::default()).unwrap();
        assert_eq!(f.status, FetchStatus::Fetched);
        assert_eq!(runner.call_count(), 2);
    }

    #[test]
    fn all_sources_bad_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"the correct bytes";
        let mani = manifest_for("f.tar.gz", good);
        let mut cfg = FetchConfig::new(dir.path());
        cfg.max_attempts = 2;
        let dest = dir.path().join("f.tar.gz");
        let runner = FakeRunner::default();
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: b"bad1".to_vec(),
        });
        runner.push(Response::WriteFile {
            status: 0,
            path: dest.clone(),
            contents: b"bad2".to_vec(),
        });
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let file = distfile("f.tar.gz", &["https://a/f.tar.gz", "https://b/f.tar.gz"]);
        let err = fetcher.fetch_one(&file, RestrictFlags::default());
        assert!(matches!(err, Err(BuildError::Fetch { .. })));
    }

    #[test]
    fn missing_digest_is_hard_error_when_required() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FetchConfig::new(dir.path());
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let err = fetcher.fetch_one(&distfile("f", &["https://x/f"]), RestrictFlags::default());
        assert!(matches!(err, Err(BuildError::MissingDigest { .. })));
    }

    #[test]
    fn fetch_restricted_missing_reports_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"abc";
        let cfg = FetchConfig::new(dir.path());
        let mani = manifest_for("f.tar.gz", data);
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let restrict = RestrictFlags {
            fetch: true,
            mirror: false,
        };
        let err = fetcher.fetch_one(&distfile("f.tar.gz", &["https://x/f.tar.gz"]), restrict);
        assert!(matches!(err, Err(BuildError::RestrictedFetch { .. })));
        // No public fetch attempted.
        assert_eq!(runner.call_count(), 0);
    }

    #[test]
    fn fetch_restricted_present_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"manual download";
        std::fs::write(dir.path().join("f.tar.gz"), data).unwrap();
        let cfg = FetchConfig::new(dir.path());
        let mani = manifest_for("f.tar.gz", data);
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let restrict = RestrictFlags {
            fetch: true,
            mirror: false,
        };
        let f = fetcher
            .fetch_one(&distfile("f.tar.gz", &["https://x/f.tar.gz"]), restrict)
            .unwrap();
        // Already-present check runs before the restrict gate, so it reports
        // AlreadyPresent.
        assert!(matches!(
            f.status,
            FetchStatus::AlreadyPresent | FetchStatus::RestrictedPresent
        ));
    }

    #[test]
    fn mirror_uri_resolved_against_lists() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        cfg.thirdparty.insert(
            "gnu".to_string(),
            vec!["https://ftp.gnu.org/gnu".to_string()],
        );
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        let file = distfile("foo.tar.gz", &["mirror://gnu/foo/foo.tar.gz"]);
        let sources = fetcher.resolve_sources(&file, RestrictFlags::default());
        assert!(
            sources
                .iter()
                .any(|s| s == "https://ftp.gnu.org/gnu/foo/foo.tar.gz")
        );
    }

    #[test]
    fn mirror_restricted_skips_gentoo_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        cfg.mirrors = vec!["https://mirror.example".to_string()];
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        let mut file = distfile("foo.tar.gz", &["https://upstream/foo.tar.gz"]);
        file.mirror_restricted = true;
        let sources = fetcher.resolve_sources(&file, RestrictFlags::default());
        assert!(!sources.iter().any(|s| s.contains("mirror.example")));
        assert_eq!(sources, vec!["https://upstream/foo.tar.gz"]);
    }

    #[test]
    fn restrict_flags_parse() {
        let r = RestrictFlags::from_tokens(["fetch", "mirror", "test"]);
        assert!(r.fetch && r.mirror);
        let r2 = RestrictFlags::from_tokens(["strip"]);
        assert!(!r2.fetch && !r2.mirror);
    }
}
