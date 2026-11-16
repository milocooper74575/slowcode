//! Project context for slowcode.
//!
//! `AGENTS.md` is injected by the kernel workspace-instruction runtime, so this
//! crate only reports its status. `GLOSSARY.md` is stable domain vocabulary and
//! enters the system prompt. `HANDOFF.md` is a pointer and is read on demand.

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// Default per-file read budget.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// How one project file entered the context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextStatus {
    Loaded { bytes: u64 },
    Pointer,
    Missing,
    Truncated { original: u64, kept: u64 },
    Error(String),
}

/// One context source and its current state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEntry {
    pub name: &'static str,
    pub path: PathBuf,
    pub status: ContextStatus,
}

impl ContextEntry {
    /// Rough token estimate for budgeting. This is deliberately labelled
    /// approximate at the display layer.
    pub fn estimated_tokens(&self) -> u64 {
        match &self.status {
            ContextStatus::Loaded { bytes } | ContextStatus::Truncated { kept: bytes, .. } => {
                bytes / 4
            }
            _ => 0,
        }
    }
}

/// Context configuration.
#[derive(Clone, Debug)]
pub struct ContextConfig {
    pub workspace: PathBuf,
    pub max_bytes: usize,
}

impl ContextConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Result of one context assembly.
#[derive(Clone, Debug)]
pub struct ContextBundle {
    /// Content appended to the stable system prompt.
    pub system_suffix: String,
    pub entries: Vec<ContextEntry>,
    pub warnings: Vec<String>,
}

/// Compose the system prompt by appending the stable glossary, when present.
pub fn compose_system_prompt(_base: &str, config: &ContextConfig) -> ContextBundle {
    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut suffix = String::new();

    let agents_path = config.workspace.join("AGENTS.md");
    entries.push(entry_for_agents(&agents_path));

    let glossary_path = config.workspace.join("GLOSSARY.md");
    match read_text(&glossary_path, config.max_bytes) {
        Ok(ReadText::Missing) => entries.push(ContextEntry {
            name: "GLOSSARY.md",
            path: glossary_path,
            status: ContextStatus::Missing,
        }),
        Ok(ReadText::Full { bytes, text }) => {
            entries.push(ContextEntry {
                name: "GLOSSARY.md",
                path: glossary_path,
                status: ContextStatus::Loaded { bytes },
            });
            suffix.push_str("\n\n## Domain glossary (GLOSSARY.md)\n\n");
            suffix.push_str(&text);
        }
        Ok(ReadText::Truncated {
            original,
            kept,
            text,
        }) => {
            entries.push(ContextEntry {
                name: "GLOSSARY.md",
                path: glossary_path,
                status: ContextStatus::Truncated { original, kept },
            });
            warnings.push(format!(
                "GLOSSARY.md truncated from {original} to {kept} bytes"
            ));
            suffix.push_str("\n\n## Domain glossary (GLOSSARY.md, truncated)\n\n");
            suffix.push_str(&text);
        }
        Err(error) => {
            entries.push(ContextEntry {
                name: "GLOSSARY.md",
                path: glossary_path,
                status: ContextStatus::Error(error.clone()),
            });
            warnings.push(format!("GLOSSARY.md could not be loaded: {error}"));
        }
    }

    let handoff_path = config.workspace.join("HANDOFF.md");
    entries.push(ContextEntry {
        name: "HANDOFF.md",
        path: handoff_path,
        status: ContextStatus::Pointer,
    });

    ContextBundle {
        system_suffix: suffix,
        entries,
        warnings,
    }
}

/// Report the three project files without consuming their content.
pub fn report(config: &ContextConfig) -> Vec<ContextEntry> {
    vec![
        entry_for_agents(&config.workspace.join("AGENTS.md")),
        status_entry("GLOSSARY.md", config.workspace.join("GLOSSARY.md")),
        ContextEntry {
            name: "HANDOFF.md",
            path: config.workspace.join("HANDOFF.md"),
            status: ContextStatus::Pointer,
        },
    ]
}

/// Render one entry for `/context`.
pub fn render_entry(entry: &ContextEntry) -> String {
    let status = match &entry.status {
        ContextStatus::Loaded { bytes } => format!("loaded, {bytes} bytes"),
        ContextStatus::Pointer => "pointer, read on demand".to_owned(),
        ContextStatus::Missing => "missing".to_owned(),
        ContextStatus::Truncated { original, kept } => {
            format!("truncated, {kept} of {original} bytes")
        }
        ContextStatus::Error(error) => format!("error: {error}"),
    };
    format!(
        "{}: {} ({} ~{} tokens)",
        entry.name,
        status,
        entry.path.display(),
        entry.estimated_tokens()
    )
}

/// Error type for context helpers.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("context file is not valid UTF-8")]
    Utf8,
    #[error("context file could not be read: {0}")]
    Io(String),
}

enum ReadText {
    Missing,
    Full {
        bytes: u64,
        text: String,
    },
    Truncated {
        original: u64,
        kept: u64,
        text: String,
    },
}

fn entry_for_agents(path: &Path) -> ContextEntry {
    status_entry("AGENTS.md", path.to_path_buf())
}

fn status_entry(name: &'static str, path: PathBuf) -> ContextEntry {
    let status = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => ContextStatus::Loaded {
            bytes: metadata.len(),
        },
        Ok(_) => ContextStatus::Error("not a regular file".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ContextStatus::Missing,
        Err(error) => ContextStatus::Error(error.to_string()),
    };
    ContextEntry { name, path, status }
}

fn read_text(path: &Path, max_bytes: usize) -> Result<ReadText, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReadText::Missing);
        }
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file() {
        return Err("not a regular file".to_owned());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let original = bytes.len() as u64;
    if bytes.len() <= max_bytes {
        let text = String::from_utf8(bytes).map_err(|_| "invalid UTF-8".to_owned())?;
        return Ok(ReadText::Full {
            bytes: original,
            text,
        });
    }
    let mut end = max_bytes;
    while end > 0 && !is_char_boundary(&bytes, end) {
        end -= 1;
    }
    let text = String::from_utf8(bytes[..end].to_vec())
        .map_err(|_| "invalid UTF-8 before truncation boundary".to_owned())?;
    Ok(ReadText::Truncated {
        original,
        kept: end as u64,
        text: format!("{text}\n\n[truncated at {end} bytes]\n"),
    })
}

fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0 || index >= bytes.len() || (bytes[index] & 0b1100_0000) != 0b1000_0000
}

impl fmt::Display for ContextStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loaded { bytes } => write!(formatter, "loaded, {bytes} bytes"),
            Self::Pointer => formatter.write_str("pointer"),
            Self::Missing => formatter.write_str("missing"),
            Self::Truncated { original, kept } => {
                write!(formatter, "truncated, {kept} of {original} bytes")
            }
            Self::Error(error) => write!(formatter, "error: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slowcode-context-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_files_are_reported_without_failing() {
        let workspace = temp_workspace("missing");
        let config = ContextConfig::new(&workspace);
        let bundle = compose_system_prompt("base", &config);
        assert_eq!(bundle.system_suffix, "");
        assert!(
            bundle
                .entries
                .iter()
                .any(|entry| entry.name == "AGENTS.md" && entry.status == ContextStatus::Missing)
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn glossary_enters_the_stable_suffix() {
        let workspace = temp_workspace("glossary");
        std::fs::write(workspace.join("GLOSSARY.md"), "**Term**: definition\n").unwrap();
        let config = ContextConfig::new(&workspace);
        let bundle = compose_system_prompt("base", &config);
        assert!(bundle.system_suffix.contains("Domain glossary"));
        assert!(bundle.system_suffix.contains("**Term**: definition"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn oversized_glossary_is_truncated_on_a_char_boundary() {
        let workspace = temp_workspace("truncate");
        std::fs::write(workspace.join("GLOSSARY.md"), "abcdefghij").unwrap();
        let mut config = ContextConfig::new(&workspace);
        config.max_bytes = 4;
        let bundle = compose_system_prompt("base", &config);
        assert!(bundle.system_suffix.contains("abcd"));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.contains("truncated"))
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn handoff_is_only_a_pointer() {
        let workspace = temp_workspace("handoff");
        std::fs::write(workspace.join("HANDOFF.md"), "secret progress").unwrap();
        let config = ContextConfig::new(&workspace);
        let bundle = compose_system_prompt("base", &config);
        assert!(!bundle.system_suffix.contains("secret progress"));
        assert!(
            bundle
                .entries
                .iter()
                .any(|entry| entry.name == "HANDOFF.md" && entry.status == ContextStatus::Pointer)
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn context_entries_render_every_status() {
        let path = PathBuf::from("/workspace/AGENTS.md");
        let cases = [
            (
                ContextStatus::Loaded { bytes: 400 },
                "loaded, 400 bytes",
                100,
            ),
            (
                ContextStatus::Truncated {
                    original: 800,
                    kept: 400,
                },
                "truncated, 400 of 800 bytes",
                100,
            ),
            (ContextStatus::Missing, "missing", 0),
            (ContextStatus::Pointer, "pointer", 0),
            (
                ContextStatus::Error("denied".to_owned()),
                "error: denied",
                0,
            ),
        ];
        for (status, label, tokens) in cases {
            let entry = ContextEntry {
                name: "AGENTS.md",
                path: path.clone(),
                status: status.clone(),
            };
            assert!(!status.to_string().is_empty(), "{status:?}");
            assert_eq!(entry.estimated_tokens(), tokens, "{status:?}");
            let rendered = render_entry(&entry);
            assert!(rendered.contains(label), "{rendered}");
            assert!(
                rendered.contains(&format!("~{tokens} tokens")),
                "{rendered}"
            );
        }
    }
}
