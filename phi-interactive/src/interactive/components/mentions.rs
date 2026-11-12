use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use phi_tui::{RenderContext, SelectItem, SelectMenu, SelectMenuState, StyledLine, Widget};

use crate::runtime::attachment::{AgentAttachment, AgentCatalog};

#[derive(Default)]
pub(in crate::interactive) struct AgentMentions {
    catalog: AgentCatalog,
}

impl AgentMentions {
    pub(in crate::interactive) async fn refresh(
        &mut self,
        attachment: &AgentAttachment,
    ) -> anyhow::Result<&AgentCatalog> {
        self.catalog = attachment.supported_agents().await?;
        Ok(&self.catalog)
    }
}

const MAX_MENTIONS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MentionKind {
    File,
    Agent,
}

#[derive(Debug, Clone)]
pub(crate) struct SelectedMention {
    pub kind: MentionKind,
    pub value: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MentionState {
    pub menu: SelectMenuState,
    directory: Option<PathBuf>,
    files: Vec<String>,
    items: Vec<SelectItem>,
    selected: Vec<SelectedMention>,
    mode_hint: Option<String>,
}

impl Default for MentionState {
    fn default() -> Self {
        Self {
            menu: SelectMenuState::closed(),
            directory: None,
            files: Vec::new(),
            items: Vec::new(),
            selected: Vec::new(),
            mode_hint: None,
        }
    }
}

impl MentionState {
    pub(in crate::interactive) fn for_directory(directory: &Path) -> Self {
        Self {
            directory: directory.is_absolute().then(|| directory.to_path_buf()),
            ..Self::default()
        }
    }

    pub(super) fn render_menu(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        SelectMenu {
            title: None,
            items: self.items.clone(),
            selected: self.menu.selected,
            max_visible: 5,
        }
        .render(context)
    }

    pub fn refresh(&mut self, input: &str, agents: &AgentCatalog) {
        let Some(query) = mention_query(input) else {
            self.close();
            return;
        };
        if self.files.is_empty()
            && let Some(directory) = &self.directory
        {
            self.files = discover_files_at(directory);
        }

        let mut selected = Vec::new();
        let mut items = Vec::new();
        let agent_matches = match_agents(agents, query.text);
        for agent in agent_matches {
            selected.push(SelectedMention {
                kind: MentionKind::Agent,
                value: agent.name.clone(),
            });
            items.push(SelectItem::with_label(
                format!("agent:{}", agent.name),
                agent.name.clone(),
                format!("[agent] {}", agent.description),
            ));
        }

        for path in match_files(
            &self.files,
            query.text,
            MAX_MENTIONS.saturating_sub(items.len()),
        ) {
            selected.push(SelectedMention {
                kind: MentionKind::File,
                value: path.clone(),
            });
            let label = file_name(&path).to_string();
            items.push(SelectItem::with_label(format!("file:{path}"), label, path));
        }

        self.items = items;
        self.selected = selected;
        if self.items.is_empty() {
            self.menu.close();
        } else {
            self.menu.open();
            self.menu.clamp(self.items.len());
        }
    }

    pub(crate) fn suggest_mode(&mut self, input: &str) {
        self.mode_hint = Some(input.to_string());
    }

    pub(super) fn has_mode_hint(&self, input: &str) -> bool {
        self.mode_hint.as_deref() == Some(input)
    }

    pub fn close(&mut self) {
        self.mode_hint = None;
        self.menu.close();
        self.items.clear();
        self.selected.clear();
    }

    pub fn is_open(&self) -> bool {
        self.menu.open && !self.items.is_empty()
    }

    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    pub fn selected(&self) -> Option<&SelectedMention> {
        self.selected.get(self.menu.selected)
    }
}

pub(crate) fn complete_mention(input: &mut String, selected: &SelectedMention) {
    let Some(range) = mention_range(input) else {
        return;
    };
    let prefix = match selected.kind {
        MentionKind::File | MentionKind::Agent => "@",
    };
    input.replace_range(range.start.., &format!("{prefix}{} ", selected.value));
}

pub(crate) fn file_mention_context(input: &str, agents: &AgentCatalog) -> Option<String> {
    let paths = mentioned_paths(input, agents);
    if paths.is_empty() {
        return None;
    }

    Some(format_references(&paths))
}

fn format_references(paths: &[String]) -> String {
    let references = paths
        .iter()
        .map(|path| format!("<file path=\"{path}\" />"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The user referenced these files. Read them with the read tool if needed:\n<file-references>\n{references}\n</file-references>"
    )
}

#[derive(Debug, Clone, Copy)]
struct MentionQuery<'a> {
    text: &'a str,
}

#[derive(Debug, Clone, Copy)]
struct MentionRange<'a> {
    start: usize,
    query: MentionQuery<'a>,
}

fn mention_query(input: &str) -> Option<MentionQuery<'_>> {
    mention_range(input).map(|range| range.query)
}

fn mention_range(input: &str) -> Option<MentionRange<'_>> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(index, c)| c.is_whitespace().then_some(index + c.len_utf8()))
        .unwrap_or(0);
    let token = &input[start..];
    let rest = token.strip_prefix('@')?;
    if rest.starts_with('@') {
        return None;
    }
    Some(MentionRange {
        start,
        query: MentionQuery { text: rest },
    })
}

fn match_agents<'a>(
    agents: &'a AgentCatalog,
    query: &str,
) -> Vec<&'a crate::resources::AgentDefinitionView> {
    let query = query.to_lowercase();
    agents
        .all()
        .iter()
        .filter(|agent| {
            let name = agent.name.to_lowercase();
            query.is_empty()
                || name.contains(&query)
                || crate::interactive::completion::fuzzy_match(&name, &query).is_some()
        })
        .take(MAX_MENTIONS)
        .collect()
}

fn discover_files_at(root: &Path) -> Vec<String> {
    WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| relative_path(root, entry.path()))
        .filter(|path| !is_ignored_path(path))
        .collect()
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let path = path.strip_prefix(root).ok()?;
    Some(path.to_string_lossy().trim_start_matches('/').to_string())
}

fn match_files(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let mut matches = files
        .iter()
        .filter_map(|path| {
            crate::interactive::completion::completion_match_key(path, query)
                .map(|(gaps, length)| (gaps, length, path.clone()))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(gaps, len, _)| (*gaps, *len));
    matches
        .into_iter()
        .take(limit)
        .map(|(_, _, path)| path)
        .collect()
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn mentioned_paths(input: &str, agents: &AgentCatalog) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
        .filter(|token| !token.starts_with('@'))
        .filter(|path| !path.is_empty() && agents.get(path).is_none() && !is_ignored_path(path))
        .map(ToString::to_string)
        .collect()
}

fn is_ignored_path(path: &str) -> bool {
    path.split('/').any(|part| part == ".git") || is_env_path(path)
}

fn is_env_path(path: &str) -> bool {
    Path::new(path).components().any(|component| {
        matches!(component, std::path::Component::Normal(name)
            if name.to_str().is_some_and(|name| name == ".env" || name.starts_with(".env.")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_is_scoped_to_the_selected_directory() {
        let directory = std::env::temp_dir().join(format!("phi-mentions-{}", uuid::Uuid::new_v4()));
        let first = directory.join("first");
        let second = directory.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("first.txt"), "first").unwrap();
        std::fs::write(second.join("second.txt"), "second").unwrap();
        let agents = AgentCatalog::default();
        let mut state = MentionState::for_directory(&first);
        state.refresh("@", &agents);
        assert_eq!(state.selected().unwrap().value, "first.txt");
        state = MentionState::for_directory(&second);
        state.refresh("@", &agents);
        assert_eq!(state.selected().unwrap().value, "second.txt");
        state = MentionState::default();
        state.refresh("@", &agents);
        assert!(state.items().is_empty());
        state = MentionState::for_directory(Path::new("."));
        state.refresh("@", &agents);
        assert!(state.items().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mentions_complete_single_at_and_reject_double_at() {
        assert_eq!(mention_query("@probe").unwrap().text, "probe");
        assert!(mention_query("@@probe").is_none());
        let mut input = "@pro".to_string();
        complete_mention(
            &mut input,
            &SelectedMention {
                kind: MentionKind::Agent,
                value: "probe".into(),
            },
        );
        assert_eq!(input, "@probe ");
    }
}
