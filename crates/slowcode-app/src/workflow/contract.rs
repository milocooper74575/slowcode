//! Scaffold contract derivation and verification.
//!
//! The model only produces files plus a manifest. The contract is derived from
//! the workspace: the system decides which files are locked, hashes them, and
//! runs the project tests to learn which tests are expected to flip. A model
//! cannot assert its own contract.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    state::{CoreRegion, ScaffoldContract, TestContract},
    test_exec::{ProjectTestConfig, TestRun, validate_selector},
};

/// Marker a scaffold leaves in code the user must implement.
pub const CORE_MARKER: &str = "TODO(user)";

/// One manifest row reduced to the fields the contract needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestFile {
    /// Workspace-relative path exactly as the manifest listed it.
    pub path: String,
    /// Human-readable blank location, used as the core region symbol label.
    pub blank: String,
}

impl ManifestFile {
    #[must_use]
    pub fn new(path: impl Into<String>, blank: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            blank: blank.into(),
        }
    }
}

/// Contract derivation failure. The message is user-facing.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ContractError {
    #[error("scaffold manifest is empty")]
    EmptyManifest,
    #[error("scaffold file is missing: {0}")]
    MissingFile(String),
    #[error("scaffold file escapes the workspace: {0}")]
    EscapesWorkspace(String),
    #[error("scaffold changed files that the manifest does not list: {0}")]
    UnlistedChanges(String),
    #[error("scaffold file {file} has {count} `{marker}` markers; expected exactly one")]
    MarkerCount {
        file: String,
        count: usize,
        marker: &'static str,
    },
    #[error("scaffold contains no `{0}` core region")]
    NoCoreRegion(&'static str),
    #[error("scaffold produced no failing test; a user-owned contract must start red")]
    NoExpectedFailures,
    #[error("scaffold tests did not fail as expected: {0}")]
    UnexpectedGreen(String),
    #[error("core file {0} still contains `TODO(user)`")]
    MarkerRemaining(String),
    #[error("workspace file changed after the contract was recorded: {0}")]
    HashMismatch(String),
    #[error("test contract is missing; run /approve or /scaffold first")]
    MissingTestContract,
    #[error("test configuration error: {0}")]
    TestConfig(String),
    #[error("workspace io error: {0}")]
    Io(String),
}

impl From<io::Error> for ContractError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Files whose names follow a test convention. Used for reporting only; every
/// non-core scaffold file is hash-locked regardless.
#[must_use]
pub fn looks_like_test_file(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    path.starts_with("tests/")
        || path.starts_with("test/")
        || path.contains("/tests/")
        || path.contains("/test/")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with("_tests.rs")
        || name.ends_with("_spec.rs")
}

/// Derive and validate the scaffold contract.
///
/// `changed` is the list of workspace-relative files created or modified since
/// the pre-scaffold snapshot. Every one of them must appear in the manifest.
#[allow(clippy::too_many_arguments)]
pub fn build_contract(
    workspace: &Path,
    manifest: &[ManifestFile],
    changed: &[String],
    project: &ProjectTestConfig,
    selector: &str,
    baseline: &TestRun,
    after: &TestRun,
) -> Result<ScaffoldContract, ContractError> {
    if manifest.is_empty() {
        return Err(ContractError::EmptyManifest);
    }
    validate_selector(project, selector)
        .map_err(|error| ContractError::TestConfig(error.to_string()))?;

    let mut paths = Vec::with_capacity(manifest.len());
    for entry in manifest {
        let path = resolve_workspace_file(workspace, &entry.path)?;
        if !path.is_file() {
            return Err(ContractError::MissingFile(entry.path.clone()));
        }
        paths.push((entry, path));
    }

    let listed: BTreeSet<&str> = manifest.iter().map(|entry| entry.path.as_str()).collect();
    let unlisted: Vec<&String> = changed
        .iter()
        .filter(|path| !listed.contains(path.as_str()))
        .collect();
    if !unlisted.is_empty() {
        let joined = unlisted
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ContractError::UnlistedChanges(joined));
    }

    let mut core_regions = Vec::new();
    let mut framework_files = Vec::new();
    let mut framework_hashes = BTreeMap::new();
    for (entry, path) in &paths {
        let bytes = fs::read(path)?;
        let count = marker_count(&bytes);
        if count > 1 {
            return Err(ContractError::MarkerCount {
                file: entry.path.clone(),
                count,
                marker: CORE_MARKER,
            });
        }
        if count == 1 {
            core_regions.push(CoreRegion {
                file: entry.path.clone(),
                symbol: entry.blank.clone(),
                marker: CORE_MARKER.to_owned(),
            });
        } else {
            framework_files.push(entry.path.clone());
            framework_hashes.insert(entry.path.clone(), sha256_hex(&bytes));
        }
    }
    if core_regions.is_empty() {
        return Err(ContractError::NoCoreRegion(CORE_MARKER));
    }

    let test_files = framework_files
        .iter()
        .filter(|path| looks_like_test_file(path))
        .cloned()
        .collect();

    let baseline_failures: BTreeSet<&str> = baseline.failures.iter().map(String::as_str).collect();
    let expected_failing_tests: Vec<String> = after
        .failures
        .iter()
        .filter(|test| !baseline_failures.contains(test.as_str()))
        .cloned()
        .collect();
    if expected_failing_tests.is_empty() {
        if after.succeeded() {
            return Err(ContractError::NoExpectedFailures);
        }
        return Err(ContractError::UnexpectedGreen(
            "the failing tests already failed before scaffolding".to_owned(),
        ));
    }
    let failing: BTreeSet<&str> = after.failures.iter().map(String::as_str).collect();
    for test in &expected_failing_tests {
        if !failing.contains(test.as_str()) {
            return Err(ContractError::UnexpectedGreen(test.clone()));
        }
    }

    let mut expected_passing_tests: Vec<String> = after
        .passed
        .iter()
        .filter(|test| !expected_failing_tests.contains(test))
        .cloned()
        .collect();
    expected_passing_tests.sort();
    expected_passing_tests.dedup();

    Ok(ScaffoldContract {
        framework_files,
        framework_hashes,
        test_files,
        core_regions,
        project_test_command: project.command.clone(),
        test_selector: selector.trim().to_owned(),
        expected_failing_tests,
        expected_passing_tests,
    })
}

/// Build the contract for an all-known workflow at Review approval time.
///
/// There are no core regions, and the baseline becomes the passing set. A red
/// baseline is rejected by the caller before this runs.
pub fn build_review_contract(
    project: &ProjectTestConfig,
    selector: &str,
    baseline: &TestRun,
) -> Result<ScaffoldContract, ContractError> {
    validate_selector(project, selector)
        .map_err(|error| ContractError::TestConfig(error.to_string()))?;
    let mut expected_passing_tests = baseline.passed.clone();
    expected_passing_tests.sort();
    expected_passing_tests.dedup();
    Ok(ScaffoldContract {
        framework_files: Vec::new(),
        framework_hashes: BTreeMap::new(),
        test_files: Vec::new(),
        core_regions: Vec::new(),
        project_test_command: project.command.clone(),
        test_selector: selector.trim().to_owned(),
        expected_failing_tests: Vec::new(),
        expected_passing_tests,
    })
}

/// Reject every core file that still holds the marker.
pub fn verify_core_implemented(
    workspace: &Path,
    contract: &ScaffoldContract,
) -> Result<(), ContractError> {
    for region in &contract.core_regions {
        let path = resolve_workspace_file(workspace, &region.file)?;
        if !path.is_file() {
            return Err(ContractError::MissingFile(region.file.clone()));
        }
        let bytes = fs::read(&path)?;
        if marker_count(&bytes) > 0 {
            return Err(ContractError::MarkerRemaining(region.file.clone()));
        }
    }
    Ok(())
}

/// Reject framework or test files that changed after the contract was recorded.
pub fn verify_hashes(workspace: &Path, contract: &ScaffoldContract) -> Result<(), ContractError> {
    for file in &contract.framework_files {
        let path = resolve_workspace_file(workspace, file)?;
        if !path.is_file() {
            return Err(ContractError::MissingFile(file.clone()));
        }
        let bytes = fs::read(&path)?;
        let expected = contract.framework_hashes.get(file);
        if expected != Some(&sha256_hex(&bytes)) {
            return Err(ContractError::HashMismatch(file.clone()));
        }
    }
    for file in &contract.test_files {
        if !contract.framework_files.contains(file) {
            return Err(ContractError::HashMismatch(file.clone()));
        }
    }
    Ok(())
}

/// Reject framework or test files that changed after a `TestContract` was
/// recorded.
///
/// Review-only workflows have no locked files, so the loops are empty. Scaffold
/// workflows reuse the same hashes as `verify_hashes`, which lets TestFix lock
/// files without requiring the full scaffold contract.
pub fn verify_test_contract_hashes(
    workspace: &Path,
    contract: &TestContract,
) -> Result<(), ContractError> {
    for (file, expected) in &contract.framework_hashes {
        let path = resolve_workspace_file(workspace, file)?;
        if !path.is_file() {
            return Err(ContractError::MissingFile(file.clone()));
        }
        let bytes = fs::read(&path)?;
        if expected != &sha256_hex(&bytes) {
            return Err(ContractError::HashMismatch(file.clone()));
        }
    }
    for file in &contract.test_files {
        if !contract.framework_hashes.contains_key(file) {
            return Err(ContractError::HashMismatch(file.clone()));
        }
    }
    Ok(())
}

/// Check that the expected failing tests no longer fail and the expected
/// passing tests still pass.
pub fn verify_test_flip(contract: &ScaffoldContract, run: &TestRun) -> Result<(), ContractError> {
    if !run.succeeded() {
        let failures = if run.failures.is_empty() {
            run.output
                .lines()
                .last()
                .unwrap_or("tests did not pass")
                .to_owned()
        } else {
            run.failures.join(", ")
        };
        return Err(ContractError::UnexpectedGreen(failures));
    }
    for test in &contract.expected_failing_tests {
        if run.failures.iter().any(|failure| failure == test) {
            return Err(ContractError::UnexpectedGreen(test.clone()));
        }
    }
    for test in &contract.expected_passing_tests {
        if run.failures.iter().any(|failure| failure == test) {
            return Err(ContractError::UnexpectedGreen(test.clone()));
        }
    }
    Ok(())
}

/// One entry of a cheap pre/post scaffold snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStamp {
    pub size: u64,
    pub modified: u128,
}

/// Workspace-relative files whose size or mtime changed between snapshots.
#[must_use]
pub fn changed_files(
    before: &BTreeMap<String, FileStamp>,
    after: &BTreeMap<String, FileStamp>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (path, stamp) in after {
        if before.get(path) != Some(stamp) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed
}

/// Capture size and mtime for workspace files, skipping build and tool output.
pub fn snapshot_workspace(workspace: &Path) -> Result<BTreeMap<String, FileStamp>, ContractError> {
    let root = workspace.canonicalize()?;
    let mut snapshot = BTreeMap::new();
    walk(&root, &root, &mut snapshot)?;
    Ok(snapshot)
}

fn walk(
    root: &Path,
    directory: &Path,
    snapshot: &mut BTreeMap<String, FileStamp>,
) -> Result<(), ContractError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if is_excluded_dir(&path) {
                continue;
            }
            walk(root, &path, snapshot)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        let relative = path.strip_prefix(root).unwrap_or(&path);
        snapshot.insert(
            relative.to_string_lossy().replace('\\', "/"),
            FileStamp {
                size: metadata.len(),
                modified,
            },
        );
    }
    Ok(())
}

fn is_excluded_dir(path: &Path) -> bool {
    super::is_scan_excluded(path)
}

fn resolve_workspace_file(workspace: &Path, file: &str) -> Result<PathBuf, ContractError> {
    let cleaned = file.trim().trim_matches('`').trim();
    let candidate = Path::new(cleaned);
    if candidate.is_absolute() {
        return Err(ContractError::EscapesWorkspace(file.to_owned()));
    }
    let joined = workspace.join(candidate);
    let root = workspace.canonicalize()?;
    let full = joined
        .canonicalize()
        .map_err(|_| ContractError::MissingFile(file.to_owned()))?;
    if !full.starts_with(&root) {
        return Err(ContractError::EscapesWorkspace(file.to_owned()));
    }
    Ok(full)
}

fn marker_count(bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(bytes);
    text.matches(CORE_MARKER).count()
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::state::TestStatus;

    fn temp_workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slowcode-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(path.join("src")).expect("workspace");
        fs::create_dir_all(path.join("tests")).expect("tests directory");
        path
    }

    fn project() -> ProjectTestConfig {
        ProjectTestConfig {
            runner: super::super::test_exec::Runner::Cargo,
            command: "cargo test".to_owned(),
        }
    }

    fn run(status: TestStatus, failures: &[&str], passed: &[&str]) -> TestRun {
        TestRun {
            command: "cargo test".to_owned(),
            status,
            exit_code: Some(if status == TestStatus::Passed { 0 } else { 101 }),
            duration_ms: 1,
            failures: failures.iter().map(|test| (*test).to_owned()).collect(),
            passed: passed.iter().map(|test| (*test).to_owned()).collect(),
            output: String::new(),
        }
    }

    #[test]
    fn derives_core_regions_and_locks_framework_files() {
        let workspace = temp_workspace();
        fs::write(
            workspace.join("src/parse.rs"),
            "pub fn parse() {\n    TODO(user)\n}\n",
        )
        .expect("core file");
        fs::write(
            workspace.join("tests/parse_test.rs"),
            "#[test]\nfn parses() {}\n",
        )
        .expect("test file");
        let manifest = vec![
            ManifestFile::new("src/parse.rs", "parse"),
            ManifestFile::new("tests/parse_test.rs", ""),
        ];
        let contract = build_contract(
            &workspace,
            &manifest,
            &[],
            &project(),
            "",
            &run(TestStatus::Passed, &[], &[]),
            &run(TestStatus::Failed, &["parses"], &[]),
        )
        .expect("contract builds");
        assert_eq!(contract.core_regions.len(), 1);
        assert_eq!(contract.framework_files, ["tests/parse_test.rs"]);
        assert_eq!(contract.test_files, ["tests/parse_test.rs"]);
        assert_eq!(contract.expected_failing_tests, ["parses"]);
        assert!(
            contract
                .framework_hashes
                .contains_key("tests/parse_test.rs")
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn unlisted_changes_invalidate_the_contract() {
        let workspace = temp_workspace();
        fs::write(workspace.join("src/parse.rs"), "TODO(user)\n").expect("core");
        let error = build_contract(
            &workspace,
            &[ManifestFile::new("src/parse.rs", "parse")],
            &["src/stray.rs".to_owned()],
            &project(),
            "",
            &run(TestStatus::Passed, &[], &[]),
            &run(TestStatus::Failed, &["parses"], &[]),
        )
        .expect_err("unlisted change is rejected");
        assert!(error.to_string().contains("stray.rs"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn multiple_markers_invalidate_the_contract() {
        let workspace = temp_workspace();
        fs::write(workspace.join("src/parse.rs"), "TODO(user)\nTODO(user)\n").expect("core");
        let error = build_contract(
            &workspace,
            &[ManifestFile::new("src/parse.rs", "parse")],
            &[],
            &project(),
            "",
            &run(TestStatus::Passed, &[], &[]),
            &run(TestStatus::Failed, &["parses"], &[]),
        )
        .expect_err("two markers are rejected");
        assert!(error.to_string().contains("2"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_green_scaffold_is_rejected() {
        let workspace = temp_workspace();
        fs::write(workspace.join("src/parse.rs"), "TODO(user)\n").expect("core");
        let error = build_contract(
            &workspace,
            &[ManifestFile::new("src/parse.rs", "parse")],
            &[],
            &project(),
            "",
            &run(TestStatus::Passed, &[], &[]),
            &run(TestStatus::Passed, &[], &[]),
        )
        .expect_err("a scaffold must start red");
        assert!(error.to_string().contains("start red"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn core_marker_must_be_removed_before_continue() {
        let workspace = temp_workspace();
        fs::write(
            workspace.join("src/parse.rs"),
            "fn parse() { TODO(user) }\n",
        )
        .expect("core");
        let contract = ScaffoldContract {
            core_regions: vec![CoreRegion {
                file: "src/parse.rs".to_owned(),
                symbol: "parse".to_owned(),
                marker: CORE_MARKER.to_owned(),
            }],
            ..ScaffoldContract::default()
        };
        assert!(verify_core_implemented(&workspace, &contract).is_err());
        fs::write(workspace.join("src/parse.rs"), "fn parse() { 1 }\n").expect("implemented");
        verify_core_implemented(&workspace, &contract).expect("marker is gone");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn framework_hash_mismatch_is_reported() {
        let workspace = temp_workspace();
        fs::write(workspace.join("tests/parse_test.rs"), "one").expect("test file");
        let hash = sha256_hex(b"one");
        let contract = ScaffoldContract {
            framework_files: vec!["tests/parse_test.rs".to_owned()],
            framework_hashes: BTreeMap::from([("tests/parse_test.rs".to_owned(), hash)]),
            test_files: vec!["tests/parse_test.rs".to_owned()],
            ..ScaffoldContract::default()
        };
        verify_hashes(&workspace, &contract).expect("hash matches");
        fs::write(workspace.join("tests/parse_test.rs"), "two").expect("changed test file");
        assert!(verify_hashes(&workspace, &contract).is_err());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn snapshots_report_created_files() {
        let workspace = temp_workspace();
        let before = snapshot_workspace(&workspace).expect("snapshot");
        fs::write(workspace.join("src/new.rs"), "new").expect("new file");
        let after = snapshot_workspace(&workspace).expect("snapshot");
        assert_eq!(changed_files(&before, &after), ["src/new.rs"]);
        let _ = fs::remove_dir_all(&workspace);
    }
}
