//! System-level test execution for the Plan workflow.
//!
//! `TestExecutor` is not a general shell. It only runs a command that came
//! from project configuration plus a selector that has already been validated
//! against that runner. Review baseline, `/continue` and `/test` all funnel
//! through this module so navigator and coach never need a writable role just
//! to run tests.

use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{process::Command, time::timeout};

use super::state::{TestRunRecord, TestStatus};

/// Default wall-clock limit for one test invocation.
pub const DEFAULT_TEST_TIMEOUT_SECONDS: u64 = 300;

/// How much output is kept in `state.json`.
pub const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

/// A test runner this module knows how to scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Runner {
    Cargo,
    Pytest,
}

impl Runner {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Pytest => "pytest",
        }
    }
}

/// Test configuration resolved from project files, never from the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTestConfig {
    pub runner: Runner,
    pub command: String,
}

impl ProjectTestConfig {
    /// Rebuild the runner from a command already stored in a test contract.
    ///
    /// Only the two commands this module detects are accepted, so a stored
    /// contract cannot smuggle a new program into the executor.
    #[must_use]
    pub fn from_command(command: &str) -> Option<Self> {
        let program = command.split_whitespace().next()?;
        let runner = match program {
            "cargo" => Runner::Cargo,
            "pytest" => Runner::Pytest,
            _ => return None,
        };
        Some(Self {
            runner,
            command: command.to_owned(),
        })
    }
}

/// One parsed test run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestRun {
    pub command: String,
    pub status: TestStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub failures: Vec<String>,
    pub passed: Vec<String>,
    pub output: String,
}

impl TestRun {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == TestStatus::Passed
    }

    /// Reduce the run to the evidence stored in `state.json`.
    #[must_use]
    pub fn record(&self, flaky_reruns: u32) -> TestRunRecord {
        TestRunRecord {
            command: self.command.clone(),
            status: self.status,
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            failures: self.failures.clone(),
            passed: self.passed.clone(),
            flaky_reruns,
            output_tail: tail(&self.output, OUTPUT_TAIL_BYTES),
        }
    }
}

fn tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// Errors from detection or execution. A red test run is not an error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TestExecError {
    #[error(
        "no test command found; add a Cargo package or pytest configuration before running tests"
    )]
    NoProjectConfig,
    #[error("invalid test selector `{selector}`: {reason}")]
    InvalidSelector { selector: String, reason: String },
    #[error("failed to run `{command}`: {message}")]
    Spawn { command: String, message: String },
    #[error("test run timed out after {0} seconds")]
    Timeout(u64),
}

/// Detect the project test command from workspace files.
///
/// Cargo wins when both Cargo and pytest configuration exist, because a Cargo
/// workspace is the narrower match for the repository layout.
#[must_use]
pub fn detect_project(workspace: &Path) -> Option<ProjectTestConfig> {
    if workspace.join("Cargo.toml").is_file() {
        return Some(ProjectTestConfig {
            runner: Runner::Cargo,
            command: "cargo test".to_owned(),
        });
    }
    if has_pytest_config(workspace) {
        return Some(ProjectTestConfig {
            runner: Runner::Pytest,
            command: "pytest".to_owned(),
        });
    }
    None
}

fn has_pytest_config(workspace: &Path) -> bool {
    if workspace.join("pytest.ini").is_file() {
        return true;
    }
    for (name, marker) in [
        ("pyproject.toml", "[tool.pytest"),
        ("setup.cfg", "[tool:pytest]"),
        ("tox.ini", "[pytest]"),
    ] {
        let path = workspace.join(name);
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains(marker)
        {
            return true;
        }
    }
    false
}

/// Reject selectors that could change the command or escape the workspace.
pub fn validate_selector(config: &ProjectTestConfig, selector: &str) -> Result<(), TestExecError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Ok(());
    }
    for token in selector.split_whitespace() {
        if token.chars().any(is_shell_control) {
            return Err(invalid_selector(
                selector,
                "shell control characters are not allowed",
            ));
        }
        if token.contains("..") {
            return Err(invalid_selector(selector, "parent paths are not allowed"));
        }
    }
    match config.runner {
        Runner::Cargo => validate_cargo_selector(selector),
        Runner::Pytest => {
            for token in selector.split_whitespace() {
                validate_pytest_token(selector, token)?;
            }
            Ok(())
        }
    }
}

/// Validate a selector and require every `-p`/`--package` name to be a member
/// of the workspace package set.
///
/// The set is empty when the caller cannot enumerate packages; in that case
/// only the syntax check runs and the caller decides whether that is enough.
pub fn validate_selector_against_packages(
    config: &ProjectTestConfig,
    selector: &str,
    packages: &std::collections::BTreeSet<String>,
) -> Result<(), TestExecError> {
    validate_selector(config, selector)?;
    if config.runner != Runner::Cargo || packages.is_empty() {
        return Ok(());
    }
    for package in cargo_package_values(selector) {
        if !packages.contains(package) {
            return Err(invalid_selector(
                selector,
                &format!("package `{package}` is not in this workspace"),
            ));
        }
    }
    Ok(())
}

fn validate_cargo_selector(selector: &str) -> Result<(), TestExecError> {
    const FLAGS: &[&str] = &[
        "--lib",
        "--bins",
        "--tests",
        "--examples",
        "--all-targets",
        "--doc",
        "--workspace",
        "--all",
    ];
    const VALUE_FLAGS: &[&str] = &["-p", "--package", "--test", "--bin", "--example"];
    let tokens: Vec<&str> = selector.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if FLAGS.contains(&token) {
            index += 1;
            continue;
        }
        if VALUE_FLAGS.contains(&token) {
            let value = tokens.get(index + 1).ok_or_else(|| {
                invalid_selector(selector, &format!("{token} needs a package or target name"))
            })?;
            if value.starts_with('-') || value.is_empty() {
                return Err(invalid_selector(
                    selector,
                    &format!("{token} needs a package or target name"),
                ));
            }
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            return Err(invalid_selector(
                selector,
                "only package or target scoping flags are allowed",
            ));
        }
        return Err(invalid_selector(
            selector,
            "bare test filters are not allowed; use -p <package> or --test <target>",
        ));
    }
    Ok(())
}

fn cargo_package_values(selector: &str) -> Vec<&str> {
    let tokens: Vec<&str> = selector.split_whitespace().collect();
    let mut packages = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if matches!(token, "-p" | "--package") {
            if let Some(value) = tokens.get(index + 1) {
                packages.push(*value);
            }
            index += 2;
            continue;
        }
        index += 1;
    }
    packages
}

fn validate_pytest_token(selector: &str, token: &str) -> Result<(), TestExecError> {
    if token.starts_with('-') {
        return Err(invalid_selector(
            selector,
            "pytest options are not allowed in a test selector",
        ));
    }
    if Path::new(token).is_absolute() {
        return Err(invalid_selector(selector, "absolute paths are not allowed"));
    }
    Ok(())
}

fn invalid_selector(selector: &str, reason: &str) -> TestExecError {
    TestExecError::InvalidSelector {
        selector: selector.to_owned(),
        reason: reason.to_owned(),
    }
}

fn is_shell_control(ch: char) -> bool {
    matches!(
        ch,
        ';' | '|' | '&' | '$' | '`' | '>' | '<' | '(' | ')' | '{' | '}' | '\n' | '\r' | '*' | '?'
    )
}

/// Run the configured command with the selector and parse the outcome.
pub async fn run(
    workspace: &Path,
    config: &ProjectTestConfig,
    selector: &str,
) -> Result<TestRun, TestExecError> {
    run_with_timeout(
        workspace,
        config,
        selector,
        Duration::from_secs(DEFAULT_TEST_TIMEOUT_SECONDS),
    )
    .await
}

/// Run with an explicit timeout. Tests use this to fail fast.
pub async fn run_with_timeout(
    workspace: &Path,
    config: &ProjectTestConfig,
    selector: &str,
    limit: Duration,
) -> Result<TestRun, TestExecError> {
    validate_selector(config, selector)?;
    let mut argv = config.command.split_whitespace();
    let program = argv.next().ok_or(TestExecError::NoProjectConfig)?;
    let mut command = Command::new(program);
    command
        .args(argv)
        .args(runner_flags(config.runner))
        .args(selector.split_whitespace())
        .current_dir(workspace)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let display = joined_command(&config.command, selector);
    let started = Instant::now();
    let child = command.spawn().map_err(|error| TestExecError::Spawn {
        command: display.clone(),
        message: error.to_string(),
    })?;
    let output = match timeout(limit, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(TestExecError::Spawn {
                command: display,
                message: error.to_string(),
            });
        }
        Err(_) => {
            return Err(TestExecError::Timeout(limit.as_secs()));
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}{stderr}");
    let (failures, passed) = match config.runner {
        Runner::Cargo => parse_cargo(&text),
        Runner::Pytest => parse_pytest(&text),
    };
    let status = if output.status.success() {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    Ok(TestRun {
        command: display,
        status,
        exit_code: output.status.code(),
        duration_ms,
        failures,
        passed,
        output: text,
    })
}

fn runner_flags(runner: Runner) -> &'static [&'static str] {
    match runner {
        Runner::Cargo => &[],
        // Pytest only prints per-test PASSED lines in verbose mode. The flag is
        // system-owned so the model still cannot change the command.
        Runner::Pytest => &["-v"],
    }
}

fn joined_command(command: &str, selector: &str) -> String {
    let selector = selector.trim();
    if selector.is_empty() {
        command.to_owned()
    } else {
        format!("{command} {selector}")
    }
}

/// Parse libtest output. Test names appear as `test <name> ... ok`.
fn parse_cargo(text: &str) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut passed = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, result)) = rest.split_once(" ... ") else {
            continue;
        };
        let name = name.trim();
        match result.trim() {
            "ok" => passed.push(name.to_owned()),
            "FAILED" => failures.push(name.to_owned()),
            _ => {}
        }
    }
    dedup(&mut failures);
    dedup(&mut passed);
    (failures, passed)
}

/// Parse pytest output. Verbose mode prints `<nodeid> PASSED` per test; the
/// short summary prints `FAILED <nodeid>` and `ERROR <nodeid>`.
fn parse_pytest(text: &str) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut passed = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("FAILED ") {
            failures.push(first_word(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("ERROR ") {
            failures.push(first_word(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("PASSED ") {
            passed.push(first_word(rest));
            continue;
        }
        if let Some(index) = line.find(" PASSED") {
            push_verbose(&mut passed, line, index);
        } else if let Some(index) = line.find(" FAILED") {
            push_verbose(&mut failures, line, index);
        } else if let Some(index) = line.find(" ERROR") {
            push_verbose(&mut failures, line, index);
        }
    }
    dedup(&mut failures);
    dedup(&mut passed);
    (failures, passed)
}

fn push_verbose(target: &mut Vec<String>, line: &str, index: usize) {
    let name = line[..index].trim();
    if !name.is_empty() && !name.contains(char::is_whitespace) {
        target.push(name.to_owned());
    }
}

fn first_word(value: &str) -> String {
    value
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn dedup(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo() -> ProjectTestConfig {
        ProjectTestConfig {
            runner: Runner::Cargo,
            command: "cargo test".to_owned(),
        }
    }

    fn pytest() -> ProjectTestConfig {
        ProjectTestConfig {
            runner: Runner::Pytest,
            command: "pytest".to_owned(),
        }
    }

    #[test]
    fn detects_cargo_before_pytest() {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-test-exec-detect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("Cargo.toml"), "[package]\nname = \"x\"\n")
            .expect("cargo file");
        std::fs::write(workspace.join("pytest.ini"), "[pytest]\n").expect("pytest file");
        assert_eq!(detect_project(&workspace), Some(cargo()));
        std::fs::remove_file(workspace.join("Cargo.toml")).expect("remove cargo");
        assert_eq!(detect_project(&workspace), Some(pytest()));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn missing_project_files_mean_no_config() {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-test-exec-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).expect("workspace");
        assert_eq!(detect_project(&workspace), None);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn cargo_selectors_accept_package_and_test_scoping() {
        for selector in [
            "",
            "-p parser",
            "--package parser",
            "--test parser",
            "-p parser --test parser",
            "--workspace",
            "--lib",
        ] {
            validate_selector(&cargo(), selector).expect("selector is legal");
        }
        for selector in [
            "; rm -rf .",
            "--release",
            "tests/../../etc",
            "$(whoami)",
            "parser",
            "parser::tests::parses",
            "-p",
            "-p ;",
            "-p parser parser",
        ] {
            assert!(
                validate_selector(&cargo(), selector).is_err(),
                "selector {selector} must be rejected"
            );
        }
    }

    #[test]
    fn cargo_selectors_must_name_a_workspace_package() {
        let packages = ["alpha".to_owned(), "beta".to_owned()]
            .into_iter()
            .collect();
        validate_selector_against_packages(&cargo(), "-p alpha", &packages)
            .expect("workspace member");
        validate_selector_against_packages(&cargo(), "-p alpha --test alpha", &packages)
            .expect("workspace member with a target");
        let error = validate_selector_against_packages(&cargo(), "-p l1-service", &packages)
            .expect_err("a private nested package is not a member");
        assert!(
            error.to_string().contains("is not in this workspace"),
            "{error}"
        );
    }

    #[test]
    fn pytest_selectors_accept_relative_node_ids() {
        validate_selector(&pytest(), "tests/test_parser.py::test_crlf").expect("legal selector");
        for selector in ["-k cache", "/etc/passwd", "../escape.py", "tests; rm -rf ."] {
            assert!(
                validate_selector(&pytest(), selector).is_err(),
                "selector {selector} must be rejected"
            );
        }
    }

    #[test]
    fn parses_libtest_results() {
        let text = "\
running 3 tests
test parser::tests::parses_lf ... ok
test parser::tests::parses_crlf ... FAILED
test parser::tests::ignored ... ignored
";
        let (failures, passed) = parse_cargo(text);
        assert_eq!(failures, ["parser::tests::parses_crlf"]);
        assert_eq!(passed, ["parser::tests::parses_lf"]);
    }

    #[test]
    fn parses_pytest_short_summary() {
        let text = "\
FAILED tests/test_parser.py::test_crlf - AssertionError
PASSED tests/test_parser.py::test_lf
";
        let (failures, passed) = parse_pytest(text);
        assert_eq!(failures, ["tests/test_parser.py::test_crlf"]);
        assert_eq!(passed, ["tests/test_parser.py::test_lf"]);
    }

    #[test]
    fn parses_pytest_verbose_lines() {
        let text = "\
tests/test_parser.py::test_lf PASSED                                  [ 50%]
tests/test_parser.py::test_crlf FAILED                                [100%]
FAILED tests/test_parser.py::test_crlf - AssertionError
";
        let (failures, passed) = parse_pytest(text);
        assert_eq!(failures, ["tests/test_parser.py::test_crlf"]);
        assert_eq!(passed, ["tests/test_parser.py::test_lf"]);
    }

    #[tokio::test]
    async fn runs_a_real_selector_and_reports_failures() {
        let workspace = temp_cargo_project();
        let config = detect_project(&workspace).expect("cargo config");
        let run =
            super::run_with_timeout(&workspace, &config, "", std::time::Duration::from_secs(120))
                .await
                .expect("test command runs");
        assert!(!run.succeeded(), "{}", run.output);
        assert!(
            run.failures.iter().any(|failure| failure == "tests::fails"),
            "{}",
            run.output
        );
        assert!(
            run.passed.iter().any(|test| test == "tests::passes"),
            "{}",
            run.output
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    fn temp_cargo_project() -> std::path::PathBuf {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-test-exec-project-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(workspace.join("src")).expect("src");
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::write(
            workspace.join("src/lib.rs"),
            "\
#[cfg(test)]
mod tests {
    #[test]
    fn passes() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn fails() {
        assert_eq!(2 + 2, 5);
    }
}
",
        )
        .expect("lib");
        workspace
    }

    #[test]
    fn runner_wire_names_are_stable() {
        assert_eq!(Runner::Cargo.as_str(), "cargo");
        assert_eq!(Runner::Pytest.as_str(), "pytest");
    }
}
