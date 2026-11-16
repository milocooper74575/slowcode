//! Validated workspace-relative Skill roots.
//!
//! Roles may add extra directories to the shared Skill scan. The kernel only
//! accepts paths that stay inside the opened workspace, so a role cannot widen
//! the workspace authority boundary. The shared roots themselves live in
//! `skills.rs`; this type only carries role-contributed additions.

use std::path::{Component, PathBuf};

use thiserror::Error;

/// Maximum number of extra roots one role may contribute.
pub const MAX_EXTRA_SKILL_ROOTS: usize = 16;

/// Maximum UTF-8 byte length of one root path.
pub const MAX_SKILL_ROOT_BYTES: usize = 4_096;

/// Extra Skill roots contributed by a role.
///
/// Paths are workspace-relative. `.` components are dropped and duplicates are
/// removed while preserving the caller's order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillRoots {
    roots: Vec<PathBuf>,
}

impl SkillRoots {
    /// Validate and normalize role-contributed roots.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self, SkillRootError> {
        let mut normalized = Vec::new();
        for root in roots {
            let root = normalize_root(root)?;
            if !normalized.contains(&root) {
                normalized.push(root);
            }
        }
        if normalized.len() > MAX_EXTRA_SKILL_ROOTS {
            return Err(SkillRootError::TooMany {
                max: MAX_EXTRA_SKILL_ROOTS,
            });
        }
        Ok(Self { roots: normalized })
    }

    /// No role-contributed roots.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Normalized roots in contribution order.
    #[must_use]
    pub fn as_slice(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Iterate over normalized roots.
    pub fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.roots.iter()
    }

    /// Whether no role contributed a root.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Number of normalized roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }
}

fn normalize_root(root: PathBuf) -> Result<PathBuf, SkillRootError> {
    if root.as_os_str().is_empty() {
        return Err(SkillRootError::Empty);
    }
    if root.as_os_str().len() > MAX_SKILL_ROOT_BYTES {
        return Err(SkillRootError::TooLong { path: root });
    }
    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SkillRootError::OutsideWorkspace { path: root });
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(SkillRootError::Empty);
    }
    Ok(normalized)
}

/// A role-contributed Skill root is not usable.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SkillRootError {
    #[error("a Skill root must be a non-empty workspace-relative path")]
    Empty,
    #[error("Skill root `{path}` must stay inside the workspace")]
    OutsideWorkspace { path: PathBuf },
    #[error("Skill root `{path}` exceeds the retained-size limit")]
    TooLong { path: PathBuf },
    #[error("at most {max} extra Skill roots may be configured")]
    TooMany { max: usize },
}

#[cfg(test)]
mod tests {
    use super::{MAX_EXTRA_SKILL_ROOTS, MAX_SKILL_ROOT_BYTES, SkillRootError, SkillRoots};

    #[test]
    fn relative_roots_are_normalized_and_deduplicated() {
        let roots = SkillRoots::new([
            ".slowcode/roles/navigator/skills".into(),
            ".slowcode/./roles/navigator/skills".into(),
            ".slowcode/skills".into(),
        ])
        .expect("relative roots are valid");
        assert_eq!(
            roots.as_slice(),
            [
                std::path::PathBuf::from(".slowcode/roles/navigator/skills"),
                std::path::PathBuf::from(".slowcode/skills"),
            ]
        );
    }

    #[test]
    fn roots_cannot_escape_the_workspace() {
        for root in ["/abs/skills", "../skills", "a/../../skills"] {
            assert_eq!(
                SkillRoots::new([root.into()]),
                Err(SkillRootError::OutsideWorkspace { path: root.into() })
            );
        }
        assert_eq!(SkillRoots::new([".".into()]), Err(SkillRootError::Empty));
    }

    #[test]
    fn too_many_roots_are_rejected() {
        let roots = (0..=MAX_EXTRA_SKILL_ROOTS)
            .map(|index| format!("roles/{index}/skills").into())
            .collect::<Vec<_>>();
        assert_eq!(
            SkillRoots::new(roots),
            Err(SkillRootError::TooMany {
                max: MAX_EXTRA_SKILL_ROOTS
            })
        );
    }

    #[test]
    fn exactly_the_maximum_root_count_is_accepted() {
        let roots = (0..MAX_EXTRA_SKILL_ROOTS)
            .map(|index| format!("roles/{index}/skills").into())
            .collect::<Vec<_>>();
        let roots = SkillRoots::new(roots).expect("the maximum root count is valid");
        assert_eq!(roots.len(), MAX_EXTRA_SKILL_ROOTS);
    }

    #[test]
    fn empty_and_oversized_roots_are_rejected() {
        assert_eq!(
            SkillRoots::new([std::path::PathBuf::new()]),
            Err(SkillRootError::Empty)
        );
        let oversized = "a".repeat(MAX_SKILL_ROOT_BYTES + 1);
        assert_eq!(
            SkillRoots::new([oversized.clone().into()]),
            Err(SkillRootError::TooLong {
                path: oversized.into()
            })
        );
    }
}
