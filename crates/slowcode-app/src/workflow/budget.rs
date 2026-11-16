//! TestFix and workflow budgets.
//!
//! Limits have built-in defaults. A workspace can override individual fields
//! in `.slowcode/workflow.toml`. Every resolved field keeps its source so
//! `/status` can explain whether a number came from code or from the file.
//!
//! The file is strict: unknown keys and zero limits are rejected. A silent
//! fallback would hide a typo exactly where the budget is supposed to protect
//! the workflow.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

/// Default failed `/test` rounds before the workflow blocks.
pub const DEFAULT_MAX_ROUNDS: u32 = 10;
/// Default model calls in one `/fix` turn.
pub const DEFAULT_MAX_MODEL_CALLS_PER_ROUND: u64 = 8;
/// Default output tokens in one `/fix` turn.
pub const DEFAULT_MAX_OUTPUT_TOKENS_PER_ROUND: u64 = 60_000;
/// Default consecutive identical failure sets before the workflow blocks.
pub const DEFAULT_MAX_CONSECUTIVE_SAME_FAILURE: u32 = 3;
/// Default active seconds across the workflow.
pub const DEFAULT_MAX_ACTIVE_SECONDS: u64 = 3_600;

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
    pub max_rounds: BudgetValue,
    pub max_model_calls_per_round: BudgetValue,
    pub max_output_tokens_per_round: BudgetValue,
    pub max_consecutive_same_failure: BudgetValue,
    pub max_active_seconds: BudgetValue,
}

impl Default for WorkflowBudget {
    fn default() -> Self {
        Self {
            max_rounds: BudgetValue::default_value(u64::from(DEFAULT_MAX_ROUNDS)),
            max_model_calls_per_round: BudgetValue::default_value(
                DEFAULT_MAX_MODEL_CALLS_PER_ROUND,
            ),
            max_output_tokens_per_round: BudgetValue::default_value(
                DEFAULT_MAX_OUTPUT_TOKENS_PER_ROUND,
            ),
            max_consecutive_same_failure: BudgetValue::default_value(u64::from(
                DEFAULT_MAX_CONSECUTIVE_SAME_FAILURE,
            )),
            max_active_seconds: BudgetValue::default_value(DEFAULT_MAX_ACTIVE_SECONDS),
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
        Ok(Self {
            max_rounds: resolve(
                "test_fix.max_rounds",
                file.test_fix.max_rounds,
                defaults.max_rounds,
            )?,
            max_model_calls_per_round: resolve(
                "test_fix.max_model_calls_per_round",
                file.test_fix.max_model_calls_per_round,
                defaults.max_model_calls_per_round,
            )?,
            max_output_tokens_per_round: resolve(
                "test_fix.max_output_tokens_per_round",
                file.test_fix.max_output_tokens_per_round,
                defaults.max_output_tokens_per_round,
            )?,
            max_consecutive_same_failure: resolve(
                "test_fix.max_consecutive_same_failure",
                file.test_fix.max_consecutive_same_failure,
                defaults.max_consecutive_same_failure,
            )?,
            max_active_seconds: resolve(
                "workflow.max_active_seconds",
                file.workflow.max_active_seconds,
                defaults.max_active_seconds,
            )?,
        })
    }

    /// Render the effective values and their sources for `/status`.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "budget:\n  max_rounds: {} ({})\n  max_model_calls_per_round: {} ({})\n  max_output_tokens_per_round: {} ({})\n  max_consecutive_same_failure: {} ({})\n  max_active_seconds: {} ({})\n  max_cost_per_round: not enforced (no pricing source)",
            self.max_rounds.value,
            self.max_rounds.source.as_str(),
            self.max_model_calls_per_round.value,
            self.max_model_calls_per_round.source.as_str(),
            self.max_output_tokens_per_round.value,
            self.max_output_tokens_per_round.source.as_str(),
            self.max_consecutive_same_failure.value,
            self.max_consecutive_same_failure.source.as_str(),
            self.max_active_seconds.value,
            self.max_active_seconds.source.as_str(),
        )
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
    test_fix: TestFixFile,
    #[serde(default)]
    workflow: WorkflowFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestFixFile {
    max_rounds: Option<u64>,
    max_model_calls_per_round: Option<u64>,
    max_output_tokens_per_round: Option<u64>,
    max_consecutive_same_failure: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowFile {
    max_active_seconds: Option<u64>,
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
        assert_eq!(budget.max_rounds.value, u64::from(DEFAULT_MAX_ROUNDS));
        assert_eq!(budget.max_rounds.source, BudgetSource::Default);
        assert_eq!(budget.max_active_seconds.value, DEFAULT_MAX_ACTIVE_SECONDS);
    }

    #[test]
    fn file_overrides_only_present_fields() {
        let workspace = temp_workspace();
        write_config(
            &workspace,
            "[test_fix]\nmax_rounds = 4\n[workflow]\nmax_active_seconds = 120\n",
        );
        let budget = WorkflowBudget::load(&workspace).expect("config loads");
        assert_eq!(budget.max_rounds.value, 4);
        assert_eq!(budget.max_rounds.source, BudgetSource::File);
        assert_eq!(
            budget.max_model_calls_per_round.source,
            BudgetSource::Default
        );
        assert_eq!(budget.max_active_seconds.value, 120);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let workspace = temp_workspace();
        write_config(&workspace, "[test_fix]\nmax_round = 4\n");
        let error = WorkflowBudget::load(&workspace).expect_err("unknown key is rejected");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn zero_limits_are_rejected() {
        let workspace = temp_workspace();
        write_config(&workspace, "[test_fix]\nmax_rounds = 0\n");
        let error = WorkflowBudget::load(&workspace).expect_err("zero is rejected");
        assert!(error.to_string().contains("greater than zero"));
    }
}
