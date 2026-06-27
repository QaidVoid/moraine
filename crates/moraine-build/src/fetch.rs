//! Source acquisition: mirror resolution, distfile fetch, and verification.
//!
//! The engine fetches each distfile by shelling out to a configurable
//! `FETCHCOMMAND`/`RESUMECOMMAND` through the injectable [`CommandRunner`], never
//! through an in-process HTTP client. It resolves `mirror://` URIs against the
//! configured mirror lists, prepends `GENTOO_MIRRORS` for mirrorable files,
//! skips already-valid files, resumes partial downloads above a threshold,
//! verifies every fetched file against the repository `Manifest`, and honors
//! `RESTRICT=fetch`/`mirror`/`nofetch`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{instrument, warn};

/// The number of consecutive checksum failures after which the upstream
/// `SRC_URI` URIs are escalated ahead of the remaining mirrors, matching
/// Portage's `checksum_failure_primaryuri`.
const CHECKSUM_FAILURE_PRIMARYURI: u32 = 2;

/// The maximum age of a cached per-mirror layout before it is re-resolved.
const MIRROR_LAYOUT_TTL_SECS: u64 = 24 * 60 * 60;

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
    /// `RESTRICT=primaryuri`: try the upstream `SRC_URI` hosts before mirrors.
    pub primaryuri: bool,
}

impl RestrictFlags {
    /// Parse the relevant tokens out of a `RESTRICT` token list.
    pub fn from_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut flags = RestrictFlags::default();
        for tok in tokens {
            match tok {
                "fetch" => flags.fetch = true,
                // The deprecated `nomirror` is treated as `mirror`, matching
                // `package/ebuild/fetch.py`.
                "mirror" | "nomirror" => flags.mirror = true,
                "primaryuri" => flags.primaryuri = true,
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
    /// Retained as a transport-failure backstop; checksum failures are bounded
    /// separately by [`checksum_try_mirrors`](Self::checksum_try_mirrors).
    pub max_attempts: u32,
    /// The hashes a verified distfile must carry and match, selected from the
    /// distfile's owning repository's `manifest-required-hashes` policy rather
    /// than a global union across repositories. Defaults to `{BLAKE2B, SHA512}`.
    pub required_hashes: BTreeSet<String>,
    /// Protocol-specific `FETCHCOMMAND_<PROTO>` templates keyed by lowercase
    /// scheme (`http`, `https`, `ftp`, `ssh`, ...); the generic
    /// [`fetchcommand`](Self::fetchcommand) is the fallback.
    pub fetchcommand_proto: BTreeMap<String, Vec<String>>,
    /// Protocol-specific `RESUMECOMMAND_<PROTO>` templates.
    pub resumecommand_proto: BTreeMap<String, Vec<String>>,
    /// `PORTAGE_SSH_OPTS`, exposed to the fetch command as `${PORTAGE_SSH_OPTS}`.
    pub ssh_opts: String,
    /// The maximum number of consecutive checksum/verification failures before
    /// giving up (Portage's `PORTAGE_FETCH_CHECKSUM_TRY_MIRRORS`, default 5).
    pub checksum_try_mirrors: u32,
    /// `FEATURES=distlocks`: take a per-distfile lock around fetch/verify/rename.
    pub distlocks: bool,
    /// `PORTAGE_RO_DISTDIRS`: read-only distfile directories checked (and
    /// symlinked from) before downloading.
    pub ro_distdirs: Vec<PathBuf>,
    /// Custom mirror tiers from `CUSTOM_MIRRORS_FILE`, preferred before the
    /// public Gentoo mirrors.
    pub custom_mirrors: CustomMirrors,
}

/// The custom-mirror tiers from `CUSTOM_MIRRORS_FILE`.
#[derive(Debug, Clone, Default)]
pub struct CustomMirrors {
    /// Local mirror base URIs, tried first and allowed under `RESTRICT=fetch`.
    pub local: Vec<String>,
    /// Public custom mirror base URIs.
    pub public: Vec<String>,
    /// Filesystem mirror directories: a verified match is copied in.
    pub filesystem: Vec<PathBuf>,
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
            required_hashes: ["BLAKE2B", "SHA512"]
                .into_iter()
                .map(String::from)
                .collect(),
            fetchcommand_proto: BTreeMap::new(),
            resumecommand_proto: BTreeMap::new(),
            ssh_opts: String::new(),
            checksum_try_mirrors: 5,
            distlocks: false,
            ro_distdirs: Vec::new(),
            custom_mirrors: CustomMirrors::default(),
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
                match manifest::verify_file(entry, &dest, &self.config.required_hashes)? {
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
            // A read-only distdir or filesystem mirror copy is allowed.
            if let Some(found) = self.adopt_local_copy(file, entry)? {
                return Ok(found);
            }
            // Local custom mirrors are explicitly permitted under RESTRICT=fetch.
            if !self.config.custom_mirrors.local.is_empty()
                && let Ok(found) = self.fetch_from_local_mirrors(file, entry)
            {
                return Ok(found);
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
        // A read-only distdir or filesystem mirror that already holds a verified
        // copy is linked/copied in instead of downloading.
        if let Some(found) = self.adopt_local_copy(file, entry)? {
            return Ok(found);
        }

        // Hold a per-distfile lock for the whole fetch/verify/rename when
        // FEATURES=distlocks is set.
        let _lock = if self.config.distlocks {
            Some(self.distlock(&file.name)?)
        } else {
            None
        };

        let sources = self.resolve_sources(file, restrict);
        // The upstream URIs are passed for checksum-failure escalation; under
        // `primaryuri` they are already at the front and no escalation is needed.
        let upstream = if restrict.primaryuri {
            Vec::new()
        } else {
            self.upstream_uris(file)
        };
        self.download_from(file, entry, &sources, &upstream)
    }

    /// Fetch a distfile from only the local custom mirrors, used under
    /// `RESTRICT=fetch` where local mirrors are permitted.
    fn fetch_from_local_mirrors(
        &self,
        file: &DistFile,
        entry: Option<&manifest::DistEntry>,
    ) -> Result<FetchedFile> {
        let digests = entry.map(|e| e.hashes.clone()).unwrap_or_default();
        let bases = self.config.custom_mirrors.local.clone();
        let layouts = self.resolve_mirror_layouts(&bases);
        let mut sources = Vec::new();
        for base in &bases {
            sources.extend(self.mirror_sources(base, &layouts, &file.name, &digests));
        }
        self.download_from(file, entry, &sources, &[])
    }

    /// Try each source in order, downloading to a `.__download__` staging path and
    /// only renaming onto the canonical name once verification passes. Transport
    /// failures use the loose backstop; consecutive checksum failures are bounded
    /// by [`checksum_try_mirrors`](FetchConfig::checksum_try_mirrors).
    fn download_from(
        &self,
        file: &DistFile,
        entry: Option<&manifest::DistEntry>,
        sources: &[String],
        upstream: &[String],
    ) -> Result<FetchedFile> {
        let dest = self.config.distdir.join(&file.name);
        let staging = self
            .config
            .distdir
            .join(format!("{}.__download__", file.name));
        let staging_name = format!("{}.__download__", file.name);

        // A growable work list so the upstream URIs can be escalated ahead of the
        // remaining mirrors after repeated checksum failures.
        let mut work: Vec<String> = sources.to_vec();
        let mut i = 0usize;
        let mut transport_attempts = 0u32;
        let mut checksum_failures = 0u32;
        let mut escalated = false;
        while i < work.len() {
            if transport_attempts >= self.config.max_attempts.max(work.len() as u32) {
                break;
            }
            if checksum_failures >= self.config.checksum_try_mirrors {
                break;
            }
            let uri = work[i].clone();
            i += 1;
            transport_attempts += 1;
            self.run_fetch(&uri, &staging_name, entry, &staging)?;

            if !staging.exists() {
                continue;
            }
            match entry {
                Some(entry) => {
                    match manifest::verify_file(entry, &staging, &self.config.required_hashes)? {
                        VerifyOutcome::Ok => {
                            std::fs::rename(&staging, &dest).at(&staging)?;
                            return Ok(FetchedFile {
                                name: file.name.clone(),
                                path: dest,
                                status: FetchStatus::Fetched,
                            });
                        }
                        outcome => {
                            warn!(uri, reason = %outcome.reason(), "verification failed; trying next source");
                            checksum_failures += 1;
                            self.move_aside(&staging)?;
                            // After the second consecutive checksum failure,
                            // insert the upstream URIs ahead of the remaining
                            // mirrors so upstream is tried before the give-up
                            // budget, matching `checksum_failure_primaryuri`.
                            if !escalated
                                && checksum_failures >= CHECKSUM_FAILURE_PRIMARYURI
                                && !upstream.is_empty()
                            {
                                escalate_to_upstream(&mut work, i, upstream);
                                escalated = true;
                            }
                        }
                    }
                }
                None => {
                    if file_len(&staging)? > 0 {
                        std::fs::rename(&staging, &dest).at(&staging)?;
                        return Ok(FetchedFile {
                            name: file.name.clone(),
                            path: dest,
                            status: FetchStatus::Fetched,
                        });
                    }
                    self.move_aside(&staging)?;
                }
            }
        }

        Err(BuildError::Fetch {
            distfile: file.name.clone(),
            attempts: transport_attempts,
        })
    }

    /// Adopt an already-present verified copy from a read-only distdir or a
    /// filesystem custom mirror, symlinking (RO distdir) or copying (filesystem
    /// mirror) it into the distdir rather than downloading. Returns the fetched
    /// file when one is adopted.
    fn adopt_local_copy(
        &self,
        file: &DistFile,
        entry: Option<&manifest::DistEntry>,
    ) -> Result<Option<FetchedFile>> {
        let dest = self.config.distdir.join(&file.name);
        for ro in &self.config.ro_distdirs {
            let candidate = ro.join(&file.name);
            if candidate.exists() && self.verify_or_ok(entry, &candidate)? {
                let _ = std::fs::remove_file(&dest);
                if std::os::unix::fs::symlink(&candidate, &dest).is_ok() {
                    return Ok(Some(FetchedFile {
                        name: file.name.clone(),
                        path: dest,
                        status: FetchStatus::AlreadyPresent,
                    }));
                }
            }
        }
        for fsm in &self.config.custom_mirrors.filesystem {
            let candidate = fsm.join(&file.name);
            if candidate.exists() && self.verify_or_ok(entry, &candidate)? {
                std::fs::copy(&candidate, &dest).at(&candidate)?;
                return Ok(Some(FetchedFile {
                    name: file.name.clone(),
                    path: dest,
                    status: FetchStatus::AlreadyPresent,
                }));
            }
        }
        Ok(None)
    }

    /// Acquire an advisory lock on `<distfile>.lockfile` held for the fetch.
    fn distlock(&self, name: &str) -> Result<std::fs::File> {
        let path = self.config.distdir.join(format!("{name}.lockfile"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .at(&path)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
            .at(&path)?;
        Ok(file)
    }

    /// Resolve the ordered source URIs for a distfile: local custom mirrors, the
    /// Gentoo mirror network (through its directory layout), public custom
    /// mirrors, `mirror://` expansions, and the upstream URIs. `RESTRICT=mirror`
    /// drops the mirror network; `RESTRICT=primaryuri` moves the upstream URIs
    /// ahead of the mirrors.
    fn resolve_sources(&self, file: &DistFile, restrict: RestrictFlags) -> Vec<String> {
        let mirrorable = !restrict.mirror && !file.mirror_restricted;
        let digests = self
            .manifest
            .dist(&file.name)
            .map(|e| e.hashes.clone())
            .unwrap_or_default();

        let upstream = self.upstream_uris(file);

        // The Gentoo mirror network, with each mirror's candidate path computed
        // from that mirror's own resolved layout plus a flat fallback.
        let mut mirror_net = Vec::new();
        if mirrorable {
            let mut bases = self.config.custom_mirrors.local.clone();
            bases.extend(self.config.mirrors.clone());
            bases.extend(self.config.custom_mirrors.public.clone());
            shuffle_seeded(&mut bases, seed_for(&file.name));
            let layouts = self.resolve_mirror_layouts(&bases);
            for base in &bases {
                mirror_net.extend(self.mirror_sources(base, &layouts, &file.name, &digests));
            }
        }

        // primaryuri moves upstream ahead of the mirror network.
        let mut out = Vec::new();
        if restrict.primaryuri {
            out.extend(upstream);
            out.extend(mirror_net);
        } else {
            out.extend(mirror_net);
            out.extend(upstream);
        }
        out.dedup();
        out
    }

    /// The upstream `SRC_URI` source URIs for a distfile, with `mirror://`
    /// references expanded against the third-party mirror lists.
    fn upstream_uris(&self, file: &DistFile) -> Vec<String> {
        let mut upstream = Vec::new();
        for uri in &file.uris {
            if let Some(rest) = uri.strip_prefix("mirror://") {
                upstream.extend(self.expand_mirror(rest));
            } else {
                upstream.push(uri.clone());
            }
        }
        upstream
    }

    /// The full source URIs for one mirror base, computing the candidate path
    /// from that mirror's resolved layout and url-encoding it for the web
    /// schemes.
    fn mirror_sources(
        &self,
        base: &str,
        layouts: &BTreeMap<String, MirrorLayout>,
        name: &str,
        digests: &BTreeMap<String, String>,
    ) -> Vec<String> {
        let key = base.trim_end_matches('/');
        let layout = layouts.get(key).unwrap_or(&MirrorLayout::Flat);
        let web = matches!(uri_scheme(base), "http" | "https" | "ftp");
        mirror_paths(layout, name, digests)
            .into_iter()
            .map(|path| {
                let path = if web { url_encode_path(&path) } else { path };
                format!("{key}/distfiles/{path}")
            })
            .collect()
    }

    /// The path to the per-distdir mirror-layout cache file.
    fn mirror_cache_path(&self) -> PathBuf {
        self.config.distdir.join(".mirror-cache.json")
    }

    /// Resolve every distinct mirror base's distfile layout, reading each from a
    /// daily-refreshed `DISTDIR/.mirror-cache.json` and otherwise fetching that
    /// mirror's `distfiles/layout.conf` through the runner. A base whose layout
    /// cannot be read falls back to a flat layout.
    fn resolve_mirror_layouts(&self, bases: &[String]) -> BTreeMap<String, MirrorLayout> {
        let cache_path = self.mirror_cache_path();
        let mut cache = MirrorCache::load(&cache_path);
        let now = now_secs();
        let mut out = BTreeMap::new();
        let mut dirty = false;
        for base in bases {
            let key = base.trim_end_matches('/').to_string();
            if out.contains_key(&key) {
                continue;
            }
            let layout = match cache.entries.get(&key) {
                Some(entry) if now.saturating_sub(entry.fetched_at) < MIRROR_LAYOUT_TTL_SECS => {
                    entry.layout.clone()
                }
                _ => {
                    let resolved = self.fetch_mirror_layout(&key);
                    cache.entries.insert(
                        key.clone(),
                        CachedMirrorLayout {
                            layout: resolved.clone(),
                            fetched_at: now,
                        },
                    );
                    dirty = true;
                    resolved
                }
            };
            out.insert(key, layout);
        }
        if dirty {
            cache.save(&cache_path);
        }
        out
    }

    /// Fetch and parse a single mirror's `distfiles/layout.conf` through the
    /// runner, returning [`MirrorLayout::Flat`] when it cannot be read or is
    /// empty, mirroring Portage's `async_mirror_url`.
    fn fetch_mirror_layout(&self, base: &str) -> MirrorLayout {
        let uri = format!("{base}/distfiles/layout.conf");
        let staging_name = ".mirror-layout.__download__";
        let staging = self.config.distdir.join(staging_name);
        let _ = std::fs::remove_file(&staging);
        if self.run_fetch(&uri, staging_name, None, &staging).is_err() {
            return MirrorLayout::Flat;
        }
        let layout = match std::fs::read_to_string(&staging) {
            Ok(text) if !text.trim().is_empty() => MirrorLayout::parse(&text),
            _ => MirrorLayout::Flat,
        };
        let _ = std::fs::remove_file(&staging);
        layout
    }

    /// Expand a `mirror://group/path` reference against the named third-party
    /// mirror list, shuffling the host order.
    fn expand_mirror(&self, rest: &str) -> Vec<String> {
        let (group, path) = match rest.split_once('/') {
            Some((g, p)) => (g, p),
            None => (rest, ""),
        };
        match self.config.thirdparty.get(group) {
            Some(bases) => {
                let mut hosts = bases.clone();
                shuffle_seeded(&mut hosts, seed_for(path));
                hosts
                    .iter()
                    .map(|b| format!("{}/{}", b.trim_end_matches('/'), path))
                    .collect()
            }
            None => Vec::new(),
        }
    }

    /// Run the fetch (or resume) command for a single URI, downloading to the
    /// staging path `dest`. The command template is selected by the URI's scheme
    /// with the generic command as fallback.
    fn run_fetch(
        &self,
        uri: &str,
        file: &str,
        entry: Option<&manifest::DistEntry>,
        dest: &Path,
    ) -> Result<()> {
        let resume = dest.exists() && file_len(dest)? >= self.config.resume_min_size;
        let scheme = uri_scheme(uri);
        let template = if resume {
            self.config
                .resumecommand_proto
                .get(scheme)
                .unwrap_or(&self.config.resumecommand)
        } else {
            self.config
                .fetchcommand_proto
                .get(scheme)
                .unwrap_or(&self.config.fetchcommand)
        };
        let spec = self.fetch_command(template, uri, file, entry)?;
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

    /// Substitute the command placeholders in a template: `${URI}`, `${FILE}`,
    /// `${DISTDIR}`, `${DIGESTS}` (space-separated `algo:hex`), and
    /// `${PORTAGE_SSH_OPTS}`. The chosen template must contain `${FILE}`.
    fn fetch_command(
        &self,
        template: &[String],
        uri: &str,
        file: &str,
        entry: Option<&manifest::DistEntry>,
    ) -> Result<CommandSpec> {
        if !template.iter().any(|s| s.contains("${FILE}")) {
            return Err(BuildError::environment(
                "fetch command template must contain ${FILE}",
            ));
        }
        let distdir = self.config.distdir.to_string_lossy().to_string();
        let digests = entry
            .map(|e| {
                e.hashes
                    .iter()
                    .map(|(a, h)| format!("{}:{h}", a.to_ascii_lowercase()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let subst = |s: &str| -> String {
            s.replace("${URI}", uri)
                .replace("${FILE}", file)
                .replace("${DISTDIR}", &distdir)
                .replace("${DIGESTS}", &digests)
                .replace("${PORTAGE_SSH_OPTS}", &self.config.ssh_opts)
        };
        let program = template.first().map(|s| subst(s)).unwrap_or_default();
        let args = template
            .iter()
            .skip(1)
            .map(|s| subst(s))
            .collect::<Vec<_>>();
        Ok(CommandSpec::new(program, &self.config.distdir).args(args))
    }

    fn verify_or_ok(&self, entry: Option<&manifest::DistEntry>, path: &Path) -> Result<bool> {
        match entry {
            Some(entry) => {
                Ok(manifest::verify_file(entry, path, &self.config.required_hashes)?.is_ok())
            }
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

/// A mirror's distfile directory layout, parsed from its `distfiles/layout.conf`.
///
/// The supported structures mirror Portage's: `flat` (the distfile sits directly
/// under `distfiles/`), `filename-hash <algo> <cutoffs>` (hashed by filename),
/// and `content-hash <algo> <cutoffs>` (addressed by the file's content digest).
/// Cutoffs are bit counts, split into nested hex directory levels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MirrorLayout {
    /// The distfile sits directly under the mirror's `distfiles/`.
    Flat,
    /// `filename-hash <algo> <cutoffs>`: nested directories from the hash of the
    /// filename.
    FilenameHash {
        /// The hash algorithm (uppercase Manifest name).
        algo: String,
        /// Per-level bit cutoffs.
        cutoffs: Vec<usize>,
    },
    /// `content-hash <algo> <cutoffs>`: nested directories from the file's content
    /// digest, with the digest itself as the filename.
    ContentHash {
        /// The hash algorithm (uppercase Manifest name).
        algo: String,
        /// Per-level bit cutoffs.
        cutoffs: Vec<usize>,
    },
}

impl MirrorLayout {
    /// Parse a `distfiles/layout.conf`, returning the first supported `[structure]`
    /// entry, or [`MirrorLayout::Flat`] when none is supported.
    pub fn parse(text: &str) -> MirrorLayout {
        let mut in_structure = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_structure = line.eq_ignore_ascii_case("[structure]");
                continue;
            }
            if !in_structure || line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((_, value)) = line.split_once('=') else {
                continue;
            };
            if let Some(layout) = Self::parse_structure(value.trim()) {
                return layout;
            }
        }
        MirrorLayout::Flat
    }

    fn parse_structure(value: &str) -> Option<MirrorLayout> {
        let mut parts = value.split_whitespace();
        match parts.next()? {
            "flat" => Some(MirrorLayout::Flat),
            kind @ ("filename-hash" | "content-hash") => {
                let algo = parts.next()?.to_ascii_uppercase();
                let cutoffs: Vec<usize> = parts
                    .next()
                    .map(|c| c.split(':').filter_map(|n| n.parse().ok()).collect())
                    .unwrap_or_default();
                if kind == "filename-hash" {
                    Some(MirrorLayout::FilenameHash { algo, cutoffs })
                } else {
                    Some(MirrorLayout::ContentHash { algo, cutoffs })
                }
            }
            _ => None,
        }
    }

    /// The mirror-relative path for `name`, given its content `digests` when the
    /// layout is content-addressed. Returns `None` when the layout needs a digest
    /// that is absent.
    pub fn get_path(&self, name: &str, digests: &BTreeMap<String, String>) -> Option<String> {
        match self {
            MirrorLayout::Flat => Some(name.to_string()),
            MirrorLayout::FilenameHash { algo, cutoffs } => {
                let digest = digest_of(algo, name.as_bytes())?;
                Some(format!("{}{name}", hash_dirs(&digest, cutoffs)))
            }
            MirrorLayout::ContentHash { algo, cutoffs } => {
                let digest = digests.get(algo)?;
                Some(format!("{}{digest}", hash_dirs(digest, cutoffs)))
            }
        }
    }
}

/// Build the nested hex directory prefix (`ab/cd/`) from a digest and bit
/// cutoffs (8 bits = 2 hex characters per level).
fn hash_dirs(digest: &str, cutoffs: &[usize]) -> String {
    let mut out = String::new();
    let mut pos = 0;
    for &bits in cutoffs {
        let chars = bits / 4;
        if pos + chars > digest.len() {
            break;
        }
        out.push_str(&digest[pos..pos + chars]);
        out.push('/');
        pos += chars;
    }
    out
}

/// The lowercase hex digest of `data` for a Manifest algorithm, or `None`.
fn digest_of(algo: &str, data: &[u8]) -> Option<String> {
    match algo {
        "BLAKE2B" => Some(moraine_common::hash::blake2b(data)),
        "SHA512" => Some(moraine_common::hash::sha512(data)),
        "SHA256" => Some(moraine_common::hash::sha256(data)),
        "MD5" => Some(moraine_common::hash::md5(data)),
        _ => None,
    }
}

/// A per-mirror resolved layout cached in `DISTDIR/.mirror-cache.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedMirrorLayout {
    /// The resolved layout for the mirror.
    layout: MirrorLayout,
    /// The unix timestamp (seconds) when the layout was last resolved.
    fetched_at: u64,
}

/// The on-disk cache of resolved mirror layouts, keyed by mirror base URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MirrorCache {
    /// The cached layout per mirror base URL.
    entries: BTreeMap<String, CachedMirrorLayout>,
}

impl MirrorCache {
    /// Load the cache from `path`, returning an empty cache when it is absent or
    /// unparseable.
    fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => MirrorCache::default(),
        }
    }

    /// Persist the cache to `path`, ignoring write errors (the cache is an
    /// optimization, not a correctness requirement).
    fn save(&self, path: &Path) {
        if let Ok(text) = serde_json::to_string(self) {
            let _ = std::fs::write(path, text);
        }
    }
}

/// The current unix time in whole seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The mirror-relative path(s) for a distfile under `distfiles/`, given a
/// mirror's resolved layout: the layout path first, then the flat name as a
/// fallback.
fn mirror_paths(
    layout: &MirrorLayout,
    name: &str,
    digests: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(p) = layout.get_path(name, digests) {
        paths.push(p);
    }
    if !paths.iter().any(|p| p == name) {
        paths.push(name.to_string());
    }
    paths
}

/// Percent-encode a mirror path for the `http`, `https`, and `ftp` schemes,
/// preserving `/` separators and the RFC 3986 unreserved characters.
fn url_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Splice `upstream` URIs ahead of the not-yet-tried tail of `work` (from index
/// `cursor`), dropping any later duplicates so upstream is tried before the
/// remaining mirrors without retrying the already-attempted prefix.
fn escalate_to_upstream(work: &mut Vec<String>, cursor: usize, upstream: &[String]) {
    let already: BTreeSet<&String> = work[..cursor].iter().collect();
    let fresh: Vec<String> = upstream
        .iter()
        .filter(|u| !already.contains(*u))
        .cloned()
        .collect();
    let remaining: Vec<String> = work[cursor..]
        .iter()
        .filter(|s| !fresh.contains(s))
        .cloned()
        .collect();
    work.truncate(cursor);
    work.extend(fresh);
    work.extend(remaining);
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path).at(path)?.len())
}

/// The lowercase URI scheme (`https`, `ftp`, `ssh`, ...), or `""` when absent,
/// used to pick a protocol-specific fetch command.
fn uri_scheme(uri: &str) -> &str {
    match uri.split_once("://") {
        Some((scheme, _)) => scheme,
        None => "",
    }
}

/// A stable seed derived from a string, so mirror shuffling varies per distfile
/// but stays deterministic (and testable) for one file.
fn seed_for(s: &str) -> u64 {
    // FNV-1a over the bytes.
    let mut h = 0xcbf29ce484222325u64;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h | 1
}

/// Deterministically shuffle `items` using a seeded LCG, spreading load across
/// mirrors the way Portage's `random.shuffle` does, without a random-number
/// dependency.
fn shuffle_seeded<T>(items: &mut [T], mut state: u64) {
    if items.len() < 2 {
        return;
    }
    for i in (1..items.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        items.swap(i, j);
    }
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
    fn mirror_layout_parse_and_get_path() {
        // filename-hash: a single 8-bit cutoff yields a 2-hex-char directory.
        let layout = MirrorLayout::parse("[structure]\n0=filename-hash BLAKE2B 8\n");
        let name = "foo-1.2.3.tar.gz";
        let hash = moraine_common::hash::blake2b(name.as_bytes());
        let want = format!("{}/{name}", &hash[..2]);
        assert_eq!(layout.get_path(name, &BTreeMap::new()).unwrap(), want);

        // flat: the name as-is.
        let flat = MirrorLayout::parse("[structure]\n0=flat\n");
        assert_eq!(flat.get_path(name, &BTreeMap::new()).unwrap(), name);

        // content-hash: directories from the content digest, digest as filename.
        let ch = MirrorLayout::parse("[structure]\n0=content-hash SHA512 8:8\n");
        let mut digests = BTreeMap::new();
        digests.insert("SHA512".to_string(), "abcdef0123456789".to_string());
        assert_eq!(
            ch.get_path(name, &digests).unwrap(),
            "ab/cd/abcdef0123456789"
        );

        // No layout.conf structure: flat fallback.
        assert_eq!(MirrorLayout::parse(""), MirrorLayout::Flat);
    }

    #[test]
    fn seeded_shuffle_is_deterministic_permutation() {
        let mut a = vec![1, 2, 3, 4, 5];
        let mut b = a.clone();
        shuffle_seeded(&mut a, seed_for("foo.tar.gz"));
        shuffle_seeded(&mut b, seed_for("foo.tar.gz"));
        assert_eq!(a, b, "same seed yields the same order");
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, vec![1, 2, 3, 4, 5], "still a permutation");
    }

    #[test]
    fn fetch_command_requires_file_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FetchConfig::new(dir.path());
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        // A template without ${FILE} is rejected.
        let err = fetcher.fetch_command(&["wget".into(), "${URI}".into()], "u", "f", None);
        assert!(matches!(err, Err(BuildError::Environment { .. })));
        // ${DIGESTS} is substituted from the entry's hashes.
        let mut hashes = BTreeMap::new();
        hashes.insert("BLAKE2B".to_string(), "aa".to_string());
        let entry = manifest::DistEntry {
            name: "f".into(),
            size: 1,
            hashes,
        };
        let spec = fetcher
            .fetch_command(
                &["cmd".into(), "${FILE}".into(), "${DIGESTS}".into()],
                "u",
                "f",
                Some(&entry),
            )
            .unwrap();
        assert!(spec.args.iter().any(|a| a == "blake2b:aa"));
    }

    #[test]
    fn primaryuri_orders_upstream_before_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        cfg.mirrors = vec!["https://mirror.example".to_string()];
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        let file = distfile("f.tar.gz", &["https://upstream.example/f.tar.gz"]);

        let normal = fetcher.resolve_sources(&file, RestrictFlags::default());
        assert!(normal[0].starts_with("https://mirror.example"));

        let primary = fetcher.resolve_sources(
            &file,
            RestrictFlags {
                primaryuri: true,
                ..RestrictFlags::default()
            },
        );
        assert_eq!(primary[0], "https://upstream.example/f.tar.gz");
    }

    #[test]
    fn checksum_failure_budget_caps_retries() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"the correct bytes";
        let mani = manifest_for("f.tar.gz", good);
        let mut cfg = FetchConfig::new(dir.path());
        cfg.checksum_try_mirrors = 2;
        let staging = dir.path().join("f.tar.gz.__download__");
        let runner = FakeRunner::default();
        // Three bad sources, but the budget stops after 2 checksum failures.
        for _ in 0..3 {
            runner.push(Response::WriteFile {
                status: 0,
                path: staging.clone(),
                contents: b"bad".to_vec(),
            });
        }
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let file = distfile(
            "f.tar.gz",
            &[
                "https://a/f.tar.gz",
                "https://b/f.tar.gz",
                "https://c/f.tar.gz",
            ],
        );
        let err = fetcher.fetch_one(&file, RestrictFlags::default());
        assert!(matches!(err, Err(BuildError::Fetch { .. })));
        assert_eq!(runner.call_count(), 2, "stopped after the checksum budget");
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
        let dest = dir.path().join("f.tar.gz.__download__");
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
        let dest = dir.path().join("f.tar.gz.__download__");
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
        assert!(dir.path().join("f.tar.gz.__download__._bad_").exists());
    }

    #[test]
    fn launch_failure_falls_through_to_next_source() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"correct payload";
        let mani = manifest_for("f.tar.gz", good);
        let cfg = FetchConfig::new(dir.path());
        let dest = dir.path().join("f.tar.gz.__download__");
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
        let dest = dir.path().join("f.tar.gz.__download__");
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
            primaryuri: false,
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
            primaryuri: false,
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
        // The deprecated `nomirror` is equivalent to `mirror`.
        let r3 = RestrictFlags::from_tokens(["nomirror"]);
        assert!(r3.mirror && !r3.fetch);
    }

    #[test]
    fn nomirror_excludes_gentoo_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        cfg.mirrors = vec!["https://mirror.example".to_string()];
        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        let file = distfile("foo.tar.gz", &["https://upstream/foo.tar.gz"]);
        let restrict = RestrictFlags::from_tokens(["nomirror"]);
        let sources = fetcher.resolve_sources(&file, restrict);
        assert!(!sources.iter().any(|s| s.contains("mirror.example")));
        assert_eq!(sources, vec!["https://upstream/foo.tar.gz"]);
    }

    #[test]
    fn per_mirror_layout_selection_with_flat_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        cfg.mirrors = vec![
            "https://m0.example".to_string(),
            "https://m1.example".to_string(),
        ];
        // Pre-seed m0's layout as filename-hash; m1 has no layout.conf and falls
        // back to flat (resolved through the always-ok runner).
        let mut cache = MirrorCache::default();
        cache.entries.insert(
            "https://m0.example".to_string(),
            CachedMirrorLayout {
                layout: MirrorLayout::FilenameHash {
                    algo: "BLAKE2B".to_string(),
                    cutoffs: vec![8],
                },
                fetched_at: now_secs(),
            },
        );
        cache.save(&dir.path().join(".mirror-cache.json"));

        let mani = Manifest::default();
        let runner = FakeRunner::always_ok();
        let fetcher = Fetcher::new(&runner, &cfg, &mani, false);
        let file = distfile("foo.tar.gz", &[]);
        let sources = fetcher.resolve_sources(&file, RestrictFlags::default());

        let hash = moraine_common::hash::blake2b("foo.tar.gz".as_bytes());
        let hashed = format!("https://m0.example/distfiles/{}/foo.tar.gz", &hash[..2]);
        assert!(
            sources.contains(&hashed),
            "m0 uses its own filename-hash layout: {sources:?}"
        );
        assert!(
            sources.contains(&"https://m1.example/distfiles/foo.tar.gz".to_string()),
            "m1 falls back to flat: {sources:?}"
        );
    }

    #[test]
    fn escalates_to_upstream_after_second_checksum_failure() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"the correct bytes";
        let mani = manifest_for("f.tar.gz", good);
        let mut cfg = FetchConfig::new(dir.path());
        cfg.checksum_try_mirrors = 5;
        cfg.mirrors = (0..5).map(|i| format!("https://m{i}.example")).collect();
        // Pre-seed every mirror as flat so layout resolution does not consume the
        // queued download responses.
        let mut cache = MirrorCache::default();
        for base in &cfg.mirrors {
            cache.entries.insert(
                base.clone(),
                CachedMirrorLayout {
                    layout: MirrorLayout::Flat,
                    fetched_at: now_secs(),
                },
            );
        }
        cache.save(&dir.path().join(".mirror-cache.json"));

        let staging = dir.path().join("f.tar.gz.__download__");
        let runner = FakeRunner::default();
        // Two bad mirror responses, then the upstream serves the correct bytes.
        runner.push(Response::WriteFile {
            status: 0,
            path: staging.clone(),
            contents: b"bad".to_vec(),
        });
        runner.push(Response::WriteFile {
            status: 0,
            path: staging.clone(),
            contents: b"bad".to_vec(),
        });
        runner.push(Response::WriteFile {
            status: 0,
            path: staging.clone(),
            contents: good.to_vec(),
        });
        let fetcher = Fetcher::new(&runner, &cfg, &mani, true);
        let file = distfile("f.tar.gz", &["https://upstream.example/f.tar.gz"]);
        let f = fetcher.fetch_one(&file, RestrictFlags::default()).unwrap();
        assert_eq!(f.status, FetchStatus::Fetched);
        // Upstream was tried on the third attempt, before exhausting the mirrors.
        assert_eq!(runner.call_count(), 3);
    }

    #[test]
    fn owning_repo_required_hashes_accepts_relaxed_overlay_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = FetchConfig::new(dir.path());
        // The owning overlay requires only SHA512 (a relaxed set).
        cfg.required_hashes = ["SHA512".to_string()].into_iter().collect();
        let data = b"overlay payload";
        // A Manifest entry listing only SHA512 (no BLAKE2B).
        let text = format!(
            "DIST f.tar.gz {} SHA512 {}\n",
            data.len(),
            moraine_common::hash::sha512(data)
        );
        let mani = Manifest::parse(&text);
        let dest = dir.path().join("f.tar.gz.__download__");
        let runner = FakeRunner::default();
        runner.push(Response::WriteFile {
            status: 0,
            path: dest,
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
        // The same entry under the global {BLAKE2B, SHA512} union would be
        // rejected for the missing BLAKE2B, so the relaxation is meaningful.
        let union: BTreeSet<String> = ["BLAKE2B", "SHA512"]
            .into_iter()
            .map(String::from)
            .collect();
        let entry = mani.dist("f.tar.gz").unwrap();
        assert!(matches!(
            manifest::verify_bytes(entry, data, &union),
            VerifyOutcome::MissingRequiredHash { .. }
        ));
    }
}
