//! Workflow token budget.
//!
//! The only user-tunable limit is the total output token budget for one
//! workflow. Internal safety limits live in code: a per-turn output cap, a
//! repeated-failure detector, and an informational active-time counter.
//!
//! The file is strict: unknown keys and zero limits are rejected. Legacy
//! `[test_fix]` and `[workflow]` keys are accepted but ignored so an older
//! workspace file does not turn into a hard failure.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

/// Default total output tokens for one workflow.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 2_000_000;
/// Internal per-turn output safety cap. Not user configurable.
pub const MAX_OUTPUT_TOKENS_PER_TURN: u64 = 60_000;
/// Internal repeated-failure limit before a workflow blocks. Not configurable.
pub const MAX_CONSECUTIVE_SAME_FAILURE: u32 = 3;
/// Internal active-time value. Recorded and displayed, never a hard block.
pub const MAX_ACTIVE_SECONDS: u64 = 7_200;

/// Where one resolved limit came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetSource {
    Default,
    File,
}

impl BudgetSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
        }
    }
}

/// One resolved limit and its source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetValue {
    pub value: u64,
    pub source: BudgetSource,
}

impl BudgetValue {
    fn default_value(value: u64) -> Self {
        Self {
            value,
            source: BudgetSource::Default,
        }
    }

    fn from_file(value: u64) -> Self {
        Self {
            value,
            source: BudgetSource::File,
        }
    }
}

/// Effective limits for one workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowBudget {
    pub max_output_tokens: BudgetValue,
    /// Removed fields found in an older config file. Accepted but ignored.
    pub legacy_keys: Vec<String>,
}

impl Default for WorkflowBudget {
    fn default() -> Self {
        Self {
            max_output_tokens: BudgetValue::default_value(DEFAULT_MAX_OUTPUT_TOKENS),
            legacy_keys: Vec::new(),
        }
    }
}

impl WorkflowBudget {
    /// Load the workspace budget, applying only fields present in the file.
    pub fn load(workspace: &Path) -> Result<Self, BudgetError> {
        let path = config_path(workspace);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(BudgetError::Io { path, error }),
        };
        let file: FileBudget = toml::from_str(&text).map_err(|error| BudgetError::Parse {
            path: path.clone(),
            message: error.to_string(),
        })?;
        Self::from_file(file).map_err(|message| BudgetError::Parse { path, message })
    }

    fn from_file(file: FileBudget) -> Result<Self, String> {
        let defaults = Self::default();
        let mut legacy_keys = Vec::new();
        file.test_fix.collect_legacy(&mut legacy_keys);
        file.workflow.collect_legacy(&mut legacy_keys);
        Ok(Self {
            max_output_tokens: resolve(
                "budget.max_output_tokens",
                file.budget.max_output_tokens,
                defaults.max_output_tokens,
            )?,
            legacy_keys,
        })
    }

    /// Render the effective values and their sources for `/status`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "budget:\n  max_output_tokens: {} ({})",
            self.max_output_tokens.value,
            self.max_output_tokens.source.as_str(),
        );
        if !self.legacy_keys.is_empty() {
            text.push_str(&format!(
                "\n  ignored legacy budget keys: {}",
                self.legacy_keys.join(", ")
            ));
        }
        text
    }
}

fn resolve(name: &str, value: Option<u64>, default: BudgetValue) -> Result<BudgetValue, String> {
    match value {
        None => Ok(default),
        Some(0) => Err(format!("{name} must be greater than zero")),
        Some(value) => Ok(BudgetValue::from_file(value)),
    }
}

fn config_path(workspace: &Path) -> PathBuf {
    workspace.join(".slowcode").join("workflow.toml")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileBudget {
    #[serde(default)]
    budget: BudgetFile,
    #[serde(default)]
    test_fix: LegacyTestFixFile,
    #[serde(default)]
    workflow: LegacyWorkflowFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetFile {
    max_output_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyTestFixFile {
    max_rounds: Option<u64>,
    max_model_calls_per_round: Option<u64>,
    max_output_tokens_per_round: Option<u64>,
    max_consecutive_same_failure: Option<u64>,
}

impl LegacyTestFixFile {
    fn collect_legacy(&self, legacy: &mut Vec<String>) {
        for (name, value) in [
            ("test_fix.max_rounds", self.max_rounds),
            (
                "test_fix.max_model_calls_per_round",
                self.max_model_calls_per_round,
            ),
            (
                "test_fix.max_output_tokens_per_round",
                self.max_output_tokens_per_round,
            ),
            (
                "test_fix.max_consecutive_same_failure",
                self.max_consecutive_same_failure,
            ),
        ] {
            if value.is_some() {
                legacy.push(name.to_owned());
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyWorkflowFile {
    max_active_seconds: Option<u64>,
}

impl LegacyWorkflowFile {
    fn collect_legacy(&self, legacy: &mut Vec<String>) {
        if self.max_active_seconds.is_some() {
            legacy.push("workflow.max_active_seconds".to_owned());
        }
    }
}

/// Budget configuration errors. Both variants are hard errors.
#[derive(Debug, Error)]
pub enum BudgetError {
    #[error("could not read workflow budget {}: {error}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("invalid workflow budget {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-budget-{}-{nonce}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(path.join(".slowcode")).expect("workspace is created");
        path
    }

    fn write_config(workspace: &Path, text: &str) {
        fs::write(config_path(workspace), text).expect("config is written");
    }

    #[test]
    fn defaults_are_used_without_a_file() {
        let workspace = temp_workspace();
        let budget = WorkflowBudget::load(&workspace).expect("defaults load");
        assert_eq!(budget.max_output_tokens.value, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(budget.max_output_tokens.source, BudgetSource::Default);
        assert!(budget.legacy_keys.is_empty());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn budget_overrides_the_default() {
        let workspace = temp_workspace();
        write_config(&workspace, "[budget]\nmax_output_tokens = 400\n");
        let budget = WorkflowBudget::load(&workspace).expect("config loads");
        assert_eq!(budget.max_output_tokens.value, 400);
        assert_eq!(budget.max_output_tokens.source, BudgetSource::File);
        assert!(budget.render().contains("max_output_tokens: 400 (file)"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn legacy_keys_are_ignored_and_reported() {
        let workspace = temp_workspace();
        write_config(
            &workspace,
            "[test_fix]\nmax_rounds = 4\nmax_model_calls_per_round = 8\nmax_output_tokens_per_round = 60000\nmax_consecutive_same_failure = 3\n[workflow]\nmax_active_seconds = 120\n",
        );
        let budget = WorkflowBudget::load(&workspace).expect("legacy config loads");
        assert_eq!(budget.max_output_tokens.value, DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(
            budget.legacy_keys,
            vec![
                "test_fix.max_rounds".to_owned(),
                "test_fix.max_model_calls_per_round".to_owned(),
                "test_fix.max_output_tokens_per_round".to_owned(),
                "test_fix.max_consecutive_same_failure".to_owned(),
                "workflow.max_active_seconds".to_owned(),
            ]
        );
        let rendered = budget.render();
        assert!(
            rendered.contains("ignored legacy budget keys"),
            "{rendered}"
        );
        assert!(rendered.contains("test_fix.max_rounds"), "{rendered}");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let workspace = temp_workspace();
        write_config(&workspace, "[budget]\nmax_output_tokenz = 4\n");
        let error = WorkflowBudget::load(&workspace).expect_err("unknown key is rejected");
        assert!(error.to_string().contains("unknown field"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn zero_limits_are_rejected() {
        let workspace = temp_workspace();
        write_config(&workspace, "[budget]\nmax_output_tokens = 0\n");
        let error = WorkflowBudget::load(&workspace).expect_err("zero is rejected");
        assert!(error.to_string().contains("greater than zero"));
        let _ = fs::remove_dir_all(workspace);
    }
}
