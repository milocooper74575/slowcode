//! `@file` input snapshots for workflow answers.
//!
//! Only workspace-relative, single, UTF-8 files are accepted. The persisted
//! snapshot is redacted; callers decide whether a secret hit is rejected or
//! allowed after the user confirms.

use std::{
    fs,
    path::{Component, Path},
};

use regex::Regex;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::state::AnswerSnapshot;

pub const MAX_INPUT_BYTES: u64 = 256 * 1024;

/// What to do when a secret is detected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretPolicy {
    Reject,
    AllowRedacted,
}

/// A read answer file and its redacted snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputSnapshot {
    pub text: String,
    pub snapshot: AnswerSnapshot,
}

/// `@file` failures. Messages are user-facing.
#[derive(Debug, Error)]
pub enum InputError {
    #[error("@file must be a workspace-relative path")]
    NotRelative,
    #[error("@file path escapes the workspace")]
    EscapesWorkspace,
    #[error("@file is empty: {0}")]
    Empty(String),
    #[error("@file is binary or not UTF-8: {0}")]
    Binary(String),
    #[error("@file is too large: {0} bytes, limit is {MAX_INPUT_BYTES} bytes")]
    TooLarge(u64),
    #[error("@file contains a likely secret; use text or confirm redaction")]
    SecretDetected,
    #[error("@file does not exist: {0}")]
    Missing(String),
    #[error("@file io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read one answer file, scan it, and persist a redacted snapshot.
pub fn read_answer_file(
    workspace: &Path,
    workflow_id: &str,
    relative: &str,
    policy: SecretPolicy,
) -> Result<InputSnapshot, InputError> {
    let relative = relative.trim();
    let candidate = Path::new(relative);
    if relative.is_empty() || candidate.is_absolute() {
        return Err(InputError::NotRelative);
    }
    if candidate
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(InputError::EscapesWorkspace);
    }

    let root = workspace
        .canonicalize()
        .map_err(|_| InputError::Missing(relative.to_owned()))?;
    let full = workspace.join(candidate);
    let canonical = full.canonicalize().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => InputError::Missing(relative.to_owned()),
        _ => InputError::Io(error),
    })?;
    if !canonical.starts_with(&root) {
        return Err(InputError::EscapesWorkspace);
    }
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() {
        return Err(InputError::Binary(relative.to_owned()));
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(InputError::TooLarge(metadata.len()));
    }
    if metadata.len() == 0 {
        return Err(InputError::Empty(relative.to_owned()));
    }

    let bytes = fs::read(&canonical)?;
    if bytes.contains(&0) {
        return Err(InputError::Binary(relative.to_owned()));
    }
    let text =
        String::from_utf8(bytes.clone()).map_err(|_| InputError::Binary(relative.to_owned()))?;
    let (redacted, redactions) = redact_secrets(&text, workspace);
    if redactions > 0 && policy == SecretPolicy::Reject {
        return Err(InputError::SecretDetected);
    }

    let sha256 = hex_digest(&bytes);
    let directory = workspace
        .join(".scratch")
        .join("workflows")
        .join(workflow_id)
        .join("inputs");
    fs::create_dir_all(&directory)?;
    let name = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("input");
    let snapshot_path = directory.join(format!("{sha256}-{}", safe_name(name)));
    fs::write(&snapshot_path, &redacted)?;

    Ok(InputSnapshot {
        text: redacted,
        snapshot: AnswerSnapshot {
            path: relative.to_owned(),
            sha256,
            size: metadata.len(),
            redactions,
        },
    })
}

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

/// Replace known environment values and secret patterns with a marker.
pub(crate) fn redact_secrets(text: &str, workspace: &Path) -> (String, u64) {
    let mut redacted = text.to_owned();
    let mut count = 0_u64;
    for (value, kind) in known_secrets(workspace) {
        if value.len() < 8 {
            continue;
        }
        let hits = redacted.matches(&value).count() as u64;
        if hits > 0 {
            redacted = redacted.replace(&value, &format!("[REDACTED:{kind}]"));
            count += hits;
        }
    }
    let patterns = secret_patterns();
    for (regex, kind) in patterns {
        let hits = regex.find_iter(&redacted).count() as u64;
        if hits > 0 {
            redacted = regex
                .replace_all(&redacted, format!("[REDACTED:{kind}]"))
                .into_owned();
            count += hits;
        }
    }
    (redacted, count)
}

fn known_secrets(workspace: &Path) -> Vec<(String, &'static str)> {
    let mut secrets = Vec::new();
    if let Ok(value) = std::env::var("DEEPSEEK_API_KEY") {
        secrets.push((value, "env"));
    }
    if let Ok(contents) = fs::read_to_string(workspace.join(".env")) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((_, value)) = line.split_once('=') {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    secrets.push((value.to_owned(), "env"));
                }
            }
        }
    }
    secrets
}

fn secret_patterns() -> Vec<(Regex, &'static str)> {
    let patterns = [
        (r"sk-[A-Za-z0-9_-]{16,}", "api_key"),
        (r"ghp_[A-Za-z0-9]{20,}", "github_token"),
        (r"github_pat_[A-Za-z0-9_]{20,}", "github_token"),
        (r"xox[baprs]-[A-Za-z0-9-]{10,}", "slack_token"),
        (r"AKIA[0-9A-Z]{16}", "aws_key"),
        (r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", "jwt"),
        (
            r#"(?i)(api[_-]?key|token|secret)\s*[:=]\s*['"]?([A-Za-z0-9_\-]{16,})"#,
            "secret_assignment",
        ),
    ];
    patterns
        .into_iter()
        .map(|(pattern, kind)| {
            (
                Regex::new(pattern).expect("built-in secret pattern compiles"),
                kind,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-input-{}-{nonce}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace");
        path
    }

    #[test]
    fn reads_a_text_file_and_writes_a_snapshot() {
        let workspace = temp_workspace();
        fs::write(workspace.join("notes.md"), "缓存选 A\n").expect("notes");
        let input = read_answer_file(&workspace, "plan-1", "notes.md", SecretPolicy::Reject)
            .expect("file is accepted");
        assert_eq!(input.text, "缓存选 A\n");
        assert_eq!(input.snapshot.path, "notes.md");
        assert_eq!(input.snapshot.redactions, 0);
        assert_eq!(input.snapshot.size, "缓存选 A\n".len() as u64);
        let snapshot = workspace
            .join(".scratch/workflows/plan-1/inputs")
            .join(format!("{}-notes.md", input.snapshot.sha256));
        assert!(snapshot.exists());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn rejects_absolute_and_escaping_paths() {
        let workspace = temp_workspace();
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "/etc/passwd", SecretPolicy::Reject),
            Err(InputError::NotRelative)
        ));
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "../outside.md", SecretPolicy::Reject),
            Err(InputError::EscapesWorkspace)
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn rejects_empty_and_binary_files() {
        let workspace = temp_workspace();
        fs::write(workspace.join("empty.md"), "").expect("empty");
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "empty.md", SecretPolicy::Reject),
            Err(InputError::Empty(_))
        ));
        fs::write(workspace.join("binary.md"), [0_u8, 1, 2]).expect("binary");
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "binary.md", SecretPolicy::Reject),
            Err(InputError::Binary(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn rejects_oversized_files() {
        let workspace = temp_workspace();
        fs::write(
            workspace.join("large.md"),
            vec![b'a'; (MAX_INPUT_BYTES + 1) as usize],
        )
        .expect("large");
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "large.md", SecretPolicy::Reject),
            Err(InputError::TooLarge(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn rejects_secrets_and_redacts_when_allowed() {
        let workspace = temp_workspace();
        fs::write(workspace.join(".env"), "SERVICE_TOKEN=abcdef1234567890\n").expect(".env");
        fs::write(
            workspace.join("notes.md"),
            "token: abcdef1234567890\napi sk-abcdefghijklmnopqrstuvwxyz\n",
        )
        .expect("notes");
        assert!(matches!(
            read_answer_file(&workspace, "plan-1", "notes.md", SecretPolicy::Reject),
            Err(InputError::SecretDetected)
        ));
        let input = read_answer_file(
            &workspace,
            "plan-1",
            "notes.md",
            SecretPolicy::AllowRedacted,
        )
        .expect("redaction is allowed");
        assert!(input.snapshot.redactions >= 2, "{:?}", input.snapshot);
        assert!(!input.text.contains("abcdef1234567890"), "{}", input.text);
        assert!(!input.text.contains("sk-abc"), "{}", input.text);
        let _ = fs::remove_dir_all(&workspace);
    }
}
