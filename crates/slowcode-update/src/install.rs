//! Download, verify, and replace the running executable.
//!
//! The updater never uninstalls and never writes inside the workspace. It only
//! writes a temporary executable beside the current one and renames it over.
//! Package-manager installs are refused before any network call.

use std::{
    fs,
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::{error::UpdateError, source::UpdateTransport, target::Target, version::Version};

/// Checkout that owns the binary, if this looks like a package-manager path.
#[must_use]
pub fn package_manager_hint(path: &Path) -> Option<&'static str> {
    let text = path.to_string_lossy();
    if text.contains("/Cellar/") || text.contains("/opt/homebrew/") {
        Some("brew upgrade slowcode")
    } else {
        None
    }
}

/// One install attempt.
pub struct InstallRequest<'a> {
    pub transport: &'a dyn UpdateTransport,
    /// The executable to replace. Production passes `std::env::current_exe`.
    pub exe_path: PathBuf,
    /// The release target whose assets to download.
    pub target: Target,
    /// The running version.
    pub current: Version,
}

/// What an install attempt did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    UpToDate {
        current: Version,
    },
    Updated {
        path: PathBuf,
        old: Version,
        new: Version,
    },
}

impl InstallOutcome {
    /// True when the executable on disk changed.
    #[must_use]
    pub fn replaced(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

/// Check, download, verify, and replace the executable.
pub async fn install(request: &InstallRequest<'_>) -> Result<InstallOutcome, UpdateError> {
    if cfg!(windows) {
        return Err(UpdateError::UnsupportedSelfReplace);
    }
    refuse_package_manager(&request.exe_path)?;
    // Replace the real file when the installed path is a symlink, so the link
    // itself survives the update.
    let exe_path = fs::canonicalize(&request.exe_path).unwrap_or_else(|_| request.exe_path.clone());

    let release = request.transport.latest_release().await?;
    if !release.version.is_newer_than(&request.current) {
        return Ok(InstallOutcome::UpToDate {
            current: request.current.clone(),
        });
    }

    let archive = request.target.archive_name();
    let checksum = request.target.checksum_name();
    let checksum_url = request.transport.asset_url(&release.tag, &checksum)?;
    let archive_url = request.transport.asset_url(&release.tag, &archive)?;

    let expected = parse_checksum(&request.transport.download(&checksum_url).await?)?;
    let bytes = request.transport.download(&archive_url).await?;
    verify_checksum(&bytes, &expected, &archive)?;
    let binary = extract_binary(&bytes)?;
    replace_executable(&exe_path, &binary)?;

    Ok(InstallOutcome::Updated {
        path: exe_path,
        old: request.current.clone(),
        new: release.version,
    })
}

fn refuse_package_manager(exe_path: &Path) -> Result<(), UpdateError> {
    if let Some(hint) = package_manager_hint(exe_path) {
        return Err(UpdateError::package_manager(exe_path, hint));
    }
    let resolved = fs::canonicalize(exe_path).unwrap_or_else(|_| exe_path.to_path_buf());
    if let Some(hint) = package_manager_hint(&resolved) {
        return Err(UpdateError::package_manager(&resolved, hint));
    }
    Ok(())
}

fn parse_checksum(bytes: &[u8]) -> Result<String, UpdateError> {
    let text = String::from_utf8_lossy(bytes);
    let token = text.split_whitespace().next().unwrap_or_default();
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(UpdateError::Archive(
            "checksum file is not a sha256 digest".to_owned(),
        ));
    }
    Ok(token.to_ascii_lowercase())
}

fn verify_checksum(bytes: &[u8], expected: &str, archive: &str) -> Result<(), UpdateError> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch {
            archive: archive.to_owned(),
        });
    }
    Ok(())
}

fn extract_binary(bytes: &[u8]) -> Result<Vec<u8>, UpdateError> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| UpdateError::Archive(error.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| UpdateError::Archive(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| UpdateError::Archive(error.to_string()))?;
        let safe = path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)));
        if !safe || path.file_name().is_none_or(|name| name != "slowcode") {
            continue;
        }
        let mut binary = Vec::new();
        entry
            .read_to_end(&mut binary)
            .map_err(|error| UpdateError::Archive(error.to_string()))?;
        return Ok(binary);
    }
    Err(UpdateError::BinaryMissing)
}

fn replace_executable(exe_path: &Path, binary: &[u8]) -> Result<(), UpdateError> {
    let directory = exe_path
        .parent()
        .ok_or_else(|| UpdateError::not_writable(Path::new(".")))?;
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".slowcode-update-{}.{sequence}.tmp",
        std::process::id()
    ));
    // A leftover from a crashed update would block create_new; it is never live.
    let _ = fs::remove_file(&temporary);
    let result = (|| {
        let mut file =
            create_executable(&temporary).map_err(|error| map_write_error(error, directory))?;
        file.write_all(binary)?;
        file.sync_all()?;
        drop(file);
        set_executable(&temporary)?;
        fs::rename(&temporary, exe_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn map_write_error(error: io::Error, directory: &Path) -> UpdateError {
    if matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
    ) {
        UpdateError::not_writable(directory)
    } else {
        UpdateError::Io(error)
    }
}

fn create_executable(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o755);
    }
    options.open(path)
}

fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use sha2::{Digest, Sha256};

    use super::{InstallOutcome, InstallRequest, install, package_manager_hint, parse_checksum};
    use crate::{
        error::UpdateError,
        source::{BoxFuture, Release, UpdateTransport},
        target::Target,
        version::Version,
    };

    struct FakeTransport {
        tag: String,
        archive: Vec<u8>,
        checksum: Vec<u8>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeTransport {
        fn new(tag: &str, archive: Vec<u8>, checksum: Vec<u8>) -> Self {
            Self {
                tag: tag.to_owned(),
                archive,
                checksum,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl UpdateTransport for FakeTransport {
        fn latest_release(&self) -> BoxFuture<'_, Result<Release, UpdateError>> {
            self.calls.lock().expect("calls").push("latest".to_owned());
            let tag = self.tag.clone();
            Box::pin(async move {
                let version = Version::parse(&tag).expect("fake tag");
                Ok(Release { tag, version })
            })
        }

        fn download(&self, url: &str) -> BoxFuture<'_, Result<Vec<u8>, UpdateError>> {
            self.calls.lock().expect("calls").push(url.to_owned());
            let checksum = self.checksum.clone();
            let archive = self.archive.clone();
            let url = url.to_owned();
            Box::pin(async move {
                if url.ends_with(".sha256") {
                    Ok(checksum)
                } else {
                    Ok(archive)
                }
            })
        }

        fn asset_url(&self, tag: &str, file: &str) -> Result<String, UpdateError> {
            Ok(format!("https://example.invalid/{tag}/{file}"))
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "slowcode-update-install-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("scratch dir");
        directory
    }

    fn archive_with(binary: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(binary.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "slowcode/slowcode", binary)
                .expect("append");
            builder.finish().expect("finish");
        }
        encoder.finish().expect("gzip")
    }

    fn checksum_for(bytes: &[u8]) -> Vec<u8> {
        format!("{:x}\n", Sha256::digest(bytes)).into_bytes()
    }

    fn request<'a>(transport: &'a FakeTransport, exe: &Path, current: &str) -> InstallRequest<'a> {
        InstallRequest {
            transport,
            exe_path: exe.to_path_buf(),
            target: Target::current().expect("host target"),
            current: Version::parse(current).expect("version"),
        }
    }

    #[test]
    fn package_manager_paths_are_refused_with_the_right_hint() {
        assert_eq!(
            package_manager_hint(Path::new("/opt/homebrew/bin/slowcode")),
            Some("brew upgrade slowcode")
        );
        assert_eq!(
            package_manager_hint(Path::new(
                "/opt/homebrew/Cellar/slowcode/0.1.0/bin/slowcode"
            )),
            Some("brew upgrade slowcode")
        );
        assert_eq!(
            package_manager_hint(Path::new("/Users/me/.local/bin/slowcode")),
            None
        );
    }

    #[tokio::test]
    async fn package_manager_install_is_refused_before_any_network_call() {
        let transport = FakeTransport::new("v0.2.0", Vec::new(), Vec::new());
        let exe = PathBuf::from("/opt/homebrew/bin/slowcode");
        let error = install(&request(&transport, &exe, "0.1.0"))
            .await
            .expect_err("refused");
        assert!(matches!(error, UpdateError::PackageManager { .. }));
        assert!(error.to_string().contains("brew upgrade slowcode"));
        assert!(transport.calls().is_empty());
    }

    #[tokio::test]
    async fn checksum_mismatch_discards_the_download() {
        let directory = scratch_dir("checksum");
        let exe = directory.join("slowcode");
        fs::write(&exe, b"old binary").expect("old binary");
        let archive = archive_with(b"new binary");
        let transport = FakeTransport::new("v0.2.0", archive, vec![b'0'; 64]);
        let error = install(&request(&transport, &exe, "0.1.0"))
            .await
            .expect_err("mismatch");
        assert!(matches!(error, UpdateError::ChecksumMismatch { .. }));
        assert_eq!(fs::read(&exe).expect("exe"), b"old binary");
        assert!(
            transport
                .calls()
                .iter()
                .any(|call| call.ends_with(".sha256"))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn successful_install_replaces_the_binary_and_keeps_it_executable() {
        let directory = scratch_dir("success");
        let exe = directory.join("slowcode");
        fs::write(&exe, b"old binary").expect("old binary");
        let archive = archive_with(b"new binary");
        let checksum = checksum_for(&archive);
        let transport = FakeTransport::new("v0.2.0", archive, checksum);
        let outcome = install(&request(&transport, &exe, "0.1.0"))
            .await
            .expect("install");
        match outcome {
            InstallOutcome::Updated { path, old, new } => {
                assert_eq!(path, fs::canonicalize(&exe).expect("canonical exe"));
                assert_eq!(old.to_string(), "0.1.0");
                assert_eq!(new.to_string(), "0.2.0");
            }
            InstallOutcome::UpToDate { .. } => panic!("expected an update"),
        }
        assert_eq!(fs::read(&exe).expect("exe"), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&exe).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn same_version_does_not_download() {
        let directory = scratch_dir("uptodate");
        let exe = directory.join("slowcode");
        fs::write(&exe, b"binary").expect("binary");
        let transport = FakeTransport::new("v0.1.0", Vec::new(), Vec::new());
        let outcome = install(&request(&transport, &exe, "0.1.0"))
            .await
            .expect("check");
        assert!(matches!(outcome, InstallOutcome::UpToDate { .. }));
        assert_eq!(transport.calls(), vec!["latest".to_owned()]);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn checksum_parser_accepts_sha256sum_output_and_rejects_junk() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(format!("{digest}  slowcode.tar.gz\n").as_bytes()).expect("digest"),
            digest
        );
        assert!(parse_checksum(b"not-a-digest").is_err());
        assert!(parse_checksum(&[b'a'; 63]).is_err());
    }

    #[test]
    fn unwritable_directories_report_a_clear_error() {
        let directory = Path::new("/opt/slowcode");
        let denied =
            super::map_write_error(io::Error::from(io::ErrorKind::PermissionDenied), directory);
        assert!(matches!(denied, UpdateError::NotWritable { .. }));
        assert!(
            denied
                .to_string()
                .contains("installation directory is not writable")
        );
        let read_only = super::map_write_error(
            io::Error::from(io::ErrorKind::ReadOnlyFilesystem),
            directory,
        );
        assert!(matches!(read_only, UpdateError::NotWritable { .. }));
    }
}
