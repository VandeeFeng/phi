use std::fs;
use std::path::{Path, PathBuf};

use super::config::{MemoryConfig, merge_tape_config};
use super::hooks::HookConfig;
use super::tape::context::SmartTapePrecompute;
use super::tape::gate::resolve_tape_gate;
use super::tape::{TapeAnchor, TapeConfig, TapeService};
use super::types::{MemoryContextKind, MemoryFile, MemoryFrontmatter, MemorySettings};
use super::utils::{DEFAULT_TAPE_EXCLUDE_DIRS, escape_xml};
use crate::context::InjectedContext;
use crate::transcript::Transcript;
pub use crate::utils::path::expand_home_path;
use crate::utils::workspace::WorkspaceMeta;

pub const DEFAULT_MEMORY_SCAN: (u64, u64) = (72, 168);
pub const DEFAULT_GLOBAL_MEMORY_DIRNAME: &str = "global";

/// One rendered `<memory_context>` section within an injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContext {
    content: String,
    kind: MemoryContextKind,
    file_count: usize,
}

impl MemoryContext {
    pub(crate) fn new(content: String, kind: MemoryContextKind, file_count: usize) -> Self {
        Self {
            content,
            kind,
            file_count,
        }
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn kind(&self) -> MemoryContextKind {
        self.kind
    }

    pub fn file_count(&self) -> usize {
        self.file_count
    }
}

/// The complete prompt-time memory injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInjection {
    memory_contexts: Vec<MemoryContext>,
}

impl MemoryInjection {
    pub(crate) fn new(memory_contexts: Vec<MemoryContext>) -> Self {
        Self { memory_contexts }
    }

    pub fn memory_contexts(&self) -> &[MemoryContext] {
        &self.memory_contexts
    }

    pub fn file_count(&self) -> usize {
        self.memory_contexts
            .iter()
            .map(MemoryContext::file_count)
            .sum()
    }

    pub fn with_retrieval_hint(mut self) -> Self {
        if let Some(context) = self.memory_contexts.last_mut() {
            context.content.push_str(
                "\nIf needed, use memory_search to look up specific content and tape_read to read relevant conversation history.",
            );
        }
        self
    }

    pub fn into_injected_contexts(self) -> Vec<InjectedContext> {
        self.memory_contexts
            .into_iter()
            .map(|context| InjectedContext {
                source: context.kind.source(),
                content: context.content,
            })
            .collect()
    }
}

pub fn default_local_path() -> PathBuf {
    crate::utils::path::phi_config_dir().join("memory")
}

pub fn default_settings() -> MemorySettings {
    MemorySettings {
        enabled: Some(true),
        repo_url: Some(String::new()),
        local_path: Some(default_local_path().display().to_string()),
        hooks: Some(HookConfig::default()),
        injection: None,
        memory_dir: Some(Default::default()),
        tape: Some(TapeConfig {
            enabled: Some(false),
            only_git: Some(true),
            exclude_dirs: DEFAULT_TAPE_EXCLUDE_DIRS
                .iter()
                .map(|dir| dir.to_string())
                .collect(),
            context: Some(super::tape::types::TapeContextConfig {
                strategy: Some(super::tape::types::TapeContextStrategy::Smart),
                file_limit: Some(10),
                memory_scan: Some(DEFAULT_MEMORY_SCAN),
                always_include: Vec::new(),
                whitelist: Vec::new(),
                blacklist: Vec::new(),
            }),
            anchor: Some(super::tape::types::TapeAnchorConfig {
                label_prefix: Some("⚓ ".to_string()),
                mode: Some(super::tape::types::TapeAnchorMode::Auto),
                keywords: Some(Default::default()),
            }),
        }),
    }
}

pub fn normalize_memory_scan_range(memory_scan: Option<(u64, u64)>) -> (u64, u64) {
    let (start_hours, max_hours) = memory_scan.unwrap_or(DEFAULT_MEMORY_SCAN);
    let start_hours = start_hours.max(1);
    (start_hours, max_hours.max(start_hours))
}

pub fn settings_from_config(config: &MemoryConfig) -> MemorySettings {
    let mut settings = default_settings();
    settings.enabled = Some(config.enabled);
    settings.memory_dir = config.memory_dir.clone();
    if let Some(local_path) = config
        .memory_dir
        .as_ref()
        .and_then(|dir| dir.local_path.clone())
    {
        settings.local_path = Some(local_path);
    }
    if let Some(repo_url) = config
        .memory_dir
        .as_ref()
        .and_then(|dir| dir.repo_url.clone())
    {
        settings.repo_url = Some(repo_url);
    }
    settings.injection = config.injection;
    settings.hooks = config.hooks.clone();
    if let Some(tape) = config.tape.clone() {
        settings.tape = Some(merge_tape_config(settings.tape.take(), tape));
    }
    settings
}

pub fn get_memory_repo_dir(settings: &MemorySettings) -> PathBuf {
    settings
        .memory_dir
        .as_ref()
        .and_then(|dir| dir.local_path.as_ref())
        .or(settings.local_path.as_ref())
        .map(expand_home_path)
        .unwrap_or_else(default_local_path)
}

pub fn get_memory_dir(settings: &MemorySettings, working_dir: &Path) -> anyhow::Result<PathBuf> {
    Ok(get_memory_repo_dir(settings).join(WorkspaceMeta::discover(working_dir, None)?.name()))
}

pub fn get_global_memory_dir(settings: &MemorySettings) -> Option<PathBuf> {
    let global_memory = settings.memory_dir.as_ref()?.global_memory.as_ref()?;
    if global_memory.trim().is_empty() {
        return None;
    }
    let safe_name = Path::new(global_memory)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim_matches('.').is_empty())
        .unwrap_or(DEFAULT_GLOBAL_MEMORY_DIRNAME);
    Some(get_memory_repo_dir(settings).join(safe_name))
}

pub fn get_memory_core_dir(memory_dir: &Path) -> PathBuf {
    memory_dir.join("core")
}

pub fn get_memory_user_dir(memory_dir: &Path) -> PathBuf {
    get_memory_core_dir(memory_dir).join("user")
}

pub fn tape_gate_enabled(settings: &MemorySettings, working_dir: &Path) -> bool {
    resolve_tape_gate(
        working_dir.to_string_lossy().as_ref(),
        settings.tape.as_ref(),
    )
    .is_ok_and(|gate| gate.enabled)
}

pub fn is_memory_initialized(memory_dir: &Path) -> bool {
    get_memory_core_dir(memory_dir).exists()
}

pub fn read_memory_file(file_path: &Path) -> anyhow::Result<MemoryFile> {
    parse_memory_file_content(file_path, &fs::read_to_string(file_path)?)
}

pub fn list_memory_files(memory_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let Ok(metadata) = fs::symlink_metadata(memory_dir) else {
        return Ok(files);
    };
    if metadata.file_type().is_symlink() {
        return Ok(files);
    }
    if metadata.is_file() {
        if memory_dir
            .extension()
            .is_some_and(|extension| extension == "md")
        {
            files.push(memory_dir.to_path_buf());
        }
        return Ok(files);
    }
    collect_memory_files(memory_dir, &mut files)?;
    Ok(files)
}

fn collect_memory_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_memory_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_memory_file_content(file_path: &Path, content: &str) -> anyhow::Result<MemoryFile> {
    let (frontmatter, body) = parse_frontmatter(content).unwrap_or_default();
    Ok(MemoryFile {
        path: file_path.display().to_string(),
        frontmatter,
        content: body.unwrap_or(content).to_string(),
    })
}

fn parse_frontmatter(content: &str) -> Option<(MemoryFrontmatter, Option<&str>)> {
    let rest = content.strip_prefix("---\n")?;
    let (yaml, body) = rest.split_once("\n---\n")?;
    let frontmatter = serde_yaml::from_str(yaml).unwrap_or_else(|_| MemoryFrontmatter {
        description: "No description".to_string(),
        ..MemoryFrontmatter::default()
    });
    Some((frontmatter, Some(body)))
}

pub fn write_memory_file(
    file_path: &Path,
    content: &str,
    frontmatter: &MemoryFrontmatter,
) -> anyhow::Result<()> {
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(frontmatter)?;
    fs::write(file_path, format!("---\n{yaml}---\n{content}"))?;
    Ok(())
}

pub fn format_memory_context(context: &str) -> String {
    if context.trim_start().starts_with("<memory_context") {
        context.to_string()
    } else {
        format!("<memory_context mode=\"normal\">\n{context}\n</memory_context>")
    }
}

pub fn count_memory_context_files(context: &str) -> usize {
    context
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with('-')
                || line.starts_with("<memory_file ")
                || (line.starts_with("<node ")
                    && line.contains("selected=\"true\"")
                    && (line.contains("kind=\"memory_file\"")
                        || line.contains("kind=\"project_file\"")))
        })
        .count()
}

pub fn memory_context_item(entry: MemoryContextItem<'_>) -> Vec<String> {
    vec![
        format!("- path: {}", entry.path),
        format!(
            "  priority: {}",
            entry.priority.unwrap_or(MemoryPriority::Normal).as_str()
        ),
        format!(
            "  description: {}",
            entry.description.unwrap_or("No description")
        ),
        format!("  tags: {}", entry.tags.unwrap_or("none")),
    ]
}

pub fn memory_context_header(
    mode: MemoryContextMode,
    options: MemoryContextOptions,
) -> Vec<String> {
    let mut lines = vec![format!("<memory_context mode=\"{}\">", mode.as_str())];
    match mode {
        MemoryContextMode::Normal => lines.extend([
            "<instructions>".to_string(),
            "These memory files can help you better understand the project and the user."
                .to_string(),
            "</instructions>".to_string(),
        ]),
        MemoryContextMode::Tape => {
            lines.extend([
                "<instructions>".to_string(),
                "Tape is enabled for this conversation. Use tape tools when you need anchors or tape history.".to_string(),
            ]);
            if options.anchor_mode == Some(AnchorMode::Manual) {
                lines.push("Anchor mode: manual. Tape mutations require permission confirmation; a matching keyword independently authorizes only its `tape_anchor`.".to_string());
            }
            lines.push("</instructions>".to_string());
        }
    }
    lines
}

pub fn memory_context(entries: &[MemoryContextEntry<'_>], mode: MemoryContextMode) -> Vec<String> {
    let mut lines = memory_context_header(mode, MemoryContextOptions::default());
    for entry in entries {
        lines.extend(memory_context_item(MemoryContextItem {
            path: entry.path,
            description: Some(&entry.memory.frontmatter.description),
            tags: Some(&entry.memory.frontmatter.tags.join(", ")),
            priority: None,
        }));
    }
    lines
}

pub fn build_memory_context(
    settings: &MemorySettings,
    working_dir: &Path,
) -> anyhow::Result<String> {
    let project_memory_dir = get_memory_dir(settings, working_dir)?;
    let global_memory_dir = get_global_memory_dir(settings);
    let mut lines = memory_context(&[], MemoryContextMode::Normal);

    if let Some(global_dir) = global_memory_dir.filter(|dir| *dir != project_memory_dir) {
        append_memory_context_section(&mut lines, "global", &global_dir, &global_dir)?;
    }
    append_memory_context_section(
        &mut lines,
        "project",
        &project_memory_dir,
        &get_memory_core_dir(&project_memory_dir),
    )?;

    lines.push("</memory_context>".to_string());
    Ok(lines.join("\n"))
}

pub fn build_tape_injection_with_prompt(
    settings: &MemorySettings,
    working_dir: &Path,
    transcript: &Transcript,
    prompt: Option<&str>,
    precompute: Option<&SmartTapePrecompute>,
) -> anyhow::Result<Option<MemoryInjection>> {
    let tape_context = settings
        .tape
        .as_ref()
        .and_then(|tape| tape.context.clone())
        .unwrap_or_default();
    let strategy = tape_context.strategy.unwrap_or_default();
    let limit = tape_context.file_limit.unwrap_or(10);
    let memory_scan = normalize_memory_scan_range(tape_context.memory_scan);
    let workspace = WorkspaceMeta::discover(working_dir, None)?;
    let project_memory_dir = get_memory_repo_dir(settings).join(workspace.name());
    let service = TapeService::create(
        &project_memory_dir,
        workspace.name(),
        transcript.id.clone(),
        working_dir.to_string_lossy().to_string(),
    )?;
    let mut selector = super::tape::context::MemoryFileSelector::new(
        &project_memory_dir,
        &workspace,
        tape_context.whitelist,
        tape_context.blacklist,
    );
    let selected = match precompute.filter(|_| strategy.as_str() == "smart") {
        Some(precompute) => selector.select_from_precompute(
            precompute,
            limit,
            &service,
            transcript,
            prompt.unwrap_or_default(),
        )?,
        None => selector.select_files_for_context_with_tape(
            strategy.as_str(),
            limit,
            memory_scan,
            &service,
            transcript,
        )?,
    };
    let mut files = tape_context.always_include;
    files.extend(selected.clone());
    let files = selector.finalize_context_files(files)?;
    let highlighted = selected
        .into_iter()
        .filter(|file_path| files.contains(file_path))
        .take(3)
        .collect::<Vec<_>>();
    let anchor_mode = settings
        .tape
        .as_ref()
        .and_then(|tape| tape.anchor.as_ref())
        .and_then(|anchor| anchor.mode);
    selector.build_context_from_files_with_highlights(files, &highlighted, anchor_mode)
}

pub fn build_memory_injection(
    settings: &MemorySettings,
    working_dir: &Path,
) -> anyhow::Result<Option<MemoryInjection>> {
    let content = build_memory_context(settings, working_dir)?;
    let file_count = count_memory_context_files(&content);
    Ok((file_count > 0).then(|| {
        MemoryInjection::new(vec![MemoryContext::new(
            content,
            MemoryContextKind::Static,
            file_count,
        )])
    }))
}

pub fn precompute_tape_context(
    settings: &MemorySettings,
    working_dir: &Path,
    transcript: &Transcript,
) -> anyhow::Result<Option<SmartTapePrecompute>> {
    let tape_context = settings
        .tape
        .as_ref()
        .and_then(|tape| tape.context.clone())
        .unwrap_or_default();
    if tape_context.strategy.unwrap_or_default().as_str() != "smart" {
        return Ok(None);
    }
    let gate = resolve_tape_gate(
        working_dir.to_string_lossy().as_ref(),
        settings.tape.as_ref(),
    )?;
    if !gate.enabled {
        return Ok(None);
    }
    let workspace = gate
        .workspace
        .ok_or_else(|| anyhow::anyhow!("enabled Tape gate has no workspace"))?;
    let project_memory_dir = get_memory_repo_dir(settings).join(workspace.name());
    let service = TapeService::create(
        &project_memory_dir,
        workspace.name(),
        transcript.id.clone(),
        working_dir.to_string_lossy().to_string(),
    )?;
    let mut selector = super::tape::context::MemoryFileSelector::new(
        &project_memory_dir,
        &workspace,
        tape_context.whitelist,
        tape_context.blacklist,
    );
    selector
        .precompute_smart_with_tape(
            normalize_memory_scan_range(tape_context.memory_scan),
            &service,
            transcript,
        )
        .map(Some)
}

fn append_memory_context_section(
    lines: &mut Vec<String>,
    source: &str,
    memory_dir: &Path,
    scan_dir: &Path,
) -> anyhow::Result<()> {
    let files = list_memory_files(scan_dir)?;
    if files.is_empty() {
        return Ok(());
    }
    lines.push(format!(
        "<memory_files source=\"{source}\" directory=\"{}\">",
        escape_xml(&memory_dir.display().to_string())
    ));
    for file in files {
        let memory = read_memory_file(&file)?;
        lines.extend(memory_context_item(MemoryContextItem {
            path: &memory.path,
            description: Some(&memory.frontmatter.description),
            tags: Some(&memory.frontmatter.tags.join(", ")),
            priority: None,
        }));
    }
    lines.push("</memory_files>".to_string());
    Ok(())
}

pub fn tape_system_prompt() -> String {
    let mut lines = memory_context_header(MemoryContextMode::Tape, MemoryContextOptions::default());
    lines.push("</memory_context>".to_string());
    lines.join("\n")
}

pub fn tape_anchor_context(anchors: &[TapeAnchor]) -> String {
    let mut lines = memory_context_header(MemoryContextMode::Tape, MemoryContextOptions::default());
    lines.push("<tape_anchors>".to_string());
    lines.extend(anchors.iter().map(render_tape_anchor));
    lines.push("</tape_anchors>".to_string());
    lines.push("</memory_context>".to_string());
    lines.join("\n")
}

fn render_tape_anchor(anchor: &TapeAnchor) -> String {
    let mut lines = vec![
        format!("- id: {}", anchor.id),
        format!("  name: {}", anchor.name),
    ];
    if let Some(summary) = anchor.meta.as_ref().and_then(|meta| meta.summary.as_ref()) {
        lines.push(format!("  summary: {summary}"));
    }
    if let Some(purpose) = anchor.meta.as_ref().and_then(|meta| meta.purpose.as_ref()) {
        lines.push(format!("  purpose: {purpose}"));
    }
    lines.join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryContextMode {
    Normal,
    Tape,
}

impl MemoryContextMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Tape => "tape",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryContextOptions {
    pub anchor_mode: Option<AnchorMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorMode {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryContextEntry<'a> {
    pub path: &'a str,
    pub memory: &'a MemoryFile,
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryContextItem<'a> {
    pub path: &'a str,
    pub description: Option<&'a str>,
    pub tags: Option<&'a str>,
    pub priority: Option<MemoryPriority>,
}

#[derive(Debug, Clone, Copy)]
pub enum MemoryPriority {
    Normal,
    High,
}

impl MemoryPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_memory_files() {
        use std::os::unix::fs::symlink;

        let directory =
            std::env::temp_dir().join(format!("phi-sdk-memory-core-{}", uuid::Uuid::new_v4()));
        let outside_file = directory.join("outside.md");
        let memory_dir = directory.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(&outside_file, "Memory").unwrap();
        symlink(&outside_file, memory_dir.join("linked.md")).unwrap();

        assert!(list_memory_files(&memory_dir).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
