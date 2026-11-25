//! Static documentation site and the repository's single task source.
//!
//! The source of truth lives in `site/`:
//!
//! - `site/docs.json` mounts every Markdown page into a section.
//! - `site/tasks.json` is the only task status source.
//! - `site/content/*.md` holds the public documentation.
//! - `site/dist/` is generated and committed so the site works offline.
//!
//! `validate` checks hierarchy, task integrity and the roadmap main flow.
//! `build_site` validates and regenerates `site/dist`. `check_site` renders in
//! memory and fails when the committed output is stale, which is the CI gate.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use serde::Deserialize;
use thiserror::Error;

/// Path of the docs configuration, relative to the repository root.
pub const DOCS_CONFIG: &str = "site/docs.json";
/// Path of the single task source, relative to the repository root.
pub const TASKS_PATH: &str = "site/tasks.json";
/// Markdown source directory, relative to the repository root.
pub const DOCS_DIR: &str = "site/content";
/// Generated site directory, relative to the repository root.
pub const SITE_OUT: &str = "site/dist";

/// Start marker for the one main-flow section in the roadmap.
pub const MAIN_FLOW_START: &str = "<!-- docs:main-flow:start -->";
/// End marker for the one main-flow section in the roadmap.
pub const MAIN_FLOW_END: &str = "<!-- docs:main-flow:end -->";
/// Steps the main flow must name. Tokens are matched case-insensitively.
pub const MAIN_FLOW_TOKENS: [&str; 6] = [
    "plan", "approve", "scaffold", "continue", "distill", "feedback",
];

const RESERVED_PAGE_IDS: [&str; 3] = ["index", "tasks", "style"];

#[derive(Debug, Error)]
pub enum DocsError {
    #[error("无法读取 {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("无法解析 {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("文档站校验失败:\n{}", format_errors(.0))]
    Validation(Vec<String>),
    #[error("文档站已过期:\n{}", format_errors(.0))]
    Stale(Vec<String>),
    #[error("无法写入 {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn format_errors(errors: &[String]) -> String {
    errors
        .iter()
        .map(|error| format!("- {error}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Todo,
    InProgress,
    Done,
    Blocked,
    Deprecated,
}

impl Status {
    const ORDER: [Self; 5] = [
        Self::InProgress,
        Self::Todo,
        Self::Blocked,
        Self::Done,
        Self::Deprecated,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Todo => "待开始",
            Self::InProgress => "进行中",
            Self::Done => "已完成",
            Self::Blocked => "阻塞",
            Self::Deprecated => "已废弃",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Deprecated => "deprecated",
        }
    }

    fn rank(self) -> usize {
        Self::ORDER
            .iter()
            .position(|status| *status == self)
            .unwrap_or(Self::ORDER.len())
    }

    fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Deprecated)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    High,
    Medium,
    Low,
}

impl Priority {
    fn label(self) -> &'static str {
        match self {
            Self::High => "高",
            Self::Medium => "中",
            Self::Low => "低",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocsConfig {
    pub version: u32,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    pub pages: Vec<Page>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub id: String,
    pub file: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSource {
    pub version: u32,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub title: String,
    #[serde(default)]
    pub next_task: Option<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    pub phases: Vec<Phase>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub phase: String,
    pub status: Status,
    pub priority: Priority,
    pub title: String,
    pub docs: Vec<String>,
    pub acceptance: String,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

struct Site {
    config: DocsConfig,
    tasks: TaskSource,
    mapping: HashMap<String, String>,
}

/// Validate the docs hierarchy, task source and roadmap main flow.
pub fn validate(root: &Path) -> Result<(), DocsError> {
    load_site(root).map(|_| ())
}

fn load_site(root: &Path) -> Result<Site, DocsError> {
    let config: DocsConfig = read_json(&root.join(DOCS_CONFIG))?;
    let tasks: TaskSource = read_json(&root.join(TASKS_PATH))?;
    let docs_dir = root.join(DOCS_DIR);
    let mut errors = Vec::new();
    validate_config(&config, &docs_dir, &mut errors);
    let mapping = page_map(&config);
    validate_tasks(&tasks, &docs_dir, &mapping, &mut errors);
    validate_main_flow(&docs_dir.join("roadmap.md"), &mut errors);
    if errors.is_empty() {
        Ok(Site {
            config,
            tasks,
            mapping,
        })
    } else {
        Err(DocsError::Validation(errors))
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, DocsError> {
    let text = fs::read_to_string(path).map_err(|source| DocsError::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| DocsError::Json {
        path: path.to_owned(),
        source,
    })
}

fn validate_config(config: &DocsConfig, docs_dir: &Path, errors: &mut Vec<String>) {
    if config.version != 1 {
        errors.push(format!("docs.json: 不支持的 version {}", config.version));
    }
    if config.title.trim().is_empty() {
        errors.push("docs.json: 缺少 title".to_owned());
    }
    if config.sections.is_empty() {
        errors.push("docs.json: 缺少 sections".to_owned());
        return;
    }

    let mut seen_sections = HashSet::new();
    let mut seen_pages = HashSet::new();
    let mut covered = HashSet::new();
    for section in &config.sections {
        if !is_slug(&section.id) {
            errors.push(format!("非法 section id: {:?}", section.id));
        }
        if !seen_sections.insert(section.id.as_str()) {
            errors.push(format!("section id 重复: {}", section.id));
        }
        if section.pages.is_empty() {
            errors.push(format!("section {} 没有页面", section.id));
        }
        for page in &section.pages {
            if !is_slug(&page.id) {
                errors.push(format!("非法 page id: {:?}", page.id));
            }
            if RESERVED_PAGE_IDS.contains(&page.id.as_str()) {
                errors.push(format!("page id 不能占用保留名: {}", page.id));
            }
            if !seen_pages.insert(page.id.as_str()) {
                errors.push(format!("page id 重复: {}", page.id));
            }
            if !is_safe_relative_markdown(&page.file) {
                errors.push(format!(
                    "page {} 的 file 必须是相对 .md 路径: {:?}",
                    page.id, page.file
                ));
                continue;
            }
            let relative = PathBuf::from(&page.file);
            if !covered.insert(relative.clone()) {
                errors.push(format!("文件被重复挂载: {}", page.file));
            }
            let path = docs_dir.join(&relative);
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(_) => {
                    errors.push(format!("文档不存在: {}", path.display()));
                    continue;
                }
            };
            if !has_h1(&text) {
                errors.push(format!("{}: 缺少一级标题", page.file));
            }
        }
    }

    for relative in markdown_files(docs_dir) {
        if !covered.contains(&relative) {
            errors.push(format!(
                "文档未挂到 docs.json: {}",
                relative.to_string_lossy()
            ));
        }
    }
}

fn validate_tasks(
    tasks: &TaskSource,
    docs_dir: &Path,
    mapping: &HashMap<String, String>,
    errors: &mut Vec<String>,
) {
    if tasks.version != 1 {
        errors.push(format!("tasks.json: 不支持的 version {}", tasks.version));
    }
    if tasks.title.trim().is_empty() {
        errors.push("tasks.json: 缺少 title".to_owned());
    }
    if tasks.phases.is_empty() {
        errors.push("tasks.json: 缺少 phases".to_owned());
    }
    let mut phase_ids = HashSet::new();
    for phase in &tasks.phases {
        if phase.id.trim().is_empty() {
            errors.push("存在空 phase id".to_owned());
        }
        if !phase_ids.insert(phase.id.as_str()) {
            errors.push(format!("phase id 重复: {}", phase.id));
        }
        if phase.title.trim().is_empty() {
            errors.push(format!("phase {} 缺少 title", phase.id));
        }
    }
    if tasks.tasks.is_empty() {
        errors.push("tasks.json: 缺少 tasks".to_owned());
        return;
    }

    let mut ids = HashSet::new();
    for task in &tasks.tasks {
        if task.id.trim().is_empty() {
            errors.push("存在空 task id".to_owned());
        }
        if !ids.insert(task.id.as_str()) {
            errors.push(format!("task id 重复: {}", task.id));
        }
    }

    for task in &tasks.tasks {
        if !phase_ids.contains(task.phase.as_str()) {
            errors.push(format!("{}: 未知 phase {:?}", task.id, task.phase));
        }
        if task.title.trim().is_empty() {
            errors.push(format!("{}: 缺少 title", task.id));
        }
        if task.acceptance.trim().is_empty() {
            errors.push(format!("{}: 缺少 acceptance", task.id));
        }
        if task.docs.is_empty() {
            errors.push(format!("{}: 至少引用一个文档", task.id));
        }
        for doc in &task.docs {
            if !is_safe_relative_markdown(doc) {
                errors.push(format!("{}: 文档必须是相对 .md 路径: {doc:?}", task.id));
                continue;
            }
            if !docs_dir.join(doc).exists() {
                errors.push(format!("{}: 文档不存在 {doc}", task.id));
            }
            if !mapping.contains_key(doc) {
                errors.push(format!("{}: 文档未挂到 docs.json: {doc}", task.id));
            }
        }
        for dependency in &task.depends_on {
            if dependency == &task.id {
                errors.push(format!("{}: 不能依赖自己", task.id));
            } else if !ids.contains(dependency.as_str()) {
                errors.push(format!("{}: 未知依赖 {dependency}", task.id));
            }
        }
        match task.status {
            Status::Done if !non_empty(task.evidence.as_deref()) => {
                errors.push(format!("{}: done 任务缺少 evidence", task.id));
            }
            Status::Deprecated if !non_empty(task.reason.as_deref()) => {
                errors.push(format!("{}: deprecated 任务缺少 reason", task.id));
            }
            Status::InProgress if !non_empty(task.next_action.as_deref()) => {
                errors.push(format!("{}: in_progress 任务缺少 next_action", task.id));
            }
            _ => {}
        }
    }

    if let Some(next_task) = tasks.next_task.as_deref()
        && !next_task.trim().is_empty()
    {
        match tasks.tasks.iter().find(|task| task.id == next_task) {
            None => errors.push(format!("next_task 不存在: {next_task}")),
            Some(task) if task.status.is_closed() => errors.push(format!(
                "next_task 不能是 {}: {next_task}",
                task.status.slug()
            )),
            Some(_) => {}
        }
    }
}

fn validate_main_flow(path: &Path, errors: &mut Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => {
            errors.push(format!("缺少 roadmap: {}", path.display()));
            return;
        }
    };
    if text.matches(MAIN_FLOW_START).count() != 1 || text.matches(MAIN_FLOW_END).count() != 1 {
        errors.push("roadmap.md 必须有且只有一个主流程标记段落".to_owned());
        return;
    }
    let start = match text.find(MAIN_FLOW_START) {
        Some(index) => index + MAIN_FLOW_START.len(),
        None => return,
    };
    let end = match text.find(MAIN_FLOW_END) {
        Some(index) => index,
        None => return,
    };
    if start > end {
        errors.push("roadmap.md 主流程标记顺序错误".to_owned());
        return;
    }
    let body = text[start..end].to_lowercase();
    for token in MAIN_FLOW_TOKENS {
        if !body.contains(token) {
            errors.push(format!("roadmap.md 主流程缺少步骤: {token}"));
        }
    }
}

fn page_map(config: &DocsConfig) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    for section in &config.sections {
        for page in &section.pages {
            mapping.insert(page.file.clone(), page.id.clone());
            mapping.insert(format!("./{}", page.file), page.id.clone());
            mapping.insert(format!("content/{}", page.file), page.id.clone());
            mapping.insert(format!("site/content/{}", page.file), page.id.clone());
        }
    }
    mapping
}

fn is_slug(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn is_safe_relative_markdown(value: &str) -> bool {
    let path = Path::new(value);
    value.ends_with(".md")
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::RootDir
                    | Component::Prefix(_)
                    | Component::CurDir
            )
        })
}

fn has_h1(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("# ") && !line[2..].trim().is_empty())
}

fn non_empty(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn markdown_files(docs_dir: &Path) -> Vec<PathBuf> {
    fn collect(root: &Path, directory: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(root, &path, found);
            } else if path.extension().is_some_and(|extension| extension == "md")
                && let Ok(relative) = path.strip_prefix(root)
            {
                found.push(relative.to_path_buf());
            }
        }
    }

    let mut found = Vec::new();
    collect(docs_dir, docs_dir, &mut found);
    found.sort();
    found
}

/// Validate and regenerate `site/dist`.
pub fn build_site(root: &Path) -> Result<Vec<PathBuf>, DocsError> {
    let site = load_site(root)?;
    let files = render_all(root, &site)?;
    let out_dir = root.join(SITE_OUT);
    let expected: HashSet<PathBuf> = files.iter().map(|(path, _)| path.clone()).collect();
    fs::create_dir_all(&out_dir).map_err(|source| DocsError::Write {
        path: out_dir.clone(),
        source,
    })?;
    let mut written = Vec::with_capacity(files.len());
    for (path, content) in &files {
        write_atomic(path, content.as_bytes())?;
        written.push(path.clone());
    }
    for path in files_under(&out_dir) {
        if expected.contains(&path) {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        fs::remove_file(&path).map_err(|source| DocsError::Write {
            path: path.clone(),
            source,
        })?;
    }
    Ok(written)
}

/// Validate and fail when `site/dist` differs from the current sources.
pub fn check_site(root: &Path) -> Result<(), DocsError> {
    let site = load_site(root)?;
    let files = render_all(root, &site)?;
    let expected: HashSet<PathBuf> = files.iter().map(|(path, _)| path.clone()).collect();
    let mut stale = Vec::new();
    for (path, content) in files {
        match fs::read_to_string(&path) {
            Ok(existing) if existing == content => {}
            Ok(_) => stale.push(format!("内容不一致: {}", display_path(root, &path))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                stale.push(format!("缺少: {}", display_path(root, &path)));
            }
            Err(source) => {
                return Err(DocsError::Read {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    for path in files_under(&root.join(SITE_OUT)) {
        if expected.contains(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with('.') {
            stale.push(format!("多余文件: {}", display_path(root, &path)));
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(DocsError::Stale(stale))
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DocsError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "docs".to_owned());
    let temporary =
        path.with_file_name(format!(".{file_name}.tmp-{}-{serial}", std::process::id()));
    fs::write(&temporary, bytes).map_err(|source| DocsError::Write {
        path: temporary.clone(),
        source,
    })?;
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(DocsError::Write {
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

fn files_under(directory: &Path) -> Vec<PathBuf> {
    fn collect(directory: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    collect(directory, &mut found);
    found.sort();
    found
}

fn render_all(root: &Path, site: &Site) -> Result<Vec<(PathBuf, String)>, DocsError> {
    let out_dir = root.join(SITE_OUT);
    let docs_dir = root.join(DOCS_DIR);
    let mut files = Vec::new();
    files.push((out_dir.join("style.css"), format!("{}\n", CSS.trim())));
    for section in &site.config.sections {
        for page in &section.pages {
            files.push((
                out_dir.join(format!("{}.html", page.id)),
                render_page(site, section, page, &docs_dir)?,
            ));
        }
    }
    files.push((out_dir.join("index.html"), render_index(site)));
    files.push((out_dir.join("tasks.html"), render_tasks(site)));
    Ok(files)
}

fn render_page(
    site: &Site,
    section: &Section,
    page: &Page,
    docs_dir: &Path,
) -> Result<String, DocsError> {
    let path = docs_dir.join(&page.file);
    let source = fs::read_to_string(&path).map_err(|source| DocsError::Read {
        path: path.clone(),
        source,
    })?;
    let body = format!(
        "<p class=\"crumb\"><a href=\"./index.html\">总览</a> / {}</p>\
         <div class=\"markdown\">{}</div>",
        escape_html(&section.title),
        render_markdown(&source, &site.mapping)
    );
    Ok(shell(site, page.title.as_str(), Some(&page.id), &body))
}

fn render_index(site: &Site) -> String {
    let counts = count_statuses(&site.tasks);
    let stats = Status::ORDER
        .iter()
        .filter(|status| {
            **status != Status::Deprecated || counts.get(status).copied().unwrap_or(0) > 0
        })
        .map(|status| {
            format!(
                "<div class=\"stat\"><strong>{}</strong><span>{}</span></div>",
                counts.get(status).copied().unwrap_or(0),
                status.label()
            )
        })
        .collect::<String>();
    let mut sections = String::new();
    for section in &site.config.sections {
        let cards = section
            .pages
            .iter()
            .map(|page| {
                format!(
                    "<a class=\"card\" href=\"./{}.html\"><h2>{}</h2><p>{}</p></a>",
                    escape_html(&page.id),
                    escape_html(&page.title),
                    escape_html(&page.summary)
                )
            })
            .collect::<String>();
        sections.push_str(&format!(
            "<section class=\"phase\"><div class=\"phase-head\"><h2>{}</h2>\
             <span>{}/ 篇</span></div><div class=\"cards\">{cards}</div></section>",
            escape_html(&section.title),
            section.pages.len()
        ));
    }
    let body = format!(
        "<div class=\"hero\"><h1>{}</h1><p>{}</p><div class=\"stats\">{stats}</div></div>{sections}",
        escape_html(&site.config.title),
        escape_html(&site.config.subtitle)
    );
    shell(site, "首页", None, &body)
}

fn render_tasks(site: &Site) -> String {
    let counts = count_statuses(&site.tasks);
    let next = site
        .tasks
        .next_task
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            format!(
                "<p class=\"meta\">下一个任务：<strong>{}</strong></p>",
                escape_html(value)
            )
        })
        .unwrap_or_default();
    let stats = Status::ORDER
        .iter()
        .map(|status| {
            format!(
                "<div class=\"stat\"><strong>{}</strong><span>{}</span></div>",
                counts.get(status).copied().unwrap_or(0),
                status.label()
            )
        })
        .collect::<String>();
    let rules = if site.tasks.rules.is_empty() {
        String::new()
    } else {
        format!(
            "<section class=\"phase\"><ul class=\"rules\">{}</ul></section>",
            site.tasks
                .rules
                .iter()
                .map(|rule| format!("<li>{}</li>", escape_html(rule)))
                .collect::<String>()
        )
    };
    let source_order: HashMap<&str, usize> = site
        .tasks
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();
    let mut sections = String::new();
    for phase in &site.tasks.phases {
        let mut phase_tasks: Vec<&Task> = site
            .tasks
            .tasks
            .iter()
            .filter(|task| task.phase == phase.id)
            .collect();
        if phase_tasks.is_empty() {
            continue;
        }
        phase_tasks.sort_by_key(|task| {
            (
                task.status.rank(),
                source_order
                    .get(task.id.as_str())
                    .copied()
                    .unwrap_or(usize::MAX),
            )
        });
        sections.push_str(&format!(
            "<section class=\"phase\"><div class=\"phase-head\"><h2>{}</h2>\
             <span>{}/ 项</span></div>",
            escape_html(&phase.title),
            phase_tasks.len()
        ));
        for task in phase_tasks {
            let docs = task
                .docs
                .iter()
                .filter_map(|doc| {
                    site.mapping.get(doc).map(|id| {
                        format!(
                            "<a href=\"./{}.html\">{}</a>",
                            escape_html(id),
                            escape_html(doc)
                        )
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            let dependencies = if task.depends_on.is_empty() {
                "无".to_owned()
            } else {
                escape_html(&task.depends_on.join(", "))
            };
            let mut extra = String::new();
            if let Some(evidence) = task
                .evidence
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                extra.push_str(&format!(
                    "<p class=\"meta\">证据：{}</p>",
                    escape_html(evidence)
                ));
            }
            if let Some(reason) = task
                .reason
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                extra.push_str(&format!(
                    "<p class=\"meta\">原因：{}</p>",
                    escape_html(reason)
                ));
            }
            if let Some(next_action) = task
                .next_action
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                extra.push_str(&format!(
                    "<p class=\"meta\">下一步：{}</p>",
                    escape_html(next_action)
                ));
            }
            sections.push_str(&format!(
                "<div class=\"task\"><div class=\"task-head\">\
                 <span class=\"task-id\">{}</span>\
                 <span class=\"badge {}\">{}</span>\
                 <span class=\"meta\">优先级 {}</span></div>\
                 <div class=\"task-title\">{}</div>\
                 <p>验收：{}</p>\
                 <p class=\"meta\">文档：{} · 依赖：{dependencies}</p>{extra}</div>",
                escape_html(&task.id),
                task.status.slug(),
                task.status.label(),
                task.priority.label(),
                escape_html(&task.title),
                escape_html(&task.acceptance),
                if docs.is_empty() {
                    "—".to_owned()
                } else {
                    docs
                }
            ));
        }
        sections.push_str("</section>");
    }
    let body = format!(
        "<div class=\"hero\"><h1>任务源</h1>\
         <p>唯一任务状态源。任务只能从这里取，HANDOFF 只写交接上下文。</p>\
         {next}<div class=\"stats\">{stats}</div></div>{rules}{sections}"
    );
    shell(site, "任务源", Some("tasks"), &body)
}

fn count_statuses(tasks: &TaskSource) -> HashMap<Status, usize> {
    let mut counts = HashMap::new();
    for task in &tasks.tasks {
        *counts.entry(task.status).or_insert(0) += 1;
    }
    counts
}

fn sidebar(site: &Site, current_id: Option<&str>) -> String {
    let mut blocks = String::new();
    for section in &site.config.sections {
        let links = section
            .pages
            .iter()
            .map(|page| {
                let current = if current_id == Some(page.id.as_str()) {
                    " current"
                } else {
                    ""
                };
                format!(
                    "<a class=\"nav-link{current}\" href=\"./{}.html\">{}</a>",
                    escape_html(&page.id),
                    escape_html(&page.title)
                )
            })
            .collect::<String>();
        blocks.push_str(&format!(
            "<div class=\"nav-section\"><h2>{}</h2>{links}</div>",
            escape_html(&section.title)
        ));
    }
    let counts = count_statuses(&site.tasks);
    let task_current = if current_id == Some("tasks") {
        " current"
    } else {
        ""
    };
    format!(
        "<aside class=\"sidebar\">\
         <a class=\"brand\" href=\"./index.html\"><strong>{}</strong><span>{}</span></a>\
         {blocks}\
         <div class=\"nav-task\"><a class=\"nav-link{task_current}\" href=\"./tasks.html\">任务源</a>\
         <div class=\"meta\" style=\"padding:6px 9px 0\">{} 进行中 · {} 待开始</div></div></aside>",
        escape_html(&site.config.title),
        escape_html(&site.config.subtitle),
        counts.get(&Status::InProgress).copied().unwrap_or(0),
        counts.get(&Status::Todo).copied().unwrap_or(0)
    )
}

fn shell(site: &Site, title: &str, current_id: Option<&str>, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{} · {}</title><link rel=\"stylesheet\" href=\"./style.css\"></head><body>\
         <div class=\"layout\">{}<main class=\"content\"><div class=\"content-inner\">{body}\
         <div class=\"footer\">由 <code>slowcode-docs</code> 生成 · {} · \
         任务状态以 <code>site/tasks.json</code> 为准</div></div></main></div></body></html>",
        escape_html(title),
        escape_html(&site.config.title),
        sidebar(site, current_id),
        escape_html(site.config.updated_at.as_deref().unwrap_or("unknown"))
    )
}

fn render_markdown(text: &str, mapping: &HashMap<String, String>) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let events = Parser::new_ext(text, options).map(|event| match event {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: CowStr::from(rewrite_href(dest_url.as_ref(), mapping)),
            title,
            id,
        }),
        other => other,
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    rendered
}

fn rewrite_href(href: &str, mapping: &HashMap<String, String>) -> String {
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with('/')
        || href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with("data:")
    {
        return href.to_owned();
    }
    let (path, suffix) = match href.split_once('#') {
        Some((path, fragment)) => (path, format!("#{fragment}")),
        None => (href, String::new()),
    };
    if path.is_empty() {
        return href.to_owned();
    }
    let normalized = path.replace('\\', "/");
    let mut candidates = vec![normalized.clone()];
    for prefix in ["./", "content/", "site/content/"] {
        if let Some(stripped) = normalized.strip_prefix(prefix) {
            candidates.push(stripped.to_owned());
        }
    }
    for candidate in &candidates {
        if let Some(id) = mapping.get(candidate) {
            return format!("./{id}.html{suffix}");
        }
    }
    if let Some(name) = Path::new(&normalized)
        .file_name()
        .and_then(|name| name.to_str())
        && let Some(id) = mapping.get(name)
    {
        return format!("./{id}.html{suffix}");
    }
    href.to_owned()
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

const CSS: &str = r#"
:root {
  --bg: #f5f4f1;
  --paper: #fffdf9;
  --ink: #25231f;
  --muted: #716a61;
  --line: #e2dcd3;
  --accent: #2f5d7c;
  --accent-soft: #e1ebf1;
  --ok: #2f6b4f;
  --warn: #9a6a1f;
  --danger: #9a3b3b;
}
* { box-sizing: border-box; }
html { scroll-behavior: smooth; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--ink);
  font: 15px/1.72 -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC",
        "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
}
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
.layout { display: grid; grid-template-columns: 280px minmax(0, 1fr); min-height: 100vh; }
.sidebar {
  position: sticky; top: 0; align-self: start; height: 100vh; overflow: auto;
  padding: 28px 20px 40px; border-right: 1px solid var(--line);
  background: #f0eee9;
}
.brand { display: block; color: var(--ink); margin-bottom: 26px; }
.brand strong { display: block; font-size: 17px; letter-spacing: .02em; }
.brand span { display: block; color: var(--muted); font-size: 12px; margin-top: 5px; }
.nav-section { margin: 0 0 20px; }
.nav-section h2 {
  font-size: 11px; letter-spacing: .14em; text-transform: uppercase;
  color: var(--muted); margin: 0 0 7px; font-weight: 600;
}
.nav-section a {
  display: block; padding: 5px 9px; border-radius: 6px; color: var(--ink);
  font-size: 13.5px; line-height: 1.45;
}
.nav-section a:hover { background: var(--accent-soft); text-decoration: none; }
.nav-section a.current { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
.nav-task { margin-top: 24px; padding-top: 18px; border-top: 1px solid var(--line); }
.content { min-width: 0; padding: 48px clamp(24px, 5vw, 72px) 80px; }
.content-inner { max-width: 880px; margin: 0 auto; }
.crumb { color: var(--muted); font-size: 12.5px; margin: 0 0 18px; }
.crumb a { color: var(--muted); }
.page-title { font-size: 30px; line-height: 1.25; margin: 0 0 10px; letter-spacing: -.01em; }
.page-summary { color: var(--muted); margin: 0 0 34px; font-size: 15px; }
.markdown h1 { font-size: 28px; margin: 0 0 18px; }
.markdown h2 { font-size: 21px; margin: 38px 0 12px; padding-top: 10px; border-top: 1px solid var(--line); }
.markdown h3 { font-size: 17px; margin: 26px 0 8px; }
.markdown h4 { font-size: 15px; margin: 20px 0 6px; }
.markdown p { margin: 10px 0; }
.markdown ul, .markdown ol { padding-left: 22px; }
.markdown li { margin: 4px 0; }
.markdown blockquote {
  margin: 16px 0; padding: 10px 16px; border-left: 3px solid var(--accent);
  background: var(--paper); color: #5c534b;
}
.markdown code {
  font: 12.5px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  background: #eee9e1; padding: 1px 5px; border-radius: 4px;
}
.markdown pre {
  background: #272522; color: #f4eee7; padding: 16px 18px; border-radius: 8px;
  overflow: auto; line-height: 1.55;
}
.markdown pre code { background: none; padding: 0; color: inherit; }
.markdown table { border-collapse: collapse; width: 100%; margin: 16px 0; font-size: 13.5px; }
.markdown th, .markdown td { border: 1px solid var(--line); padding: 8px 10px; text-align: left; vertical-align: top; }
.markdown th { background: #f0ebe3; font-weight: 600; }
.markdown hr { border: 0; border-top: 1px solid var(--line); margin: 28px 0; }
.markdown img { max-width: 100%; }
.markdown a { overflow-wrap: anywhere; }
.hero { max-width: 880px; margin: 0 auto; }
.hero h1 { font-size: 38px; line-height: 1.18; margin: 0 0 14px; letter-spacing: -.02em; }
.hero p { color: var(--muted); font-size: 16px; max-width: 680px; }
.stats { display: flex; flex-wrap: wrap; gap: 10px; margin: 24px 0 40px; }
.stat { background: var(--paper); border: 1px solid var(--line); border-radius: 8px; padding: 10px 14px; }
.stat strong { display: block; font-size: 20px; }
.stat span { color: var(--muted); font-size: 12px; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 14px; }
.card {
  display: block; background: var(--paper); border: 1px solid var(--line);
  border-radius: 10px; padding: 18px 18px 16px; color: var(--ink);
}
.card:hover { border-color: var(--accent); text-decoration: none; }
.card h2 { font-size: 17px; margin: 0 0 7px; }
.card p { color: var(--muted); font-size: 13px; margin: 0; }
.phase { margin: 34px 0; }
.phase-head { display: flex; align-items: baseline; gap: 12px; border-bottom: 1px solid var(--line); padding-bottom: 8px; }
.phase-head h2 { font-size: 20px; margin: 0; }
.phase-head span { color: var(--muted); font-size: 12px; }
.rules { margin: 0; padding-left: 22px; color: var(--muted); font-size: 13px; }
.task {
  background: var(--paper); border: 1px solid var(--line); border-radius: 9px;
  padding: 14px 16px; margin: 12px 0;
}
.task-head { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
.task-id { font: 12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; color: var(--muted); }
.task-title { font-size: 15.5px; font-weight: 600; margin: 2px 0 6px; }
.badge { display: inline-block; border-radius: 999px; padding: 2px 9px; font-size: 11.5px; border: 1px solid transparent; }
.badge.done { color: var(--ok); background: #e4f0e9; }
.badge.in_progress { color: var(--accent); background: var(--accent-soft); }
.badge.todo { color: var(--muted); background: #f0ebe4; }
.badge.blocked { color: var(--danger); background: #f7e3e3; }
.badge.deprecated { color: var(--muted); background: #eceae7; text-decoration: line-through; }
.meta { color: var(--muted); font-size: 12.5px; margin: 5px 0; }
.task p { margin: 6px 0; font-size: 13.5px; }
.footer { max-width: 880px; margin: 56px auto 0; padding-top: 18px; border-top: 1px solid var(--line); color: var(--muted); font-size: 12px; }
@media (max-width: 860px) {
  .layout { grid-template-columns: 1fr; }
  .sidebar { position: static; height: auto; border-right: 0; border-bottom: 1px solid var(--line); }
  .content { padding-top: 28px; }
  .page-title, .hero h1 { font-size: 27px; }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "slowcode-docs-{}-{nonce}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(root.join(DOCS_DIR)).expect("docs dir");
        fs::create_dir_all(root.join(SITE_OUT)).expect("out dir");
        root
    }

    fn write_config(root: &Path, files: &[&str]) {
        let pages = files
            .iter()
            .map(|file| {
                format!(
                    "{{\"id\":\"{}\",\"file\":\"{file}\",\"title\":\"{file}\",\"summary\":\"\"}}",
                    file.trim_end_matches(".md").replace('_', "-")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let config = format!(
            "{{\"version\":1,\"title\":\"Docs\",\"subtitle\":\"\",\"sections\":[\
             {{\"id\":\"main\",\"title\":\"Main\",\"pages\":[{pages}]}}]}}"
        );
        fs::write(root.join(DOCS_CONFIG), config).expect("config");
    }

    fn write_task_source(root: &Path, tasks: &str, next_task: &str) {
        let payload = format!(
            "{{\"version\":1,\"title\":\"Tasks\",\"next_task\":\"{next_task}\",\
             \"phases\":[{{\"id\":\"P0\",\"title\":\"P0\"}}],\"tasks\":[{tasks}]}}"
        );
        fs::write(root.join(TASKS_PATH), payload).expect("tasks");
    }

    fn write_roadmap(root: &Path, body: &str) {
        fs::write(
            root.join(DOCS_DIR).join("roadmap.md"),
            format!("# Roadmap\n{MAIN_FLOW_START}\n{body}\n{MAIN_FLOW_END}\n"),
        )
        .expect("roadmap");
    }

    fn valid_page(root: &Path, name: &str) {
        fs::write(root.join(DOCS_DIR).join(name), format!("# {name}\n")).expect("page");
    }

    fn valid_fixture(root: &Path) {
        valid_page(root, "roadmap.md");
        valid_page(root, "overview.md");
        write_roadmap(
            root,
            "plan -> approve -> scaffold -> continue -> distill -> feedback",
        );
        write_config(root, &["roadmap.md", "overview.md"]);
        write_task_source(
            root,
            "{\"id\":\"DOCS-001\",\"phase\":\"P0\",\"status\":\"done\",\"priority\":\"high\",\
             \"title\":\"docs\",\"docs\":[\"overview.md\"],\"acceptance\":\"x\",\
             \"evidence\":\"built\"}",
            "DOCS-002",
        );
        // The next task must exist, so append it to the source.
        let payload = fs::read_to_string(root.join(TASKS_PATH)).expect("tasks");
        let with_next = payload.replace(
            "]}",
            ",{\"id\":\"DOCS-002\",\"phase\":\"P0\",\"status\":\"todo\",\"priority\":\"medium\",\
             \"title\":\"next\",\"docs\":[\"overview.md\"],\"acceptance\":\"y\"}]}",
        );
        fs::write(root.join(TASKS_PATH), with_next).expect("tasks");
    }

    #[test]
    fn valid_fixture_passes() {
        let root = temp_root();
        valid_fixture(&root);
        validate(&root).expect("fixture validates");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn orphan_markdown_is_reported() {
        let root = temp_root();
        valid_fixture(&root);
        valid_page(&root, "orphan.md");
        let errors = validate(&root).expect_err("orphan fails");
        let message = errors.to_string();
        assert!(
            message.contains("文档未挂到 docs.json: orphan.md"),
            "{message}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_document_is_reported() {
        let root = temp_root();
        valid_fixture(&root);
        write_config(&root, &["roadmap.md", "missing.md"]);
        let errors = validate(&root).expect_err("missing fails");
        let message = errors.to_string();
        assert!(message.contains("文档不存在"), "{message}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn done_task_needs_evidence_and_dependencies_must_exist() {
        let root = temp_root();
        valid_fixture(&root);
        write_task_source(
            &root,
            "{\"id\":\"T1\",\"phase\":\"P0\",\"status\":\"done\",\"priority\":\"high\",\
             \"title\":\"no evidence\",\"docs\":[\"overview.md\"],\"acceptance\":\"x\",\
             \"depends_on\":[\"NOPE\"]}",
            "NOPE",
        );
        let errors = validate(&root).expect_err("task fails");
        let message = errors.to_string();
        assert!(message.contains("done 任务缺少 evidence"), "{message}");
        assert!(message.contains("未知依赖 NOPE"), "{message}");
        assert!(message.contains("next_task 不存在: NOPE"), "{message}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn main_flow_requires_every_step() {
        let root = temp_root();
        valid_fixture(&root);
        write_roadmap(&root, "plan -> approve -> scaffold");
        let errors = validate(&root).expect_err("flow fails");
        let message = errors.to_string();
        assert!(message.contains("缺少步骤: continue"), "{message}");
        assert!(message.contains("缺少步骤: distill"), "{message}");
        assert!(message.contains("缺少步骤: feedback"), "{message}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn internal_doc_links_are_rewritten() {
        let mapping = HashMap::from([("workflow.md".to_owned(), "workflow".to_owned())]);
        assert_eq!(
            rewrite_href("site/content/workflow.md#gate", &mapping),
            "./workflow.html#gate"
        );
        assert_eq!(
            rewrite_href("https://example.com/workflow.md", &mapping),
            "https://example.com/workflow.md"
        );
        let rendered = render_markdown("[workflow](workflow.md)", &mapping);
        assert!(rendered.contains("href=\"./workflow.html\""), "{rendered}");
    }

    #[test]
    fn task_page_puts_unfinished_before_done_in_each_phase() {
        let tasks = TaskSource {
            version: 1,
            updated_at: None,
            title: "Tasks".to_owned(),
            next_task: Some("T2".to_owned()),
            rules: Vec::new(),
            phases: vec![Phase {
                id: "P0".to_owned(),
                title: "P0".to_owned(),
            }],
            tasks: vec![
                Task {
                    id: "T1".to_owned(),
                    phase: "P0".to_owned(),
                    status: Status::Done,
                    priority: Priority::High,
                    title: "done first".to_owned(),
                    docs: Vec::new(),
                    acceptance: "x".to_owned(),
                    evidence: Some("y".to_owned()),
                    depends_on: Vec::new(),
                    next_action: None,
                    reason: None,
                },
                Task {
                    id: "T2".to_owned(),
                    phase: "P0".to_owned(),
                    status: Status::Todo,
                    priority: Priority::High,
                    title: "todo".to_owned(),
                    docs: Vec::new(),
                    acceptance: "x".to_owned(),
                    evidence: None,
                    depends_on: Vec::new(),
                    next_action: None,
                    reason: None,
                },
                Task {
                    id: "T3".to_owned(),
                    phase: "P0".to_owned(),
                    status: Status::Done,
                    priority: Priority::Low,
                    title: "done last".to_owned(),
                    docs: Vec::new(),
                    acceptance: "x".to_owned(),
                    evidence: Some("y".to_owned()),
                    depends_on: Vec::new(),
                    next_action: None,
                    reason: None,
                },
                Task {
                    id: "T4".to_owned(),
                    phase: "P0".to_owned(),
                    status: Status::InProgress,
                    priority: Priority::High,
                    title: "running".to_owned(),
                    docs: Vec::new(),
                    acceptance: "x".to_owned(),
                    evidence: None,
                    depends_on: Vec::new(),
                    next_action: Some("go".to_owned()),
                    reason: None,
                },
            ],
        };
        let site = Site {
            config: DocsConfig {
                version: 1,
                updated_at: None,
                title: "Docs".to_owned(),
                subtitle: String::new(),
                sections: Vec::new(),
            },
            tasks,
            mapping: HashMap::new(),
        };
        let html = render_tasks(&site);
        let positions = ["T1", "T2", "T3", "T4"]
            .map(|id| html.find(&format!("task-id\">{id}<")).expect("task"));
        assert!(positions[3] < positions[1]);
        assert!(positions[1] < positions[0]);
        assert!(positions[0] < positions[2]);
    }

    #[test]
    fn build_check_round_trip_detects_stale_output() {
        let root = temp_root();
        valid_fixture(&root);
        build_site(&root).expect("build");
        check_site(&root).expect("fresh output passes");
        let overview = root.join(SITE_OUT).join("overview.html");
        fs::write(&overview, "stale").expect("overwrite");
        let error = check_site(&root).expect_err("stale output fails");
        assert!(error.to_string().contains("内容不一致"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_removes_stale_generated_pages() {
        let root = temp_root();
        valid_fixture(&root);
        let stale = root.join(SITE_OUT).join("old.html");
        let hidden = root.join(SITE_OUT).join(".keep");
        fs::write(&stale, "old").expect("stale");
        fs::write(&hidden, "keep").expect("hidden");
        build_site(&root).expect("build");
        assert!(!stale.exists(), "stale page must be removed");
        assert!(hidden.exists(), "hidden files are left alone");
        let _ = fs::remove_dir_all(root);
    }
}
