//! Fetching binary packages from a remote binhost.
//!
//! Fetching shells out to a configurable command (a `wget`/`curl`-style tool)
//! through [`std::process::Command`]; this crate adds no HTTP-client
//! dependency. A fetched artifact is verified against its Manifest, and against
//! its signature when verification is configured, before it is exposed as usable
//! or written to the local cache. Network access and verification are confined
//! to this crate.

use std::path::Path;
use std::process::Command;

use crate::detect::{Format, detect};
use crate::error::{ContainerError, FetchError};
use crate::signature::SignatureConfig;

/// The template-based fetch command configuration.
///
/// `command` is the program to run and `args` its arguments, with two
/// placeholders substituted per fetch: `{uri}` for the source URI and `{file}`
/// for the destination path. A typical wget configuration is
/// `command = "wget"`, `args = ["-O", "{file}", "{uri}"]`.
#[derive(Debug, Clone)]
pub struct FetchCommand {
    /// The fetch program to invoke.
    pub command: String,
    /// The argument template; `{uri}` and `{file}` are substituted.
    pub args: Vec<String>,
}

impl Default for FetchCommand {
    fn default() -> Self {
        Self {
            command: "wget".to_string(),
            args: vec!["-O".to_string(), "{file}".to_string(), "{uri}".to_string()],
        }
    }
}

impl FetchCommand {
    /// Run the fetch command, downloading `uri` to `dest`.
    pub fn run(&self, uri: &str, dest: &Path) -> Result<(), FetchError> {
        let span = tracing::info_span!("binpkg.fetch.run", uri, dest = %dest.display());
        let _enter = span.enter();

        let dest_str = dest.to_string_lossy();
        let mut cmd = Command::new(&self.command);
        for arg in &self.args {
            let substituted = arg.replace("{uri}", uri).replace("{file}", &dest_str);
            cmd.arg(substituted);
        }
        let output = cmd.output().map_err(|source| FetchError::Launch {
            command: self.command.clone(),
            source,
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(FetchError::Command {
                status: output.status.code().unwrap_or(-1),
                stderr,
            });
        }
        tracing::info!("fetch complete");
        Ok(())
    }
}

/// How to verify a fetched artifact.
#[derive(Debug, Clone, Default)]
pub struct VerifyPolicy {
    /// When set, the artifact's signature is verified against this config.
    pub signature: Option<SignatureConfig>,
}

/// Fetch a binary package and verify it before exposing its bytes.
///
/// Downloads `uri` to a temporary file under `cache_dir`, verifies its
/// integrity manifest and (when configured) signature, and only on success
/// returns the verified bytes and persists the file into `cache_dir` under
/// `filename`. A verification failure rejects the artifact and removes the
/// temporary file, leaving the cache untouched so the resolver can fall back to
/// a source candidate.
pub fn fetch_and_verify(
    command: &FetchCommand,
    uri: &str,
    cache_dir: &Path,
    filename: &str,
    policy: &VerifyPolicy,
) -> Result<Vec<u8>, FetchError> {
    let span = tracing::info_span!("binpkg.fetch.verify", uri, filename);
    let _enter = span.enter();

    std::fs::create_dir_all(cache_dir).map_err(|source| FetchError::Io {
        path: cache_dir.to_path_buf(),
        source,
    })?;
    let tmp = cache_dir.join(format!("{filename}.partial"));
    command.run(uri, &tmp)?;

    let bytes = std::fs::read(&tmp).map_err(|source| FetchError::Io {
        path: tmp.clone(),
        source,
    })?;

    if let Err(err) = verify(&bytes, policy) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("verification failed; artifact rejected");
        return Err(FetchError::Verification(err));
    }

    let final_path = cache_dir.join(filename);
    std::fs::rename(&tmp, &final_path).map_err(|source| FetchError::Io {
        path: final_path,
        source,
    })?;
    tracing::info!("fetched artifact verified and cached");
    Ok(bytes)
}

/// Verify a fetched artifact's manifest and optional signature.
///
/// For the greenfield format this verifies the embedded manifest and, when a
/// signature policy is set, the detached signature. For stock formats the
/// importer performs manifest verification on read; here a configured signature
/// policy still applies to a present detached signature.
fn verify(bytes: &[u8], policy: &VerifyPolicy) -> Result<(), ContainerError> {
    match detect(bytes)? {
        Format::Greenfield => {
            let reader = crate::greenfield::Reader::open(bytes)?;
            reader.verify_manifest()?;
            if let Some(config) = &policy.signature {
                reader.verify_signature(config)?;
            }
            Ok(())
        }
        Format::Gpkg => {
            // Import verifies member checksums and, with a signature config, the
            // detached signatures.
            crate::gpkg::read(bytes, policy.signature.as_ref())?;
            Ok(())
        }
        Format::Xpak => {
            // xpak carries no integrity manifest; a structural parse is the only
            // available check, and a signature policy cannot apply.
            crate::xpak::read(bytes)?;
            Ok(())
        }
    }
}

/// Read an artifact already present in the local cache and verify it.
///
/// Memory-maps `path`, runs the same verification as a fresh fetch, and returns
/// the bytes on success.
pub fn read_cached(path: &Path, policy: &VerifyPolicy) -> Result<Vec<u8>, FetchError> {
    let bytes = std::fs::read(path).map_err(|source| FetchError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    verify(&bytes, policy).map_err(FetchError::Verification)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greenfield::{WriteOptions, attach_signature, write_bytes};
    use crate::metadata::{KEY_CHOST, MetadataMap};

    fn greenfield_artifact() -> Vec<u8> {
        let mut m = MetadataMap::new();
        m.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
        write_bytes(&m, b"image-bytes", &WriteOptions::default()).unwrap()
    }

    #[test]
    fn fetch_via_cp_command_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.bpkg");
        std::fs::write(&src, greenfield_artifact()).unwrap();

        // Use `cp` as the fetch tool: cp {uri} {file}.
        let command = FetchCommand {
            command: "cp".to_string(),
            args: vec!["{uri}".to_string(), "{file}".to_string()],
        };
        let cache = dir.path().join("cache");
        let bytes = fetch_and_verify(
            &command,
            src.to_str().unwrap(),
            &cache,
            "pkg.bpkg",
            &VerifyPolicy::default(),
        )
        .unwrap();
        assert!(crate::greenfield::is_greenfield(&bytes));
        assert!(cache.join("pkg.bpkg").exists());
    }

    #[test]
    fn corrupt_artifact_rejected_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact = greenfield_artifact();
        let off = crate::greenfield::Reader::open(&artifact)
            .unwrap()
            .image_offset();
        artifact[off] ^= 0xff;
        let src = dir.path().join("source.bpkg");
        std::fs::write(&src, &artifact).unwrap();

        let command = FetchCommand {
            command: "cp".to_string(),
            args: vec!["{uri}".to_string(), "{file}".to_string()],
        };
        let cache = dir.path().join("cache");
        let res = fetch_and_verify(
            &command,
            src.to_str().unwrap(),
            &cache,
            "pkg.bpkg",
            &VerifyPolicy::default(),
        );
        assert!(matches!(res, Err(FetchError::Verification(_))));
        assert!(!cache.join("pkg.bpkg").exists());
    }

    #[test]
    fn fetch_command_failure_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let command = FetchCommand {
            command: "false".to_string(),
            args: vec![],
        };
        let res = fetch_and_verify(
            &command,
            "irrelevant",
            dir.path(),
            "pkg.bpkg",
            &VerifyPolicy::default(),
        );
        assert!(matches!(res, Err(FetchError::Command { .. })));
    }

    #[test]
    fn signature_verified_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let signed = attach_signature(&greenfield_artifact(), b"FAKE").unwrap();
        let src = dir.path().join("source.bpkg");
        std::fs::write(&src, &signed).unwrap();

        let command = FetchCommand {
            command: "cp".to_string(),
            args: vec!["{uri}".to_string(), "{file}".to_string()],
        };
        let policy = VerifyPolicy {
            signature: Some(SignatureConfig {
                gpg_command: "false".to_string(),
                keyring: None,
                extra_args: Vec::new(),
            }),
        };
        let res = fetch_and_verify(
            &command,
            src.to_str().unwrap(),
            &dir.path().join("cache"),
            "pkg.bpkg",
            &policy,
        );
        // `false` gpg always fails, so verification rejects the artifact.
        assert!(matches!(res, Err(FetchError::Verification(_))));
    }
}
