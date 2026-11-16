//! Update failures.
//!
//! Messages are user-facing and never include credentials. The release API is
//! called without a token, so there is nothing secret to redact, but errors
//! deliberately show only status codes and paths.

use std::{
    io,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// Everything that can stop a check or an install.
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("release API returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release API returned HTTP {0}")]
    Status(u16),
    #[error("release response has no tag_name")]
    MissingTag,
    #[error("invalid release version: {0}")]
    InvalidVersion(String),
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error(
        "refusing to self-update a package-manager install at {path}; upgrade with `{hint}` instead"
    )]
    PackageManager { path: PathBuf, hint: &'static str },
    #[error("installation directory is not writable: {path}")]
    NotWritable { path: PathBuf },
    #[error("checksum mismatch for {archive}; the download was discarded")]
    ChecksumMismatch { archive: String },
    #[error("release archive is malformed: {0}")]
    Archive(String),
    #[error("release archive does not contain a slowcode binary")]
    BinaryMissing,
    #[error("refusing insecure URL: {0}")]
    InsecureUrl(String),
    #[error("SLOWCODE_VERSION pins this build to {0}; unset it to use the updater")]
    PinnedVersion(String),
    #[error("self-update is not supported on this platform; reinstall with install.sh")]
    UnsupportedSelfReplace,
}

impl UpdateError {
    /// Whether a caller may show this on startup without a user asking. Cache
    /// writes are best effort and never reach the user.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        matches!(self, Self::Io(_) | Self::Json(_))
    }

    pub(crate) fn not_writable(directory: &Path) -> Self {
        Self::NotWritable {
            path: directory.to_path_buf(),
        }
    }

    pub(crate) fn package_manager(path: &Path, hint: &'static str) -> Self {
        Self::PackageManager {
            path: path.to_path_buf(),
            hint,
        }
    }
}
