//! Local skill installation for slowcode.
//!
//! P0 installs from a local directory only. Installation previews every file,
//! rejects path escape and executables, copies into a scanned skill root and
//! updates an atomic lock file.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slowcode_kernel::skills::{SkillMetadata, read_skill_metadata};
use slowcode_roles::RoleName;
use thiserror::Error;

const MAX_FILES: usize = 200;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_NAME_CHARS: usize = 64;

/// Where a skill is installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SkillScope {
    Project,
    Global,
    Role(RoleName),
}

impl SkillScope {
    fn key(&self) -> String {
        match self {
            Self::Project => "project".to_owned(),
            Self::Global => "global".to_owned(),
            Self::Role(role) => format!("role:{}", role.as_str()),
        }
    }

    fn role(&self) -> Option<&str> {
        match self {
            Self::Role(role) => Some(role.as_str()),
            _ => None,
        }
    }
}

/// Installation options.
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    pub name: Option<String>,
    pub force: bool,
    pub allow_executables: bool,
    pub dry_run: bool,
}

/// What an install would do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallPreview {
    pub name: String,
    /// Frontmatter name when `--name` renames the installed skill.
    pub renamed_from: Option<String>,
    pub description: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub overwrites: bool,
    pub role: Option<String>,
}

/// Result of an install.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallOutcome {
    pub preview: InstallPreview,
    pub installed: bool,
}

/// One installed skill discovered on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: String,
    pub role: Option<String>,
}

/// Installer bound to one workspace.
#[derive(Clone, Debug)]
pub struct SkillInstaller {
    workspace: PathBuf,
    home: Option<PathBuf>,
}

impl SkillInstaller {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Validate a local source and return the exact install preview.
    pub fn preview(
        &self,
        source: impl AsRef<Path>,
        scope: &SkillScope,
        options: &InstallOptions,
    ) -> Result<InstallPreview, SkillError> {
        let source = source.as_ref();
        let skill_root = find_skill_root(source)?;
        let metadata = read_source_metadata(&skill_root)?;
        let (name, renamed_from) = install_name(&metadata, options)?;
        let (files, bytes) = collect_files(&skill_root, options.allow_executables)?;
        let bytes = match &renamed_from {
            Some(_) => renamed_bytes(&skill_root, &name, bytes)?,
            None => bytes,
        };
        let target = self.root(scope)?.join(&name);
        Ok(InstallPreview {
            name,
            renamed_from,
            description: metadata.description,
            source: skill_root,
            target: target.clone(),
            files: files.len(),
            bytes,
            overwrites: target.exists(),
            role: scope.role().map(str::to_owned),
        })
    }

    /// Install one local skill directory.
    pub fn install(
        &self,
        source: impl AsRef<Path>,
        scope: &SkillScope,
        options: &InstallOptions,
    ) -> Result<InstallOutcome, SkillError> {
        let preview = self.preview(source, scope, options)?;
        if options.dry_run {
            return Ok(InstallOutcome {
                preview,
                installed: false,
            });
        }
        if preview.overwrites && !options.force {
            return Err(SkillError::Exists(preview.target.display().to_string()));
        }
        self.apply(&preview, scope, options.allow_executables)?;
        Ok(InstallOutcome {
            preview,
            installed: true,
        })
    }

    fn apply(
        &self,
        preview: &InstallPreview,
        scope: &SkillScope,
        allow_executables: bool,
    ) -> Result<(), SkillError> {
        let root = self.root(scope)?;
        fs::create_dir_all(&root)?;
        let staged = stage_skill(&root, &preview.source, allow_executables)?;
        if preview.renamed_from.is_some()
            && let Err(error) = rename_staged_skill(&staged, &preview.name)
        {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
        if let Err(error) = replace_skill_dir(&staged, &root, &preview.name) {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
        self.record_install(preview, scope, &preview.target)
    }

    fn record_install(
        &self,
        preview: &InstallPreview,
        scope: &SkillScope,
        target: &Path,
    ) -> Result<(), SkillError> {
        let record = SkillRecord {
            name: preview.name.clone(),
            source: preview.source.display().to_string(),
            sha256: hash_tree(target)?,
            scope: scope.key(),
            role: scope.role().map(str::to_owned),
            installed_at: now_unix(),
        };
        self.update_lock(scope, record)
    }

    /// Remove one installed skill and its lock record.
    pub fn remove(&self, name: &str, scope: &SkillScope) -> Result<(), SkillError> {
        validate_name(name)?;
        let root = self.root(scope)?;
        let target = root.join(name);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        self.remove_lock(scope, name)?;
        Ok(())
    }

    /// List skills in project, role and global roots.
    pub fn list(&self) -> Result<Vec<InstalledSkill>, SkillError> {
        let mut skills = Vec::new();
        let scopes = [
            (SkillScope::Project, self.root(&SkillScope::Project).ok()),
            (
                SkillScope::Role(RoleName::Navigator),
                self.root(&SkillScope::Role(RoleName::Navigator)).ok(),
            ),
            (
                SkillScope::Role(RoleName::Driver),
                self.root(&SkillScope::Role(RoleName::Driver)).ok(),
            ),
            (SkillScope::Global, self.root(&SkillScope::Global).ok()),
        ];
        for (scope, root) in scopes {
            let Some(root) = root else {
                continue;
            };
            let Ok(entries) = fs::read_dir(root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let metadata_path = path.join("SKILL.md");
                let Ok(Some(metadata)) = read_skill_metadata(&metadata_path) else {
                    continue;
                };
                skills.push(InstalledSkill {
                    name: metadata.name,
                    description: metadata.description,
                    path,
                    scope: scope.key(),
                    role: scope.role().map(str::to_owned),
                });
            }
        }
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(skills)
    }

    fn root(&self, scope: &SkillScope) -> Result<PathBuf, SkillError> {
        match scope {
            SkillScope::Project => Ok(self.workspace.join(".slowcode/skills")),
            SkillScope::Role(role) => Ok(self
                .workspace
                .join(".slowcode/roles")
                .join(role.as_str())
                .join("skills")),
            SkillScope::Global => self
                .home
                .as_ref()
                .map(|home| home.join(".slowcode/skills"))
                .ok_or_else(|| SkillError::Validation("HOME is not available".to_owned())),
        }
    }

    fn lock_path(&self, scope: &SkillScope) -> Result<PathBuf, SkillError> {
        match scope {
            SkillScope::Global => self
                .home
                .as_ref()
                .map(|home| home.join(".slowcode/skills-lock.json"))
                .ok_or_else(|| SkillError::Validation("HOME is not available".to_owned())),
            _ => Ok(self.workspace.join(".slowcode/skills-lock.json")),
        }
    }

    fn update_lock(&self, scope: &SkillScope, record: SkillRecord) -> Result<(), SkillError> {
        let path = self.lock_path(scope)?;
        let mut lock = read_lock(&path)?;
        lock.skills
            .insert(format!("{}:{}", scope.key(), record.name), record);
        write_lock(&path, &lock)
    }

    fn remove_lock(&self, scope: &SkillScope, name: &str) -> Result<(), SkillError> {
        let path = self.lock_path(scope)?;
        if !path.exists() {
            return Ok(());
        }
        let mut lock = read_lock(&path)?;
        lock.skills.remove(&format!("{}:{}", scope.key(), name));
        write_lock(&path, &lock)
    }
}

/// Lock file schema.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SkillLock {
    version: u32,
    skills: BTreeMap<String, SkillRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SkillRecord {
    name: String,
    source: String,
    sha256: String,
    scope: String,
    role: Option<String>,
    installed_at: u64,
}

/// Skill installation errors.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("source: {0}")]
    Source(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error("a skill already exists at {0}")]
    Exists(String),
    #[error("lock file is corrupt: {0}")]
    Lock(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

fn find_skill_root(source: &Path) -> Result<PathBuf, SkillError> {
    if source.join("SKILL.md").is_file() {
        return Ok(source.to_path_buf());
    }
    let mut entries = fs::read_dir(source)
        .map_err(|error| SkillError::Source(error.to_string()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    if entries.len() == 1 && entries[0].join("SKILL.md").is_file() {
        return Ok(entries.remove(0));
    }
    Err(SkillError::Validation(format!(
        "{} does not contain SKILL.md",
        source.display()
    )))
}

fn validate_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty()
        || name.len() > MAX_NAME_CHARS
        || name.starts_with('-')
        || name.ends_with('-')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(SkillError::Validation(format!(
            "invalid skill name: {name}; expected lowercase letters, digits and hyphens"
        )));
    }
    Ok(())
}

fn collect_files(root: &Path, allow_executables: bool) -> Result<(Vec<PathBuf>, u64), SkillError> {
    let mut files = Vec::new();
    let mut bytes = 0_u64;
    walk(root, root, allow_executables, &mut files, &mut bytes)?;
    if files.len() > MAX_FILES {
        return Err(SkillError::Validation(format!(
            "skill has {} files; limit is {MAX_FILES}",
            files.len()
        )));
    }
    Ok((files, bytes))
}

fn walk(
    root: &Path,
    directory: &Path,
    allow_executables: bool,
    files: &mut Vec<PathBuf>,
    bytes: &mut u64,
) -> Result<(), SkillError> {
    for entry in fs::read_dir(directory)?.flatten() {
        walk_entry(root, entry.path(), allow_executables, files, bytes)?;
    }
    Ok(())
}

fn walk_entry(
    root: &Path,
    path: PathBuf,
    allow_executables: bool,
    files: &mut Vec<PathBuf>,
    bytes: &mut u64,
) -> Result<(), SkillError> {
    let (metadata, kind) = classify_entry(&path)?;
    match kind {
        EntryKind::Directory => walk(root, &path, allow_executables, files, bytes),
        EntryKind::File => {
            validate_file(&path, &metadata, allow_executables)?;
            *bytes = bytes.saturating_add(metadata.len());
            if *bytes > MAX_TOTAL_BYTES {
                return Err(SkillError::Validation(format!(
                    "skill exceeds {MAX_TOTAL_BYTES} bytes"
                )));
            }
            files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            Ok(())
        }
    }
}

enum EntryKind {
    Directory,
    File,
}

fn classify_entry(path: &Path) -> Result<(fs::Metadata, EntryKind), SkillError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(reject_symlink(path));
    }
    if metadata.is_dir() {
        return Ok((metadata, EntryKind::Directory));
    }
    if metadata.is_file() {
        return Ok((metadata, EntryKind::File));
    }
    Err(SkillError::Validation(format!(
        "unsupported file type: {}",
        path.display()
    )))
}

fn validate_file(
    path: &Path,
    metadata: &fs::Metadata,
    allow_executables: bool,
) -> Result<(), SkillError> {
    ensure_not_executable(path, metadata, allow_executables)?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(SkillError::Validation(format!(
            "file is too large: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_not_executable(
    path: &Path,
    metadata: &fs::Metadata,
    allow_executables: bool,
) -> Result<(), SkillError> {
    if allow_executables || !is_executable(metadata) {
        return Ok(());
    }
    Err(SkillError::Validation(format!(
        "executable files are not allowed: {}",
        path.display()
    )))
}

fn reject_symlink(path: &Path) -> SkillError {
    SkillError::Validation(format!(
        "symlinks are not allowed in skills: {}",
        path.display()
    ))
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn copy_tree(source: &Path, target: &Path, allow_executables: bool) -> Result<(), SkillError> {
    for entry in fs::read_dir(source)?.flatten() {
        copy_entry(target, &entry, allow_executables)?;
    }
    Ok(())
}

fn copy_entry(
    target: &Path,
    entry: &fs::DirEntry,
    allow_executables: bool,
) -> Result<(), SkillError> {
    let path = entry.path();
    let (metadata, kind) = classify_entry(&path)?;
    match kind {
        EntryKind::Directory => {
            let destination = target.join(entry.file_name());
            fs::create_dir_all(&destination)?;
            copy_tree(&path, &destination, allow_executables)
        }
        EntryKind::File => {
            ensure_not_executable(&path, &metadata, allow_executables)?;
            fs::copy(&path, target.join(entry.file_name()))?;
            Ok(())
        }
    }
}

fn stage_skill(root: &Path, source: &Path, allow_executables: bool) -> Result<PathBuf, SkillError> {
    let temp = unique_sibling(root, "install");
    fs::create_dir_all(&temp)?;
    if let Err(error) = copy_tree(source, &temp, allow_executables) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    Ok(temp)
}

fn install_name(
    metadata: &SkillMetadata,
    options: &InstallOptions,
) -> Result<(String, Option<String>), SkillError> {
    validate_name(options.name.as_deref().unwrap_or(&metadata.name))?;
    if metadata.description.trim().is_empty() {
        return Err(SkillError::Validation(
            "description must not be empty".to_owned(),
        ));
    }
    let Some(name) = options.name.clone() else {
        return Ok((metadata.name.clone(), None));
    };
    if name == metadata.name {
        return Ok((name, None));
    }
    Ok((name, Some(metadata.name.clone())))
}

fn read_source_metadata(skill_root: &Path) -> Result<SkillMetadata, SkillError> {
    let path = skill_root.join("SKILL.md");
    read_skill_metadata(&path)
        .map_err(|error| SkillError::Source(error.to_string()))?
        .ok_or_else(|| {
            SkillError::Validation(format!(
                "{} has invalid or missing frontmatter",
                path.display()
            ))
        })
}

fn renamed_bytes(skill_root: &Path, name: &str, bytes: u64) -> Result<u64, SkillError> {
    let text = fs::read_to_string(skill_root.join("SKILL.md"))?;
    let rewritten = rename_frontmatter(&text, name)?;
    Ok(bytes.saturating_sub(text.len() as u64) + rewritten.len() as u64)
}

/// Rewrite the frontmatter `name` line of a staged `SKILL.md`.
fn rename_staged_skill(staged: &Path, name: &str) -> Result<(), SkillError> {
    let path = staged.join("SKILL.md");
    let text = fs::read_to_string(&path)?;
    fs::write(&path, rename_frontmatter(&text, name)?)?;
    Ok(())
}

/// Replace the first `name:` line inside the frontmatter block.
fn rename_frontmatter(text: &str, name: &str) -> Result<String, SkillError> {
    let missing = || SkillError::Validation("SKILL.md has no frontmatter".to_owned());
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Err(missing());
    };
    if first.trim() != "---" {
        return Err(missing());
    }
    let (out, closed, renamed) = rename_block(first, lines, name);
    if !closed {
        return Err(missing());
    }
    if !renamed {
        return Err(SkillError::Validation(
            "SKILL.md frontmatter has no name line".to_owned(),
        ));
    }
    Ok(out)
}

fn rename_block<'a>(
    first: &str,
    lines: impl Iterator<Item = &'a str>,
    name: &str,
) -> (String, bool, bool) {
    let mut out = String::with_capacity(first.len());
    out.push_str(first);
    let mut closed = false;
    let mut renamed = false;
    for line in lines {
        if !closed && line.trim() == "---" {
            closed = true;
            out.push_str(line);
            continue;
        }
        if !closed && !renamed && line.starts_with("name:") {
            out.push_str("name: ");
            out.push_str(name);
            out.push_str(line_ending(line));
            renamed = true;
            continue;
        }
        out.push_str(line);
    }
    (out, closed, renamed)
}

fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn replace_skill_dir(staged: &Path, root: &Path, name: &str) -> Result<(), SkillError> {
    let target = root.join(name);
    let backup = unique_sibling(root, "backup");
    let had_existing = target.exists();
    if had_existing {
        fs::rename(&target, &backup)?;
    }
    if let Err(error) = fs::rename(staged, &target) {
        if had_existing {
            let _ = fs::rename(&backup, &target);
        }
        return Err(error.into());
    }
    if had_existing {
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

fn read_lock(path: &Path) -> Result<SkillLock, SkillError> {
    if !path.exists() {
        return Ok(SkillLock {
            version: 1,
            skills: BTreeMap::new(),
        });
    }
    let text = fs::read_to_string(path)?;
    let lock: SkillLock = serde_json::from_str(&text)
        .map_err(|error| SkillError::Lock(format!("{}: {error}", path.display())))?;
    if lock.version != 1 {
        return Err(SkillError::Lock(format!(
            "unsupported lock version {}",
            lock.version
        )));
    }
    Ok(lock)
}

fn write_lock(path: &Path, lock: &SkillLock) -> Result<(), SkillError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text =
        serde_json::to_string_pretty(lock).map_err(|error| SkillError::Lock(error.to_string()))?;
    let temp = unique_sibling(path.parent().unwrap_or(Path::new(".")), "lock");
    fs::write(&temp, text)?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn hash_tree(root: &Path) -> Result<String, SkillError> {
    let mut hasher = Sha256::new();
    let mut files = Vec::new();
    collect_relative_files(root, root, &mut files)?;
    files.sort();
    for relative in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(root.join(&relative))?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_relative_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_relative_files(root, &path, files)?;
        } else if metadata.is_file() {
            files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn unique_sibling(parent: &Path, prefix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(".{prefix}-{}-{stamp}", std::process::id()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slowcode-skills-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_skill(directory: &Path, name: &str) -> PathBuf {
        let skill = directory.join(name);
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\nbody\n"),
        )
        .unwrap();
        skill
    }

    #[test]
    fn installs_copies_and_records_the_lock() {
        let workspace = temp_dir("workspace");
        let source_root = temp_dir("source");
        let source = write_skill(&source_root, "release-note");
        fs::write(source.join("extra.txt"), "hello").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let outcome = installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();
        assert!(outcome.installed);
        assert!(
            workspace
                .join(".slowcode/skills/release-note/SKILL.md")
                .is_file()
        );
        assert!(
            workspace
                .join(".slowcode/skills/release-note/extra.txt")
                .is_file()
        );
        let lock = fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(lock.contains("release-note"));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn duplicate_install_requires_force() {
        let workspace = temp_dir("duplicate");
        let source_root = temp_dir("duplicate-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();
        assert!(matches!(
            installer.install(&source, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Exists(_))
        ));
        installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    force: true,
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn installs_into_a_role_root_and_lists_its_role() {
        let workspace = temp_dir("role-workspace");
        let source_root = temp_dir("role-source");
        let source = write_skill(&source_root, "role-skill");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);

        let outcome = installer
            .install(
                &source,
                &SkillScope::Role(RoleName::Navigator),
                &InstallOptions::default(),
            )
            .unwrap();

        assert!(outcome.installed);
        assert!(
            workspace
                .join(".slowcode/roles/navigator/skills/role-skill/SKILL.md")
                .is_file()
        );
        assert!(!workspace.join(".slowcode/skills/role-skill").exists());

        let listed = installer.list().unwrap();
        let skill = listed
            .iter()
            .find(|skill| skill.name == "role-skill")
            .expect("role skill is listed");
        assert_eq!(skill.scope, "role:navigator");
        assert_eq!(skill.role.as_deref(), Some("navigator"));

        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn invalid_frontmatter_is_rejected() {
        let workspace = temp_dir("invalid");
        let source_root = temp_dir("invalid-source");
        let skill = source_root.join("broken");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "not frontmatter").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        assert!(matches!(
            installer.preview(&skill, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Validation(_))
        ));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let workspace = temp_dir("symlink");
        let source_root = temp_dir("symlink-source");
        let source = write_skill(&source_root, "escape");
        let outside = temp_dir("outside");
        std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        assert!(matches!(
            installer.preview(&source, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Validation(_))
        ));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn remove_deletes_the_skill_and_lock_entry() {
        let workspace = temp_dir("remove");
        let source_root = temp_dir("remove-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();
        installer.remove("demo", &SkillScope::Project).unwrap();
        assert!(!workspace.join(".slowcode/skills/demo").exists());
        let lock = fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(!lock.contains("demo"));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn remove_deletes_skill_from_global_and_role_scopes() {
        let workspace = temp_dir("remove-scopes");
        let home = temp_dir("remove-scopes-home");
        let source_root = temp_dir("remove-scopes-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&home);
        let global = SkillScope::Global;
        let role = SkillScope::Role(RoleName::Driver);
        installer
            .install(&source, &global, &InstallOptions::default())
            .unwrap();
        installer
            .install(&source, &role, &InstallOptions::default())
            .unwrap();
        assert!(home.join(".slowcode/skills/demo").is_dir());
        assert!(
            workspace
                .join(".slowcode/roles/driver/skills/demo")
                .is_dir()
        );

        installer.remove("demo", &global).unwrap();
        installer.remove("demo", &role).unwrap();

        assert!(!home.join(".slowcode/skills/demo").exists());
        assert!(
            !workspace
                .join(".slowcode/roles/driver/skills/demo")
                .exists()
        );
        let home_lock = fs::read_to_string(home.join(".slowcode/skills-lock.json")).unwrap();
        assert!(!home_lock.contains("demo"));
        let workspace_lock =
            fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(!workspace_lock.contains("demo"));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn remove_only_deletes_the_requested_scope() {
        let workspace = temp_dir("remove-one-scope");
        let home = temp_dir("remove-one-scope-home");
        let source_root = temp_dir("remove-one-scope-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&home);
        let role = SkillScope::Role(RoleName::Navigator);
        installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();
        installer
            .install(&source, &role, &InstallOptions::default())
            .unwrap();

        installer.remove("demo", &role).unwrap();

        assert!(
            !workspace
                .join(".slowcode/roles/navigator/skills/demo")
                .exists()
        );
        assert!(workspace.join(".slowcode/skills/demo/SKILL.md").is_file());
        let lock = fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(lock.contains("project:demo"));
        assert!(!lock.contains("role:navigator:demo"));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn corrupt_lock_is_not_overwritten() {
        let workspace = temp_dir("corrupt-lock");
        let source_root = temp_dir("corrupt-lock-source");
        let source = write_skill(&source_root, "demo");
        fs::create_dir_all(workspace.join(".slowcode")).unwrap();
        let lock_path = workspace.join(".slowcode/skills-lock.json");
        fs::write(&lock_path, "not json").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        assert!(matches!(
            installer.install(&source, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Lock(_))
        ));
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), "not json");
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn force_reinstall_replaces_previous_contents() {
        let workspace = temp_dir("force-replace");
        let source_root = temp_dir("force-replace-source");
        let source = write_skill(&source_root, "demo");
        fs::write(source.join("extra.txt"), "first").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();

        fs::write(source.join("extra.txt"), "second").unwrap();
        let outcome = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    force: true,
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert!(outcome.installed);
        assert_eq!(
            fs::read_to_string(workspace.join(".slowcode/skills/demo/extra.txt")).unwrap(),
            "second"
        );
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn dry_run_previews_without_copying_or_locking() {
        let workspace = temp_dir("dry-run");
        let source_root = temp_dir("dry-run-source");
        let source = write_skill(&source_root, "demo");
        fs::write(source.join("extra.txt"), "hello").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let outcome = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    dry_run: true,
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert!(!outcome.installed);
        assert_eq!(outcome.preview.files, 2);
        assert_eq!(
            outcome.preview.bytes,
            fs::metadata(source.join("SKILL.md")).unwrap().len() + 5
        );
        assert!(!outcome.preview.overwrites);
        assert!(!workspace.join(".slowcode/skills/demo").exists());
        assert!(!workspace.join(".slowcode/skills-lock.json").exists());
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn missing_source_is_rejected() {
        let workspace = temp_dir("missing-source");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let missing = workspace.join("does-not-exist");
        assert!(matches!(
            installer.preview(&missing, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Source(_))
        ));
        assert!(matches!(
            installer.install(&missing, &SkillScope::Project, &InstallOptions::default()),
            Err(SkillError::Source(_))
        ));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn source_without_skill_md_is_rejected() {
        let workspace = temp_dir("no-skill-md");
        let source_root = temp_dir("no-skill-md-source");
        fs::write(source_root.join("README.md"), "nothing here").unwrap();
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        assert!(matches!(
            installer.preview(
                &source_root,
                &SkillScope::Project,
                &InstallOptions::default()
            ),
            Err(SkillError::Validation(_))
        ));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn single_nested_directory_is_treated_as_the_skill_root() {
        let workspace = temp_dir("nested-source");
        let source_root = temp_dir("nested-source-root");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let outcome = installer
            .install(
                &source_root,
                &SkillScope::Project,
                &InstallOptions::default(),
            )
            .unwrap();
        assert_eq!(outcome.preview.source, source);
        assert!(workspace.join(".slowcode/skills/demo/SKILL.md").is_file());
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn name_override_is_used_for_the_install_target() {
        let workspace = temp_dir("name-override");
        let source_root = temp_dir("name-override-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let outcome = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert_eq!(outcome.preview.name, "renamed");
        assert_eq!(outcome.preview.renamed_from.as_deref(), Some("demo"));
        assert!(
            workspace
                .join(".slowcode/skills/renamed/SKILL.md")
                .is_file()
        );
        let metadata = read_skill_metadata(&workspace.join(".slowcode/skills/renamed/SKILL.md"))
            .unwrap()
            .expect("renamed skill keeps valid frontmatter");
        assert_eq!(metadata.name, "renamed");
        let lock = fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(lock.contains("\"project:renamed\""));
        assert!(!lock.contains("\"project:demo\""));
        for invalid in ["Bad Name", "..", "", "-leading"] {
            assert!(
                matches!(
                    installer.install(
                        &source,
                        &SkillScope::Project,
                        &InstallOptions {
                            name: Some(invalid.to_owned()),
                            ..InstallOptions::default()
                        },
                    ),
                    Err(SkillError::Validation(_))
                ),
                "{invalid:?} must be rejected"
            );
        }
        let unchanged = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("demo".to_owned()),
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert_eq!(unchanged.preview.renamed_from, None);
        assert_eq!(
            fs::read_to_string(workspace.join(".slowcode/skills/demo/SKILL.md")).unwrap(),
            "---\nname: demo\ndescription: test skill\n---\nbody\n"
        );
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn dry_run_reports_a_rename_without_writing() {
        let workspace = temp_dir("dry-run-rename");
        let source_root = temp_dir("dry-run-rename-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let outcome = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    dry_run: true,
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert!(!outcome.installed);
        assert_eq!(outcome.preview.name, "renamed");
        assert_eq!(outcome.preview.renamed_from.as_deref(), Some("demo"));
        assert!(
            !workspace.join(".slowcode/skills/renamed").exists(),
            "dry run must not create the target"
        );
        assert!(!workspace.join(".slowcode/skills-lock.json").exists());
        assert_eq!(
            outcome.preview.bytes,
            fs::metadata(source.join("SKILL.md")).unwrap().len() + 3,
            "the preview must count the rewritten name line"
        );
        let installed = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert!(installed.installed);
        assert_eq!(
            installed.preview, outcome.preview,
            "the dry run must match the real install preview"
        );
        assert_eq!(
            installed.preview.bytes,
            fs::metadata(workspace.join(".slowcode/skills/renamed/SKILL.md"))
                .unwrap()
                .len(),
            "the preview byte count must match the installed file"
        );
        assert_eq!(
            fs::read_to_string(source.join("SKILL.md")).unwrap(),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
            "the source skill must not be rewritten"
        );
        let conflict = installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    dry_run: true,
                    ..InstallOptions::default()
                },
            )
            .expect("a dry run reports the conflict instead of failing");
        assert!(!conflict.installed);
        assert!(conflict.preview.overwrites);
        assert!(matches!(
            installer.install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    ..InstallOptions::default()
                },
            ),
            Err(SkillError::Exists(_))
        ));
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn global_rename_records_the_user_lock() {
        let workspace = temp_dir("global-rename");
        let home = temp_dir("global-rename-home");
        let source_root = temp_dir("global-rename-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&home);
        installer
            .install(
                &source,
                &SkillScope::Global,
                &InstallOptions {
                    name: Some("renamed".to_owned()),
                    ..InstallOptions::default()
                },
            )
            .unwrap();
        assert!(home.join(".slowcode/skills/renamed/SKILL.md").is_file());
        let lock = fs::read_to_string(home.join(".slowcode/skills-lock.json")).unwrap();
        assert!(lock.contains("\"global:renamed\""));
        assert!(!workspace.join(".slowcode/skills-lock.json").exists());
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn rename_target_uses_the_force_rule() {
        let workspace = temp_dir("rename-force");
        let source_root = temp_dir("rename-force-source");
        let source = write_skill(&source_root, "demo");
        let installer = SkillInstaller::new(&workspace).with_home(&workspace);
        let renamed = InstallOptions {
            name: Some("renamed".to_owned()),
            ..InstallOptions::default()
        };
        installer
            .install(&source, &SkillScope::Project, &renamed)
            .unwrap();
        assert!(matches!(
            installer.install(&source, &SkillScope::Project, &renamed),
            Err(SkillError::Exists(_))
        ));
        installer
            .install(
                &source,
                &SkillScope::Project,
                &InstallOptions {
                    force: true,
                    ..renamed
                },
            )
            .unwrap();
        fs::remove_dir_all(workspace).unwrap();
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn rename_frontmatter_preserves_crlf_and_body() {
        let text = "---\r\nname: demo\r\ndescription: d\r\n---\r\nbody\r\n";
        let renamed = rename_frontmatter(text, "renamed").unwrap();
        assert_eq!(
            renamed,
            "---\r\nname: renamed\r\ndescription: d\r\n---\r\nbody\r\n"
        );
    }

    #[test]
    fn rename_frontmatter_rejects_missing_blocks_and_name_lines() {
        for text in [
            "",
            "body only\n",
            "---\ndescription: d\n",
            "---\ndescription: d\n---\nbody\n",
        ] {
            assert!(matches!(
                rename_frontmatter(text, "renamed"),
                Err(SkillError::Validation(_))
            ));
        }
    }

    #[test]
    fn rename_frontmatter_ignores_body_name_lines_and_keeps_field_order() {
        let text = "---\ndescription: d\nname: demo\n---\nname: body-mention\n";
        assert_eq!(
            rename_frontmatter(text, "renamed").unwrap(),
            "---\ndescription: d\nname: renamed\n---\nname: body-mention\n"
        );
    }

    #[test]
    fn rename_frontmatter_handles_a_fence_without_a_trailing_newline() {
        let text = "---\nname: demo\n---";
        assert_eq!(
            rename_frontmatter(text, "renamed").unwrap(),
            "---\nname: renamed\n---"
        );
    }

    #[test]
    fn walk_collects_relative_paths_and_byte_total() {
        let root = temp_dir("walk");
        fs::write(root.join("a.txt"), "abc").unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/b.txt"), "de").unwrap();

        let mut files = Vec::new();
        let mut bytes = 0_u64;
        walk(&root, &root, false, &mut files, &mut bytes).unwrap();

        files.sort();
        assert_eq!(
            files,
            vec![PathBuf::from("a.txt"), PathBuf::from("nested/b.txt")]
        );
        assert_eq!(bytes, 5);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn walk_rejects_oversized_files() {
        let root = temp_dir("walk-oversized");
        let file = fs::File::create(root.join("big.bin")).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);
        let mut files = Vec::new();
        let mut bytes = 0_u64;
        assert!(matches!(
            walk(&root, &root, false, &mut files, &mut bytes),
            Err(SkillError::Validation(_))
        ));
        assert!(files.is_empty());
        assert_eq!(bytes, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn walk_rejects_executables_unless_allowed() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("walk-executable");
        let script = root.join("run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let mut files = Vec::new();
        let mut bytes = 0_u64;
        assert!(matches!(
            walk(&root, &root, false, &mut files, &mut bytes),
            Err(SkillError::Validation(_))
        ));

        let mut files = Vec::new();
        let mut bytes = 0_u64;
        walk(&root, &root, true, &mut files, &mut bytes).unwrap();
        assert_eq!(files, vec![PathBuf::from("run.sh")]);
        assert_eq!(bytes, 10);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn walk_rejects_symlinks() {
        let root = temp_dir("walk-symlink");
        let outside = temp_dir("walk-symlink-outside");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let mut files = Vec::new();
        let mut bytes = 0_u64;
        assert!(matches!(
            walk(&root, &root, false, &mut files, &mut bytes),
            Err(SkillError::Validation(_))
        ));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn copy_tree_copies_nested_directories() {
        let source = temp_dir("copy-tree-source");
        fs::write(source.join("a.txt"), "abc").unwrap();
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/b.txt"), "de").unwrap();
        let target = temp_dir("copy-tree-target");

        copy_tree(&source, &target, false).unwrap();

        assert_eq!(fs::read_to_string(target.join("a.txt")).unwrap(), "abc");
        assert_eq!(
            fs::read_to_string(target.join("nested/b.txt")).unwrap(),
            "de"
        );
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_rejects_symlinks() {
        let source = temp_dir("copy-tree-symlink-source");
        let outside = temp_dir("copy-tree-symlink-outside");
        std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
        let target = temp_dir("copy-tree-symlink-target");
        assert!(matches!(
            copy_tree(&source, &target, false),
            Err(SkillError::Validation(_))
        ));
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn copy_tree_rejects_executables_unless_allowed() {
        use std::os::unix::fs::PermissionsExt;

        let source = temp_dir("copy-tree-executable-source");
        let script = source.join("run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let target = temp_dir("copy-tree-executable-target");

        assert!(matches!(
            copy_tree(&source, &target, false),
            Err(SkillError::Validation(_))
        ));
        assert!(!target.join("run.sh").exists());

        copy_tree(&source, &target, true).unwrap();
        assert!(target.join("run.sh").is_file());
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
    }
}
