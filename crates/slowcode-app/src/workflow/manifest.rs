//! Minimal per-command skill manifest.
//!
//! The kernel owns runtime Skill discovery. This module records the same
//! workspace roots for the workflow journal so `/status` can explain which
//! Skills a phase could load.

use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

use super::{
    contract::sha256_hex,
    state::{CapabilityProfile, Phase},
};

/// Shared roots scanned for every profile.
const SHARED_SKILL_ROOTS: &[&str] = &[".slowcode/skills", ".agents/skills"];

/// One Skill visible to a phase.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SkillManifestEntry {
    pub name: String,
    /// Workspace-relative Skill directory.
    pub source: String,
    pub sha256: String,
    #[serde(default)]
    pub description: String,
}

/// Context sources used by one Plan command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextManifest {
    pub phase: Phase,
    #[serde(default)]
    pub profile: Option<String>,
    pub skill_roots: Vec<String>,
    pub skills: Vec<SkillManifestEntry>,
}

impl ContextManifest {
    #[must_use]
    pub fn skill_names(&self) -> Vec<&str> {
        self.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }
}

/// Build the Skill manifest for one phase and profile.
///
/// Roots are scanned in role-first order, matching the kernel. Duplicate names
/// are resolved by the first root that contains them.
#[must_use]
pub fn build_skill_manifest(
    workspace: &Path,
    phase: Phase,
    profile: Option<CapabilityProfile>,
) -> ContextManifest {
    let mut roots = Vec::new();
    if let Some(profile) = profile {
        roots.push(format!(".slowcode/roles/{}/skills", profile.role_name()));
    }
    roots.extend(SHARED_SKILL_ROOTS.iter().map(|root| (*root).to_owned()));

    let mut skills = BTreeMap::new();
    for root in &roots {
        let root_path = workspace.join(root);
        let Ok(entries) = fs::read_dir(&root_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let metadata_path = path.join("SKILL.md");
            let Some((name, description)) = read_skill_metadata(&metadata_path) else {
                continue;
            };
            let Ok(bytes) = fs::read(&metadata_path) else {
                continue;
            };
            let source = relative_source(workspace, &path);
            skills.entry(name.clone()).or_insert(SkillManifestEntry {
                name,
                source,
                sha256: sha256_hex(&bytes),
                description,
            });
        }
    }

    ContextManifest {
        phase,
        profile: profile.map(|profile| profile.as_str().to_owned()),
        skill_roots: roots,
        skills: skills.into_values().collect(),
    }
}

fn relative_source(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_skill_metadata(path: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(path).ok()?;
    let mut in_frontmatter = false;
    let mut name = None;
    let mut description = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = value.trim().trim_matches(['"', '\'']);
            if !value.is_empty() {
                name = Some(value.to_owned());
            }
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            description = value.trim().trim_matches(['"', '\'']).to_owned();
        }
    }
    name.map(|name| (name, description))
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn temp_workspace() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("slowcode-manifest-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("workspace");
        path
    }

    fn write_skill(workspace: &Path, root: &str, dir: &str, name: &str) {
        let path = workspace.join(root).join(dir);
        fs::create_dir_all(&path).expect("skill dir");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\nBody.\n"),
        )
        .expect("skill");
    }

    #[test]
    fn role_manifest_includes_role_and_shared_skills_in_order() {
        let workspace = temp_workspace();
        write_skill(
            &workspace,
            ".slowcode/roles/navigator/skills",
            "navigate",
            "navigate-skill",
        );
        write_skill(&workspace, ".slowcode/skills", "shared", "shared-skill");
        write_skill(&workspace, ".agents/skills", "agents", "agents-skill");
        write_skill(
            &workspace,
            ".slowcode/roles/driver/skills",
            "drive",
            "drive-skill",
        );
        write_skill(&workspace, ".dsh/skills", "legacy", "legacy-skill");

        let manifest = build_skill_manifest(
            &workspace,
            Phase::Review,
            Some(CapabilityProfile::Navigator),
        );
        assert_eq!(
            manifest.skill_names(),
            vec!["agents-skill", "navigate-skill", "shared-skill"]
        );
        assert!(
            manifest
                .skill_roots
                .contains(&".slowcode/skills".to_owned())
        );
        assert!(manifest.skills.iter().all(|skill| skill.sha256.len() == 64));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn driver_manifest_uses_the_driver_root() {
        let workspace = temp_workspace();
        write_skill(
            &workspace,
            ".slowcode/roles/navigator/skills",
            "navigate",
            "navigate-skill",
        );
        write_skill(
            &workspace,
            ".slowcode/roles/driver/skills",
            "drive",
            "drive-skill",
        );

        let manifest =
            build_skill_manifest(&workspace, Phase::TestFix, Some(CapabilityProfile::Driver));
        assert_eq!(manifest.skill_names(), vec!["drive-skill"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn duplicate_names_keep_the_first_root() {
        let workspace = temp_workspace();
        write_skill(
            &workspace,
            ".slowcode/roles/navigator/skills",
            "first",
            "same",
        );
        write_skill(&workspace, ".slowcode/skills", "second", "same");

        let manifest = build_skill_manifest(
            &workspace,
            Phase::Review,
            Some(CapabilityProfile::Navigator),
        );
        assert_eq!(manifest.skills.len(), 1);
        assert_eq!(
            manifest.skills[0].source,
            ".slowcode/roles/navigator/skills/first"
        );
        let _ = fs::remove_dir_all(&workspace);
    }
}
