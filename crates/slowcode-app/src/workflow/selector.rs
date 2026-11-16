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
    state::Task,
    test_exec::{ProjectTestConfig, Runner, validate_selector},
};

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
    if let Some(selector) = override_selector {
        return resolve(project, selector);
    }
    match project.runner {
        Runner::Cargo => derive_cargo(workspace, tasks, project),
        Runner::Pytest => derive_pytest(workspace, tasks, project),
    }
}

fn derive_cargo(workspace: &Path, tasks: &[Task], project: &ProjectTestConfig) -> SelectorChoice {
    let packages: BTreeSet<String> = task_paths(tasks)
        .iter()
        .filter_map(|path| nearest_package_name(workspace, path))
        .collect();
    if !packages.is_empty() {
        let selector = packages
            .iter()
            .map(|name| format!("-p {name}"))
            .collect::<Vec<_>>()
            .join(" ");
        return resolve(project, &selector);
    }
    let options = cargo_package_options(workspace);
    if options.len() == 1 {
        return resolve(project, &options[0]);
    }
    SelectorChoice::NeedsUser {
        reason: "tasks do not reference exactly one Cargo package".to_owned(),
        options,
    }
}

fn derive_pytest(workspace: &Path, tasks: &[Task], project: &ProjectTestConfig) -> SelectorChoice {
    let mut directories: Vec<PathBuf> = task_paths(tasks)
        .iter()
        .filter_map(|path| nearest_existing_dir(workspace, path))
        .filter_map(|path| path.strip_prefix(workspace).ok().map(Path::to_path_buf))
        .collect();
    directories.sort();
    directories.dedup();
    if directories.is_empty() {
        return SelectorChoice::NeedsUser {
            reason: "tasks do not reference a Python test directory".to_owned(),
            options: vec![".".to_owned()],
        };
    }
    let common = common_prefix(&directories);
    let selector = if common.as_os_str().is_empty() {
        String::new()
    } else {
        common.to_string_lossy().replace('\\', "/")
    };
    resolve(project, &selector)
}

fn resolve(project: &ProjectTestConfig, selector: &str) -> SelectorChoice {
    match validate_selector(project, selector) {
        Ok(()) => SelectorChoice::Resolved(selector.trim().to_owned()),
        Err(error) => SelectorChoice::NeedsUser {
            reason: error.to_string(),
            options: Vec::new(),
        },
    }
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

fn cargo_package_options(workspace: &Path) -> Vec<String> {
    let mut packages = BTreeSet::new();
    collect_packages(workspace, workspace, 0, &mut packages);
    packages
        .into_iter()
        .map(|name| format!("-p {name}"))
        .collect()
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
}
