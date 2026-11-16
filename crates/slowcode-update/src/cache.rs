//! The 24 hour release-check cache.
//!
//! The cache is advisory. A missing, stale, or corrupt file simply means the
//! next check goes to the network. Writes use a temporary file plus rename so
//! a crash cannot leave a half-written JSON document.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{error::UpdateError, version::Version};

/// How long a successful check is trusted.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One cached release check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedCheck {
    /// Unix seconds when the release API answered.
    pub checked_at: u64,
    /// The `tag_name` that answered.
    pub latest: String,
}

impl CachedCheck {
    /// Whether this record is younger than the TTL. A clock that moved
    /// backwards is treated as stale so the user still gets a check.
    #[must_use]
    pub fn is_fresh(&self, now: u64, ttl: Duration) -> bool {
        now.checked_sub(self.checked_at)
            .is_some_and(|age| age < ttl.as_secs())
    }

    /// The cached latest version, if the tag still parses.
    #[must_use]
    pub fn latest_version(&self) -> Option<Version> {
        Version::parse(&self.latest)
    }
}

/// Read a cache file. Corrupt or absent files return `None`.
#[must_use]
pub fn read(path: &Path) -> Option<CachedCheck> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write a cache file atomically. Missing parent directories are created.
pub fn write(path: &Path, check: &CachedCheck) -> Result<(), UpdateError> {
    let directory = path.parent().ok_or_else(|| {
        UpdateError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache path has no parent",
        ))
    })?;
    fs::create_dir_all(directory)?;
    let temporary = temporary_path(path);
    let result = (|| {
        let bytes = serde_json::to_vec(check)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "update-check.json".to_owned());
    path.with_file_name(format!(".{name}.{}.{sequence}.tmp", std::process::id()))
}

/// Current Unix seconds. Before the epoch, zero.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{CACHE_TTL, CachedCheck, now_unix, read, write};

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "slowcode-update-test-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("scratch dir");
        directory
    }

    #[test]
    fn cache_ttl_is_24_hours() {
        let check = CachedCheck {
            checked_at: 1_000,
            latest: "v0.2.0".to_owned(),
        };
        assert!(check.is_fresh(1_000, CACHE_TTL));
        assert!(check.is_fresh(1_000 + 23 * 3600, CACHE_TTL));
        assert!(!check.is_fresh(1_000 + 24 * 3600, CACHE_TTL));
        assert!(!check.is_fresh(999, CACHE_TTL));
        assert_eq!(CACHE_TTL, Duration::from_secs(86_400));
    }

    #[test]
    fn write_then_read_round_trips_and_replaces_atomically() {
        let directory = scratch_dir("cache");
        let path = directory.join("update-check.json");
        let first = CachedCheck {
            checked_at: 10,
            latest: "v0.2.0".to_owned(),
        };
        write(&path, &first).expect("write");
        assert_eq!(read(&path), Some(first));

        let second = CachedCheck {
            checked_at: 20,
            latest: "v0.3.0".to_owned(),
        };
        write(&path, &second).expect("replace");
        assert_eq!(read(&path), Some(second));
        assert_eq!(
            fs::read_dir(&directory).expect("entries").count(),
            1,
            "no temporary files remain"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn corrupt_cache_reads_as_absent() {
        let directory = scratch_dir("corrupt");
        let path = directory.join("update-check.json");
        fs::write(&path, b"{not json").expect("write");
        assert_eq!(read(&path), None);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn now_is_after_the_epoch() {
        assert!(now_unix() > 0);
    }
}
