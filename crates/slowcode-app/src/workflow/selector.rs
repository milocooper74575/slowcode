//! Derive the test selector from the tasks that reference workspace files.
//!
//! The selector is system-derived, never model-authored. Cargo workflows scope
//! by package with `-p`; pytest workflows scope by a shared directory. When the
//! derivation is not unique the caller asks the user, because guessing a test
//! scope would silently narrow or widen the contract.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use super::{
    state::{ExecState, Strategy, Task},
    test_exec::{ProjectTestConfig, Runner, validate_selector_against_packages},
};

/// User-facing option that skips the test contract for a docs-only plan.
pub const NO_TEST_OPTION: &str = "无需测试（仅文档改动）";

/// Extensions that cannot change executable behavior.
const DOC_EXTENSIONS: &[&str] = &["md", "mdx", "txt", "rst", "adoc"];

/// Result of selector derivation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectorChoice {
    /// A selector validated against the project command.
    Resolved(String),
    /// The system could not pick one; the caller asks the user.
    NeedsUser {
        options: Vec<String>,
        reason: String,
    },
}

/// Derive a selector, honouring an explicit override first.
pub fn derive_selector(
    workspace: &Path,
    tasks: &[Task],
    project: &ProjectTestConfig,
    override_selector: Option<&str>,
) -> SelectorChoice {
    let packages = workspace_packages(workspace);
    let can_skip = plan_can_skip_test(tasks);
    let options = selector_options(&packages, can_skip);
    if let Some(selector) = override_selector {
        return resolve(project, selector, &packages, &options, can_skip);
    }
    match project.runner {
        Runner::Cargo => derive_cargo(workspace, tasks, project, &packages, can_skip),
        Runner::Pytest => derive_pytest(workspace, tasks, project, can_skip),
    }
}

fn derive_cargo(
    workspace: &Path,
    tasks: &[Task],
    project: &ProjectTestConfig,
    packages: &BTreeSet<String>,
    can_skip: bool,
) -> SelectorChoice {
    let options = selector_options(packages, can_skip);
    let task_packages: BTreeSet<String> = task_paths(tasks)
        .iter()
        .filter_map(|path| nearest_package_name(workspace, path))
        .filter(|name| packages.contains(name))
        .collect();
    if !task_packages.is_empty() {
        let selector = task_packages
            .iter()
            .map(|name| format!("-p {name}"))
            .collect::<Vec<_>>()
            .join(" ");
        return resolve(project, &selector, packages, &options, can_skip);
    }
    if options.len() == 1 {
        return resolve(project, &options[0], packages, &options, can_skip);
    }
    SelectorChoice::NeedsUser {
        reason: "tasks do not reference exactly one Cargo package".to_owned(),
        options,
    }
}

fn derive_pytest(
    workspace: &Path,
    tasks: &[Task],
    project: &ProjectTestConfig,
    can_skip: bool,
) -> SelectorChoice {
    let mut directories: Vec<PathBuf> = task_paths(tasks)
        .iter()
        .filter_map(|path| nearest_existing_dir(workspace, path))
        .filter_map(|path| path.strip_prefix(workspace).ok().map(Path::to_path_buf))
        .collect();
    directories.sort();
    directories.dedup();
    if directories.is_empty() {
        let mut options = vec![".".to_owned()];
        if can_skip {
            options.push(NO_TEST_OPTION.to_owned());
        }
        return SelectorChoice::NeedsUser {
            reason: "tasks do not reference a Python test directory".to_owned(),
            options,
        };
    }
    let common = common_prefix(&directories);
    let selector = if common.as_os_str().is_empty() {
        String::new()
    } else {
        common.to_string_lossy().replace('\\', "/")
    };
    let mut options = vec![selector.clone()];
    if can_skip {
        options.push(NO_TEST_OPTION.to_owned());
    }
    resolve(project, &selector, &BTreeSet::new(), &options, can_skip)
}

fn resolve(
    project: &ProjectTestConfig,
    selector: &str,
    packages: &BTreeSet<String>,
    options: &[String],
    can_skip: bool,
) -> SelectorChoice {
    if selector.trim() == NO_TEST_OPTION {
        return if can_skip {
            SelectorChoice::Resolved(NO_TEST_OPTION.to_owned())
        } else {
            SelectorChoice::NeedsUser {
                reason: "test skip is only available for docs-only plans".to_owned(),
                options: options.to_vec(),
            }
        };
    }
    match validate_selector_against_packages(project, selector, packages) {
        Ok(()) => SelectorChoice::Resolved(selector.trim().to_owned()),
        Err(error) => SelectorChoice::NeedsUser {
            reason: error.to_string(),
            options: options.to_vec(),
        },
    }
}

fn selector_options(packages: &BTreeSet<String>, can_skip: bool) -> Vec<String> {
    let mut options = packages
        .iter()
        .map(|name| format!("-p {name}"))
        .collect::<Vec<_>>();
    if can_skip {
        options.push(NO_TEST_OPTION.to_owned());
    }
    options
}

/// True when every file the task table points at is documentation.
///
/// This is deliberately conservative: a task with no file reference, a
/// directory reference, or any code extension keeps the test gate.
#[must_use]
pub fn plan_is_docs_only(tasks: &[Task]) -> bool {
    let tokens = file_tokens(tasks);
    !tokens.is_empty() && tokens.iter().all(|token| is_doc_file(token))
}

/// True when the plan may record "no tests" instead of a Cargo selector.
#[must_use]
pub fn plan_can_skip_test(tasks: &[Task]) -> bool {
    plan_is_docs_only(tasks)
        && !tasks.iter().any(|task| {
            task.exec_state == ExecState::PendingUser
                || task.strategy == Strategy::UserImplementsCore
        })
}

fn file_tokens(tasks: &[Task]) -> Vec<String> {
    let mut tokens = Vec::new();
    for task in tasks {
        for field in [&task.min_path, &task.acceptance_ref] {
            for raw in field.split(|character: char| {
                character.is_whitespace()
                    || matches!(character, ',' | ';' | '，' | '、' | '|' | '→' | '；')
            }) {
                let token = trim_token(raw);
                if looks_like_file(token) {
                    tokens.push(token.to_owned());
                }
            }
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

fn trim_token(raw: &str) -> &str {
    raw.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\''
                | '"'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '。'
                | '，'
                | '；'
                | ':'
                | '：'
        )
    })
}

fn looks_like_file(token: &str) -> bool {
    !token.is_empty() && !token.contains("://") && (token.contains('/') || token.contains('.'))
}

fn is_doc_file(token: &str) -> bool {
    let path = strip_location_suffix(token).to_ascii_lowercase();
    DOC_EXTENSIONS
        .iter()
        .any(|extension| path.ends_with(&format!(".{extension}")))
}

fn strip_location_suffix(token: &str) -> &str {
    if let Some((path, suffix)) = token.rsplit_once(':') {
        let suffix = suffix.trim();
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return path;
        }
    }
    token
}

/// Path-like tokens from the task's minimal path and acceptance reference.
fn task_paths(tasks: &[Task]) -> Vec<String> {
    let mut paths = Vec::new();
    for task in tasks {
        for field in [&task.min_path, &task.acceptance_ref] {
            for raw in field.split(|character: char| {
                character.is_whitespace() || matches!(character, ',' | ';' | '，' | '、' | '|')
            }) {
                let token = raw.trim_matches(|character: char| {
                    matches!(
                        character,
                        '`' | '\''
                            | '"'
                            | '('
                            | ')'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '<'
                            | '>'
                            | ':'
                            | '。'
                    )
                });
                if looks_like_path(token) {
                    paths.push(token.to_owned());
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn looks_like_path(token: &str) -> bool {
    !token.is_empty()
        && !token.contains("://")
        && (token.contains('/')
            || token.ends_with(".rs")
            || token.ends_with(".py")
            || token.ends_with(".toml"))
}

fn nearest_package_name(workspace: &Path, relative: &str) -> Option<String> {
    let mut current = workspace.join(relative);
    if current.is_file() {
        current.pop();
    }
    loop {
        if current.starts_with(workspace) {
            let cargo = current.join("Cargo.toml");
            if cargo.is_file()
                && let Some(name) = package_name(&cargo)
            {
                return Some(name);
            }
        }
        if current == workspace || !current.pop() {
            return None;
        }
    }
}

fn package_name(cargo: &Path) -> Option<String> {
    let text = fs::read_to_string(cargo).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    value
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

fn nearest_existing_dir(workspace: &Path, relative: &str) -> Option<PathBuf> {
    let mut current = workspace.join(relative);
    if current.is_file() {
        current.pop();
    }
    loop {
        if current.is_dir() && current.starts_with(workspace) {
            return Some(current);
        }
        if current == workspace || !current.pop() {
            return None;
        }
    }
}

fn common_prefix(directories: &[PathBuf]) -> PathBuf {
    let mut common = PathBuf::new();
    for component in directories[0].components() {
        let index = common.components().count();
        if directories[1..]
            .iter()
            .any(|directory| directory.components().nth(index) != Some(component))
        {
            break;
        }
        common.push(component);
    }
    common
}

/// Package names that belong to the root Cargo workspace.
///
/// `None` means the root manifest is missing or unreadable, so the caller
/// should fall back to the bounded directory scan. A nested Cargo workspace
/// that the root manifest excludes is never returned.
#[must_use]
pub fn workspace_package_names(workspace: &Path) -> Option<BTreeSet<String>> {
    let text = fs::read_to_string(workspace.join("Cargo.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let mut names = BTreeSet::new();
    if let Some(workspace_table) = value.get("workspace") {
        let excludes = string_array(workspace_table.get("exclude"));
        let members = string_array(workspace_table.get("members"));
        if !members.is_empty() {
            for member in members {
                for path in expand_member_pattern(workspace, &member) {
                    let relative = relative_path(workspace, &path);
                    if excludes
                        .iter()
                        .any(|pattern| glob_match(pattern, &relative))
                    {
                        continue;
                    }
                    if let Some(name) = package_name(&path.join("Cargo.toml")) {
                        names.insert(name);
                    }
                }
            }
            return Some(names);
        }
    }
    if let Some(name) = package_name(&workspace.join("Cargo.toml")) {
        names.insert(name);
        return Some(names);
    }
    if names.is_empty() { None } else { Some(names) }
}

fn workspace_packages(workspace: &Path) -> BTreeSet<String> {
    workspace_package_names(workspace).unwrap_or_else(|| {
        let mut packages = BTreeSet::new();
        collect_packages(workspace, workspace, 0, &mut packages);
        packages
    })
}

/// Directories excluded by the root Cargo manifest's `workspace.exclude`.
pub(crate) fn workspace_excluded_dirs(workspace: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(workspace.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return Vec::new();
    };
    let Some(table) = value.get("workspace") else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for pattern in string_array(table.get("exclude")) {
        for path in expand_member_pattern(workspace, &pattern) {
            dirs.push(path);
        }
        let plain = workspace.join(&pattern);
        if plain.is_dir() {
            dirs.push(plain);
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn string_array(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn expand_member_pattern(workspace: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern = pattern.replace('\\', "/");
    if !pattern.contains('*') {
        let path = workspace.join(&pattern);
        return if path.join("Cargo.toml").is_file() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    let mut current = vec![workspace.to_path_buf()];
    for segment in pattern.split('/').filter(|segment| !segment.is_empty()) {
        let mut next = Vec::new();
        for base in &current {
            if segment == "*" || segment == "**" {
                let Ok(entries) = fs::read_dir(base) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && !excluded(&path) {
                        next.push(path);
                    }
                }
            } else {
                let path = base.join(segment);
                if path.is_dir() {
                    next.push(path);
                }
            }
        }
        current = next;
    }
    current
        .into_iter()
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect()
}

fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let text = text.replace('\\', "/");
    wildcard_match(pattern.as_bytes(), text.as_bytes())
}

fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => {
            wildcard_match(rest, text) || (!text.is_empty() && wildcard_match(pattern, &text[1..]))
        }
        Some((b'?', rest)) => !text.is_empty() && wildcard_match(rest, &text[1..]),
        Some((byte, rest)) => text.first() == Some(byte) && wildcard_match(rest, &text[1..]),
    }
}

fn collect_packages(root: &Path, directory: &Path, depth: usize, packages: &mut BTreeSet<String>) {
    if depth > 4 {
        return;
    }
    let cargo = directory.join("Cargo.toml");
    if cargo.is_file()
        && let Some(name) = package_name(&cargo)
    {
        packages.insert(name);
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if excluded(&path) {
            continue;
        }
        if path.starts_with(root) {
            collect_packages(root, &path, depth + 1, packages);
        }
    }
}

fn excluded(path: &Path) -> bool {
    super::is_scan_excluded(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::state::{Domain, ExecState, Strategy};

    fn task(id: &str, path: &str) -> Task {
        Task::new(
            id.to_owned(),
            format!("title {id}"),
            Domain::Known,
            "evidence".to_owned(),
            ExecState::Executable,
            "action".to_owned(),
            "role".to_owned(),
            path.to_owned(),
            path.to_owned(),
            Strategy::AiImplements,
        )
    }

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("slowcode-selector-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("workspace is created");
        path
    }

    fn write_package(workspace: &Path, relative: &str, name: &str) {
        let directory = workspace.join(relative);
        fs::create_dir_all(directory.join("src")).expect("package is created");
        fs::write(
            directory.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .expect("cargo toml is written");
        fs::write(directory.join("src/lib.rs"), "pub fn x() {}\n").expect("source is written");
    }

    fn write_workspace(workspace: &Path, members: &str, excludes: &str) {
        fs::write(
            workspace.join("Cargo.toml"),
            format!("[workspace]\nresolver = \"3\"\nmembers = {members}\nexclude = {excludes}\n"),
        )
        .expect("workspace manifest");
    }

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
    fn cargo_uses_the_nearest_package() {
        let workspace = temp_workspace("cargo-one");
        write_package(&workspace, "crates/alpha", "alpha");
        let choice = derive_selector(
            &workspace,
            &[task("task-1", "crates/alpha/src/lib.rs")],
            &cargo(),
            None,
        );
        assert_eq!(choice, SelectorChoice::Resolved("-p alpha".to_owned()));
    }

    #[test]
    fn cargo_combines_multiple_packages() {
        let workspace = temp_workspace("cargo-two");
        write_package(&workspace, "crates/alpha", "alpha");
        write_package(&workspace, "crates/beta", "beta");
        let choice = derive_selector(
            &workspace,
            &[
                task("task-1", "crates/alpha/src/lib.rs"),
                task("task-2", "`crates/beta/src/lib.rs`"),
            ],
            &cargo(),
            None,
        );
        assert_eq!(
            choice,
            SelectorChoice::Resolved("-p alpha -p beta".to_owned())
        );
    }

    #[test]
    fn cargo_without_paths_offers_packages() {
        let workspace = temp_workspace("cargo-options");
        write_package(&workspace, "crates/alpha", "alpha");
        write_package(&workspace, "crates/beta", "beta");
        let choice = derive_selector(
            &workspace,
            &[task("task-1", "不做具体文件")],
            &cargo(),
            None,
        );
        match choice {
            SelectorChoice::NeedsUser { options, .. } => {
                assert_eq!(options, vec!["-p alpha".to_owned(), "-p beta".to_owned()]);
            }
            other => panic!("unexpected choice: {other:?}"),
        }
    }

    #[test]
    fn pytest_uses_the_common_directory() {
        let workspace = temp_workspace("pytest-common");
        fs::create_dir_all(workspace.join("tests/unit")).expect("directory is created");
        let choice = derive_selector(
            &workspace,
            &[
                task("task-1", "tests/unit/test_a.py"),
                task("task-2", "tests/unit/test_b.py"),
            ],
            &pytest(),
            None,
        );
        assert_eq!(choice, SelectorChoice::Resolved("tests/unit".to_owned()));
    }

    #[test]
    fn override_wins_and_is_validated() {
        let workspace = temp_workspace("override");
        write_package(&workspace, "crates/alpha", "alpha");
        let choice = derive_selector(&workspace, &[], &cargo(), Some("-p alpha"));
        assert_eq!(choice, SelectorChoice::Resolved("-p alpha".to_owned()));
    }

    #[test]
    fn excluded_nested_workspace_is_not_a_package() {
        let workspace = temp_workspace("nested-excluded");
        write_workspace(&workspace, "[\"crates/*\"]", "[\"excerise\"]");
        write_package(&workspace, "crates/alpha", "alpha");
        write_package(&workspace, "crates/beta", "beta");
        fs::create_dir_all(workspace.join("excerise")).expect("exercise directory");
        fs::write(
            workspace.join("excerise/Cargo.toml"),
            "[workspace]\nmembers = [\"l1-service\"]\n",
        )
        .expect("exercise workspace");
        write_package(&workspace, "excerise/l1-service", "l1-service");

        let packages = workspace_package_names(&workspace).expect("workspace packages");
        assert_eq!(
            packages,
            ["alpha".to_owned(), "beta".to_owned()]
                .into_iter()
                .collect()
        );
        match derive_selector(&workspace, &[], &cargo(), None) {
            SelectorChoice::NeedsUser { options, .. } => {
                assert_eq!(options, vec!["-p alpha".to_owned(), "-p beta".to_owned()]);
            }
            other => panic!("unexpected choice: {other:?}"),
        }
    }

    #[test]
    fn stored_selector_outside_the_workspace_reasks() {
        let workspace = temp_workspace("stored-outside");
        write_workspace(&workspace, "[\"crates/*\"]", "[\"excerise\"]");
        write_package(&workspace, "crates/alpha", "alpha");
        write_package(&workspace, "excerise/l1-service", "l1-service");

        let choice = derive_selector(&workspace, &[], &cargo(), Some("-p l1-service"));
        match choice {
            SelectorChoice::NeedsUser { options, reason } => {
                assert!(reason.contains("not in this workspace"), "{reason}");
                assert_eq!(options, vec!["-p alpha".to_owned()]);
            }
            other => panic!("unexpected choice: {other:?}"),
        }
    }

    #[test]
    fn docs_only_plan_offers_no_test() {
        let workspace = temp_workspace("docs-only");
        write_package(&workspace, ".", "fixture");
        let docs = task("task-1", "AGENTS.md");

        match derive_selector(&workspace, std::slice::from_ref(&docs), &cargo(), None) {
            SelectorChoice::NeedsUser { options, .. } => {
                assert_eq!(
                    options,
                    vec!["-p fixture".to_owned(), NO_TEST_OPTION.to_owned()]
                );
            }
            other => panic!("unexpected choice: {other:?}"),
        }
        assert_eq!(
            derive_selector(&workspace, &[docs], &cargo(), Some(NO_TEST_OPTION)),
            SelectorChoice::Resolved(NO_TEST_OPTION.to_owned())
        );
    }

    #[test]
    fn code_plan_does_not_offer_no_test() {
        let workspace = temp_workspace("code-plan");
        write_package(&workspace, ".", "fixture");
        let code = task("task-1", "src/lib.rs");

        assert!(!plan_can_skip_test(std::slice::from_ref(&code)));
        assert_eq!(
            derive_selector(&workspace, std::slice::from_ref(&code), &cargo(), None),
            SelectorChoice::Resolved("-p fixture".to_owned())
        );
        let options = selector_options(&workspace_packages(&workspace), false);
        assert_eq!(options, vec!["-p fixture".to_owned()]);
    }

    #[test]
    fn no_test_is_rejected_for_code_plans() {
        let workspace = temp_workspace("no-test-code");
        write_package(&workspace, ".", "fixture");
        let code = task("task-1", "src/lib.rs");
        match derive_selector(&workspace, &[code], &cargo(), Some(NO_TEST_OPTION)) {
            SelectorChoice::NeedsUser { reason, .. } => {
                assert!(reason.contains("docs-only"), "{reason}");
            }
            other => panic!("unexpected choice: {other:?}"),
        }
    }
}
