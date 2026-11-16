//! Cached release checks and the current-versus-latest comparison.

use std::{path::Path, time::Duration};

use crate::{
    cache::{self, CACHE_TTL, CachedCheck},
    error::UpdateError,
    source::UpdateTransport,
    version::Version,
};

/// The result of one successful check, whether it came from cache or network.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateInfo {
    pub current: Version,
    pub latest: Version,
}

impl UpdateInfo {
    /// True when the release is newer than the running binary.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.latest.is_newer_than(&self.current)
    }

    /// One line for the TUI transcript.
    #[must_use]
    pub fn prompt_line(&self) -> String {
        format!(
            "update available: {} (current {}); type /update",
            self.latest, self.current
        )
    }
}

/// Read a fresh cache entry, if the tag still parses.
#[must_use]
pub fn fresh_cached(path: &Path, now: u64, ttl: Duration) -> Option<UpdateInfo> {
    let cached = cache::read(path)?;
    if !cached.is_fresh(now, ttl) {
        return None;
    }
    let latest = cached.latest_version()?;
    Some(UpdateInfo {
        current: Version::current(),
        latest,
    })
}

/// Check the latest release, reusing a fresh cache entry. A failed cache write
/// never fails the check; the cache is an optimisation, not a fact.
pub async fn check(
    transport: &dyn UpdateTransport,
    cache_path: Option<&Path>,
    now: u64,
    ttl: Duration,
) -> Result<UpdateInfo, UpdateError> {
    if let Some(path) = cache_path
        && let Some(info) = fresh_cached(path, now, ttl)
    {
        return Ok(info);
    }
    let release = transport.latest_release().await?;
    if let Some(path) = cache_path {
        let entry = CachedCheck {
            checked_at: now,
            latest: release.tag.clone(),
        };
        let _ = cache::write(path, &entry);
    }
    Ok(UpdateInfo {
        current: Version::current(),
        latest: release.version,
    })
}

/// Check with the default TTL.
pub async fn check_default(
    transport: &dyn UpdateTransport,
    cache_path: Option<&Path>,
) -> Result<UpdateInfo, UpdateError> {
    check(transport, cache_path, cache::now_unix(), CACHE_TTL).await
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{UpdateInfo, check, fresh_cached};
    use crate::{
        cache::{CACHE_TTL, CachedCheck, write},
        error::UpdateError,
        source::{BoxFuture, Release, UpdateTransport},
        version::Version,
    };

    /// A fake transport that counts calls and returns a fixed release.
    #[derive(Clone)]
    struct FakeTransport {
        tag: String,
        calls: Arc<Mutex<usize>>,
    }

    impl FakeTransport {
        fn new(tag: &str) -> Self {
            Self {
                tag: tag.to_owned(),
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().expect("calls")
        }
    }

    impl UpdateTransport for FakeTransport {
        fn latest_release(&self) -> BoxFuture<'_, Result<Release, UpdateError>> {
            *self.calls.lock().expect("calls") += 1;
            let tag = self.tag.clone();
            Box::pin(async move {
                let version = Version::parse(&tag).expect("fake tag");
                Ok(Release { tag, version })
            })
        }

        fn download(&self, _url: &str) -> BoxFuture<'_, Result<Vec<u8>, UpdateError>> {
            Box::pin(async { Err(UpdateError::BinaryMissing) })
        }

        fn asset_url(&self, _tag: &str, _file: &str) -> Result<String, UpdateError> {
            Ok("https://example.invalid/asset".to_owned())
        }
    }

    fn scratch_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "slowcode-update-check-{name}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn fresh_cache_skips_the_network_and_stale_cache_does_not() {
        let path = scratch_path("ttl");
        let cached = CachedCheck {
            checked_at: 1_000,
            latest: "v0.2.0".to_owned(),
        };
        write(&path, &cached).expect("cache");

        let transport = FakeTransport::new("v0.3.0");
        let info = check(&transport, Some(&path), 1_000 + 60, CACHE_TTL)
            .await
            .expect("check");
        assert_eq!(info.latest, Version::parse("v0.2.0").expect("version"));
        assert_eq!(transport.calls(), 0);

        let transport = FakeTransport::new("v0.3.0");
        let info = check(&transport, Some(&path), 1_000 + 86_400, CACHE_TTL)
            .await
            .expect("check");
        assert_eq!(info.latest, Version::parse("v0.3.0").expect("version"));
        assert_eq!(transport.calls(), 1);

        let refreshed = fresh_cached(&path, 1_000 + 86_400, CACHE_TTL).expect("refreshed");
        assert_eq!(refreshed.latest, Version::parse("v0.3.0").expect("version"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn check_reports_availability() {
        let transport = FakeTransport::new("v0.0.1");
        let info = check(&transport, None, 0, CACHE_TTL).await.expect("check");
        assert!(!info.is_available());
        assert!(info.prompt_line().contains("/update"));

        let transport = FakeTransport::new("v99.0.0");
        let info = check(&transport, None, 0, CACHE_TTL).await.expect("check");
        assert!(info.is_available());
        assert!(info.prompt_line().contains("update available: 99.0.0"));
    }

    #[test]
    fn info_is_constructed_without_network() {
        let info = UpdateInfo {
            current: Version::parse("0.1.0").expect("version"),
            latest: Version::parse("0.2.0").expect("version"),
        };
        assert!(info.is_available());
    }
}
