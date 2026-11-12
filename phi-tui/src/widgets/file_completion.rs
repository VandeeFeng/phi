use std::path::Path;

use ignore::WalkBuilder;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

use super::menu::{SelectItem, SelectMenuState};

const MAX_FILE_MENTIONS: usize = 50;

#[derive(Debug, Clone)]
pub struct FileMentionState {
    pub menu: SelectMenuState,
    files: Vec<String>,
    query: String,
    items: Vec<SelectItem>,
}

impl Default for FileMentionState {
    fn default() -> Self {
        Self {
            menu: SelectMenuState::closed(),
            files: Vec::new(),
            query: String::new(),
            items: Vec::new(),
        }
    }
}

impl FileMentionState {
    pub fn refresh(&mut self, input: &str) {
        let Some(query) = mention_query(input) else {
            self.close();
            return;
        };

        if self.files.is_empty() {
            self.files = discover_files();
        }
        self.query = query.to_string();
        self.items = match_files(&self.files, query);
        self.menu.open();
        self.menu.clamp(self.items.len());
    }

    pub fn close(&mut self) {
        self.menu.close();
        self.query.clear();
        self.items.clear();
    }

    pub fn is_open(&self) -> bool {
        self.menu.open && !self.items.is_empty()
    }

    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    pub fn selected_path(&self) -> Option<&str> {
        self.menu
            .selected_item(&self.items)
            .map(|item| item.value.as_str())
    }
}

pub fn complete_file_mention(input: &mut String, path: &str) {
    let Some((start, _)) = mention_range(input) else {
        return;
    };
    input.replace_range(start.., &format!("@{path} "));
}

pub fn expand_file_mentions(input: &str) -> anyhow::Result<String> {
    let paths = mentioned_paths(input);
    if paths.is_empty() {
        return Ok(input.to_string());
    }

    let references = paths
        .iter()
        .map(|path| format!("<file path=\"{path}\" />"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "{input}\n\nThe user referenced these files. Read them with the read tool if needed:\n<file-references>\n{references}\n</file-references>"
    ))
}

fn mention_query(input: &str) -> Option<&str> {
    mention_range(input).map(|(_, query)| query)
}

fn mention_range(input: &str) -> Option<(usize, &str)> {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(index, c)| c.is_whitespace().then_some(index + c.len_utf8()))
        .unwrap_or(0);
    let token = &input[start..];
    token.strip_prefix('@').map(|query| (start, query))
}

fn discover_files() -> Vec<String> {
    WalkBuilder::new(".")
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| relative_path(entry.path()))
        .filter(|path| !is_ignored_path(path))
        .collect()
}

fn relative_path(path: &Path) -> Option<String> {
    let path = path.strip_prefix(".").unwrap_or(path);
    Some(path.to_string_lossy().trim_start_matches('/').to_string())
}

fn match_files(files: &[String], query: &str) -> Vec<SelectItem> {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = pattern.match_list(files.iter().map(String::as_str), &mut matcher);
    matches.truncate(MAX_FILE_MENTIONS);
    matches
        .into_iter()
        .map(|(path, _)| SelectItem::with_label(path, file_name(path), path))
        .collect()
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn mentioned_paths(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
        .filter(|path| !path.is_empty() && !is_ignored_path(path))
        .map(ToString::to_string)
        .collect()
}

fn is_ignored_path(path: &str) -> bool {
    path.split('/').any(|part| part == ".git") || is_env_path(path)
}

fn is_env_path(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".env" || name.starts_with(".env."))
}
