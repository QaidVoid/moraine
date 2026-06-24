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
}
