//! Detached signature verification by shelling out to `gpg`.
//!
//! This crate adds no cryptography dependency. When signature verification is
//! configured, it writes the signed bytes and the detached signature to a
//! temporary directory and runs the configured `gpg` command to verify them
//! against a configured keyring. A non-zero exit rejects the artifact.

use std::path::PathBuf;
use std::process::Command;

use crate::error::{ContainerError, IoResultExt as _};

/// Configuration for detached signature verification.
#[derive(Debug, Clone)]
pub struct SignatureConfig {
    /// The `gpg`-compatible program to invoke.
    pub gpg_command: String,
    /// An optional keyring file passed via `--keyring`. When `None`, the
    /// invoking user's default keyring is used.
    pub keyring: Option<PathBuf>,
    /// Extra arguments inserted before the verify operands.
    pub extra_args: Vec<String>,
}

impl Default for SignatureConfig {
    fn default() -> Self {
        Self {
            gpg_command: "gpg".to_string(),
            keyring: None,
            extra_args: Vec::new(),
        }
    }
}

impl SignatureConfig {
    /// Verify `signature` is a valid detached signature over `data`.
    ///
    /// Writes both buffers to a temporary directory and runs the configured
    /// `gpg --verify`. Returns `Ok(())` on a zero exit and
    /// [`ContainerError::Signature`] otherwise.
    pub fn verify_detached(&self, data: &[u8], signature: &[u8]) -> Result<(), ContainerError> {
        let span = tracing::info_span!("binpkg.signature.verify");
        let _enter = span.enter();

        let dir = tempfile::tempdir().map_err(ContainerError::IoBare)?;
        let data_path = dir.path().join("artifact");
        let sig_path = dir.path().join("artifact.sig");
        std::fs::write(&data_path, data).with_path(&data_path)?;
        std::fs::write(&sig_path, signature).with_path(&sig_path)?;

        let mut cmd = Command::new(&self.gpg_command);
        cmd.arg("--batch").arg("--status-fd").arg("2");
        if let Some(keyring) = &self.keyring {
            cmd.arg("--no-default-keyring")
                .arg("--keyring")
                .arg(keyring);
        }
        for extra in &self.extra_args {
            cmd.arg(extra);
        }
        cmd.arg("--verify").arg(&sig_path).arg(&data_path);

        let output = cmd.output().map_err(|source| {
            ContainerError::Signature(format!("failed to launch `{}`: {source}", self.gpg_command))
        })?;

        if output.status.success() {
            tracing::info!("signature verified");
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            tracing::warn!(%stderr, "signature verification failed");
            Err(ContainerError::Signature(stderr))
        }
    }

    /// Verify an inline cleartext-signed document (a `-----BEGIN PGP SIGNED
    /// MESSAGE-----` blob, as `binpkg-signing` embeds in the gpkg Manifest),
    /// returning the verified cleartext body on success.
    ///
    /// Runs the configured `gpg --decrypt`, which both checks the signature and
    /// emits the signed text; a non-zero exit rejects the document.
    pub fn verify_inline(&self, signed: &[u8]) -> Result<Vec<u8>, ContainerError> {
        let span = tracing::info_span!("binpkg.signature.verify_inline");
        let _enter = span.enter();

        let dir = tempfile::tempdir().map_err(ContainerError::IoBare)?;
        let path = dir.path().join("Manifest.asc");
        std::fs::write(&path, signed).with_path(&path)?;

        let mut cmd = Command::new(&self.gpg_command);
        cmd.arg("--batch").arg("--status-fd").arg("2");
        if let Some(keyring) = &self.keyring {
            cmd.arg("--no-default-keyring")
                .arg("--keyring")
                .arg(keyring);
        }
        for extra in &self.extra_args {
            cmd.arg(extra);
        }
        cmd.arg("--decrypt").arg(&path);

        let output = cmd.output().map_err(|source| {
            ContainerError::Signature(format!("failed to launch `{}`: {source}", self.gpg_command))
        })?;

        if output.status.success() {
            tracing::info!("inline signature verified");
            Ok(output.stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            tracing::warn!(%stderr, "inline signature verification failed");
            Err(ContainerError::Signature(stderr))
        }
    }
}

/// The policy for a gpkg Manifest signature, mirroring Portage's
/// `binpkg-request-signature`/`binpkg-ignore-signature`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignaturePolicy {
    /// Verify a present signature; an unsigned Manifest is accepted.
    #[default]
    VerifyIfPresent,
    /// A missing Manifest signature is fatal.
    RequestSignature,
    /// Signature verification is skipped entirely.
    IgnoreSignature,
}
