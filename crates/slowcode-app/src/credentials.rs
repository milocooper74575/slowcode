//! DeepSeek credential onboarding and the workspace `.env` store.
//!
//! Credentials resolve from the workspace `.env` first and then the process
//! environment. The file wins so that `/login` and `/logout` take effect
//! immediately for a running process instead of waiting for a restart.
//!
//! The secret is never formatted, logged or stored anywhere but the workspace
//! `.env`, which is written with mode `0600` through a temporary file and a
//! rename.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use slowcode_kernel::provider::deepseek::{
    CredentialLookup, CredentialRef, CredentialSource, EnvironmentCredentials, SecretValue,
};
use thiserror::Error;

/// Environment variable that carries the DeepSeek API key.
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// Longest key accepted into the workspace `.env`.
const MAX_API_KEY_BYTES: usize = 512;

/// Where the active credential came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialSourceKind {
    /// A key line in the workspace `.env`.
    WorkspaceEnv,
    /// A process environment variable.
    Environment,
    /// No key is available.
    Missing,
}

/// Read-only credential state used by `/login`, `/logout` and startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CredentialStatus {
    pub source: CredentialSourceKind,
    pub env_path: PathBuf,
}

impl CredentialStatus {
    #[must_use]
    pub fn configured(&self) -> bool {
        self.source != CredentialSourceKind::Missing
    }
}

/// Failures while reading or updating the workspace `.env`.
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the API key is empty")]
    Empty,
    #[error("the API key is longer than {MAX_API_KEY_BYTES} bytes")]
    TooLong,
    #[error("the API key must be a single line of printable ASCII")]
    InvalidCharacters,
    #[error("workspace .env: {0}")]
    Io(#[from] std::io::Error),
}

/// Inspect the configured credential without exposing its value.
#[must_use]
pub fn credential_status(workspace: &Path) -> CredentialStatus {
    let env_path = workspace.join(".env");
    let source = if workspace_key(&env_path).is_some() {
        CredentialSourceKind::WorkspaceEnv
    } else if process_key().is_some() {
        CredentialSourceKind::Environment
    } else {
        CredentialSourceKind::Missing
    };
    CredentialStatus { source, env_path }
}

/// Store a validated key in the workspace `.env`, replacing any previous line.
///
/// Other entries and comments are preserved. The write is atomic: a temporary
/// file with mode `0600` is renamed over the target.
pub fn save_api_key(workspace: &Path, value: &str) -> Result<PathBuf, CredentialError> {
    let key = validate_api_key(value)?;
    let env_path = workspace.join(".env");
    let existing = match fs::read_to_string(&env_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let (mut rendered, _) = rewrite_without_key(&existing);
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push_str(API_KEY_ENV);
    rendered.push('=');
    rendered.push_str(key);
    rendered.push('\n');
    write_private(&env_path, &rendered)?;
    Ok(env_path)
}

/// Remove the key line from the workspace `.env`.
///
/// Returns `true` when a line was removed. A key exported in the process
/// environment is untouched.
pub fn remove_api_key(workspace: &Path) -> Result<bool, CredentialError> {
    let env_path = workspace.join(".env");
    let existing = match fs::read_to_string(&env_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let (rendered, removed) = rewrite_without_key(&existing);
    if !removed {
        return Ok(false);
    }
    write_private(&env_path, &rendered)?;
    Ok(true)
}

/// Process environment plus the workspace `.env`, used by every provider.
#[derive(Clone, Debug)]
pub struct WorkspaceCredentials {
    workspace: PathBuf,
    reference: CredentialRef,
}

impl WorkspaceCredentials {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            // The literal is a valid portable identifier.
            reference: CredentialRef::new(API_KEY_ENV).expect("API_KEY_ENV is a valid reference"),
        }
    }
}

impl CredentialSource for WorkspaceCredentials {
    fn resolve(&self, reference: &CredentialRef) -> CredentialLookup {
        if reference != &self.reference {
            return EnvironmentCredentials.resolve(reference);
        }
        if let Some(value) = workspace_key(&self.workspace.join(".env")) {
            return CredentialLookup::Present(SecretValue::new(value));
        }
        EnvironmentCredentials.resolve(reference)
    }
}

fn validate_api_key(value: &str) -> Result<&str, CredentialError> {
    let key = value.trim();
    if key.is_empty() {
        return Err(CredentialError::Empty);
    }
    if key.len() > MAX_API_KEY_BYTES {
        return Err(CredentialError::TooLong);
    }
    if !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(CredentialError::InvalidCharacters);
    }
    Ok(key)
}

fn process_key() -> Option<String> {
    match std::env::var_os(API_KEY_ENV) {
        Some(value) if !value.is_empty() => value.into_string().ok(),
        _ => None,
    }
}

fn workspace_key(env_path: &Path) -> Option<String> {
    let contents = fs::read_to_string(env_path).ok()?;
    contents.lines().find_map(key_value)
}

/// The value of a `DEEPSEEK_API_KEY` line, honoring `export` and optional
/// matching quotes.
fn key_value(line: &str) -> Option<String> {
    let line = line.trim();
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (name, value) = line.split_once('=')?;
    if name.trim() != API_KEY_ENV {
        return None;
    }
    let value = unquote(value.trim());
    (!value.is_empty()).then(|| value.to_owned())
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Remove every key line while leaving the other bytes intact.
///
/// Returns the rewritten text and whether any line was removed.
fn rewrite_without_key(existing: &str) -> (String, bool) {
    let mut rendered = String::with_capacity(existing.len());
    let mut removed = false;
    for line in existing.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\r', '\n']);
        if key_line(bare) {
            removed = true;
            continue;
        }
        rendered.push_str(line);
    }
    if removed && !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    (rendered, removed)
}

fn key_line(line: &str) -> bool {
    let line = line.trim();
    let line = line.strip_prefix("export ").unwrap_or(line);
    line.split_once('=')
        .is_some_and(|(name, _)| name.trim() == API_KEY_ENV)
}

/// Write `contents` to `path` atomically with owner-only permissions.
fn write_private(path: &Path, contents: &str) -> Result<(), CredentialError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".env");
    let temporary = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    // A leftover from a crashed run would block create_new; it is never live.
    let _ = fs::remove_file(&temporary);

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            File::open(&temporary)?.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(CredentialError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "slowcode-credentials-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace");
        path
    }

    #[test]
    fn saves_a_key_with_owner_only_permissions_and_preserves_other_lines() {
        let workspace = temp_workspace("save");
        fs::write(
            workspace.join(".env"),
            "# local config\nOTHER=value\nDEEPSEEK_API_KEY=old\n",
        )
        .expect("seed");

        let path = save_api_key(&workspace, "  sk-new-key  ").expect("save");

        let contents = fs::read_to_string(&path).expect("read");
        assert_eq!(
            contents,
            "# local config\nOTHER=value\nDEEPSEEK_API_KEY=sk-new-key\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(
            credential_status(&workspace).source,
            CredentialSourceKind::WorkspaceEnv
        );
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn saving_twice_keeps_exactly_one_key_line() {
        let workspace = temp_workspace("replace");
        save_api_key(&workspace, "sk-first").expect("first");
        let path = save_api_key(&workspace, "sk-second").expect("second");

        let contents = fs::read_to_string(&path).expect("read");
        assert_eq!(contents.matches(API_KEY_ENV).count(), 1);
        assert!(contents.contains("DEEPSEEK_API_KEY=sk-second"));
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn removes_exported_and_quoted_key_lines_only() {
        let workspace = temp_workspace("remove");
        fs::write(
            workspace.join(".env"),
            "A=1\nexport DEEPSEEK_API_KEY=\"sk-quoted\"\nB=2\n",
        )
        .expect("seed");

        assert!(remove_api_key(&workspace).expect("remove"));
        let contents = fs::read_to_string(workspace.join(".env")).expect("read");
        assert_eq!(contents, "A=1\nB=2\n");
        assert!(!remove_api_key(&workspace).expect("idempotent"));

        fs::write(workspace.join(".env"), "A=1").expect("no trailing newline");
        assert!(!remove_api_key(&workspace).expect("nothing to remove"));
        assert_eq!(
            fs::read_to_string(workspace.join(".env")).expect("read"),
            "A=1"
        );
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn rejects_unusable_keys() {
        let workspace = temp_workspace("invalid");
        assert!(matches!(
            save_api_key(&workspace, "   "),
            Err(CredentialError::Empty)
        ));
        assert!(matches!(
            save_api_key(&workspace, "two words"),
            Err(CredentialError::InvalidCharacters)
        ));
        assert!(matches!(
            save_api_key(&workspace, &"k".repeat(MAX_API_KEY_BYTES + 1)),
            Err(CredentialError::TooLong)
        ));
        assert!(!workspace.join(".env").exists());
        fs::remove_dir_all(workspace).ok();
    }

    #[test]
    fn workspace_env_is_read_before_the_process_environment() {
        let workspace = temp_workspace("source");
        fs::write(workspace.join(".env"), "DEEPSEEK_API_KEY=sk-file\n").expect("seed");
        let credentials = WorkspaceCredentials::new(&workspace);
        let reference = CredentialRef::new(API_KEY_ENV).expect("reference");

        assert_eq!(
            workspace_key(&workspace.join(".env")).as_deref(),
            Some("sk-file")
        );
        assert!(matches!(
            credentials.resolve(&reference),
            CredentialLookup::Present(_)
        ));
        fs::remove_dir_all(workspace).ok();
    }
}
