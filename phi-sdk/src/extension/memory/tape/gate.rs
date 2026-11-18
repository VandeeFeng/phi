use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::agent::tools::ToolError;
use crate::extension::memory::tape::anchor::{TapeAnchor, TapeAnchorTrigger};
use crate::extension::memory::tape::types::{TapeAnchorMode, TapeConfig, TapeKeywordConfig};
use crate::extension::memory::types::MemorySettings;
use crate::extension::memory::utils::format_time_suffix;
use crate::permission::runtime::PermissionRuntime;
use crate::utils::workspace::WorkspaceMeta;

const MIN_KEYWORD_PROMPT_LENGTH: usize = 10;
const MAX_KEYWORD_PROMPT_LENGTH: usize = 300;
const KEYWORD_CONTEXT_UNITS_PER_SIDE: usize = 2;

#[derive(Debug, Clone, Default)]
pub struct TapeGate {
    pub enabled: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapeGateReason {
    Disabled,
    ExcludedDir,
    MissingGit,
    Enabled,
}

#[derive(Debug, Clone)]
pub struct TapeGateResult {
    pub enabled: bool,
    pub reason: TapeGateReason,
    pub workspace: Option<WorkspaceMeta>,
    pub matched_exclude_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct KeywordAnchorInstruction {
    pub primary: String,
    pub matched: Vec<String>,
    pub anchor_name: String,
    pub message: String,
}

pub(crate) struct KeywordAnchorCandidate {
    pub(crate) instruction: KeywordAnchorInstruction,
    signatures: HashSet<KeywordContextSignature>,
}

impl KeywordAnchorCandidate {
    pub(crate) fn accept(self, state: &mut TapeMutationState) {
        state.seen_keyword_contexts.extend(self.signatures);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KeywordContextSignature {
    keyword: String,
    left: Vec<String>,
    right: Vec<String>,
}

pub struct TapeMutationGate {
    settings: Arc<MemorySettings>,
    state: Arc<Mutex<TapeMutationState>>,
    permission: Option<PermissionRuntime>,
}

#[derive(Debug, Clone, Default)]
pub struct TapeMutationState {
    pub pending_keyword_anchor: Option<KeywordAnchorInstruction>,
    pub(crate) pending_mutation: Option<TapeAnchor>,
    pub(crate) seen_keyword_contexts: HashSet<KeywordContextSignature>,
}

impl TapeMutationGate {
    pub fn new(
        settings: Arc<MemorySettings>,
        state: Arc<Mutex<TapeMutationState>>,
        permission: Option<PermissionRuntime>,
    ) -> Self {
        Self {
            settings,
            state,
            permission,
        }
    }

    pub async fn authorize_anchor(&self, name: &str) -> Result<ResolvedAnchorTrigger, ToolError> {
        let (resolved, consent) = {
            let state = self
                .state
                .lock()
                .map_err(|_| ToolError::Msg("tape anchor gate is unavailable".to_string()))?;
            (
                resolve_tape_anchor_trigger(&self.settings, &state, name),
                state
                    .pending_keyword_anchor
                    .as_ref()
                    .map(|instruction| instruction.anchor_name.clone()),
            )
        };
        self.permission
            .as_ref()
            .ok_or_else(|| {
                ToolError::Msg("Tape operations require an explicit permission policy".into())
            })?
            .authorize_tool_with_exact_consent("tape_anchor", name, consent.as_deref())
            .await
            .map_err(|error| ToolError::Msg(error.to_string()))?;
        Ok(resolved)
    }

    pub fn stage_anchor(&self, anchor: TapeAnchor) -> Result<(), ToolError> {
        self.state
            .lock()
            .map_err(|_| ToolError::Msg("tape anchor gate is unavailable".to_string()))?
            .stage_anchor(anchor)
            .map_err(ToolError::Msg)
    }

    pub(crate) async fn authorize_delete(&self, ids: &[String]) -> Result<(), ToolError> {
        let mut ids = ids.to_vec();
        ids.sort();
        self.require_permission("tape_delete", &format!("delete {}", ids.join(" ")))
            .await
    }

    pub(crate) async fn require_permission(
        &self,
        tool: &str,
        input: &str,
    ) -> Result<(), ToolError> {
        self.permission
            .as_ref()
            .ok_or_else(|| {
                ToolError::Msg("Tape operations require an explicit permission policy".into())
            })?
            .authorize_tool(tool, input, None, None)
            .await
            .map_err(|error| ToolError::Msg(error.to_string()))
    }
}

impl TapeMutationState {
    pub(crate) fn stage_anchor(&mut self, anchor: TapeAnchor) -> Result<(), String> {
        if self.pending_mutation.is_some() {
            return Err("tape mutation is already pending".to_string());
        }
        self.pending_mutation = Some(anchor);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedAnchorTrigger {
    pub trigger: crate::extension::memory::tape::anchor::TapeAnchorTrigger,
    pub keywords: Vec<String>,
}

impl TapeGate {
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            reason: None,
        }
    }

    pub fn disabled(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
        }
    }
}

pub fn set_keyword_anchor(state: &mut TapeMutationState, instruction: KeywordAnchorInstruction) {
    state.pending_keyword_anchor = Some(instruction);
}

pub fn clear_keyword_anchor(state: &mut TapeMutationState) {
    state.pending_keyword_anchor = None;
}

pub fn consume_keyword_anchor(state: &mut TapeMutationState, trigger: TapeAnchorTrigger) {
    if trigger == TapeAnchorTrigger::Keyword {
        clear_keyword_anchor(state);
    }
}

pub fn resolve_tape_anchor_trigger(
    settings: &MemorySettings,
    state: &TapeMutationState,
    name: &str,
) -> ResolvedAnchorTrigger {
    let anchor_mode = settings
        .tape
        .as_ref()
        .and_then(|tape| tape.anchor.as_ref())
        .and_then(|anchor| anchor.mode)
        .unwrap_or(TapeAnchorMode::Auto);

    match state.pending_keyword_anchor.as_ref() {
        Some(instruction) if instruction.anchor_name == name => {
            return ResolvedAnchorTrigger {
                trigger: TapeAnchorTrigger::Keyword,
                keywords: instruction.matched.clone(),
            };
        }
        _ => {}
    }

    ResolvedAnchorTrigger {
        trigger: if anchor_mode == TapeAnchorMode::Manual {
            TapeAnchorTrigger::Manual
        } else {
            TapeAnchorTrigger::Direct
        },
        keywords: Vec::new(),
    }
}

pub fn resolve_tape_gate(cwd: &str, tape: Option<&TapeConfig>) -> anyhow::Result<TapeGateResult> {
    let absolute_cwd = absolutize(Path::new(cwd));
    let Some(tape) = tape.filter(|tape| tape.enabled != Some(false)) else {
        return Ok(TapeGateResult {
            enabled: false,
            reason: TapeGateReason::Disabled,
            workspace: None,
            matched_exclude_dir: None,
        });
    };

    let canonical_cwd = absolute_cwd.canonicalize().ok();
    for excluded_dir in &tape.exclude_dirs {
        let is_excluded = match (
            canonical_cwd.as_ref(),
            Path::new(excluded_dir).canonicalize(),
        ) {
            (Some(cwd), Ok(excluded)) => cwd == &excluded || cwd.starts_with(excluded),
            _ => false,
        };
        if is_excluded {
            return Ok(TapeGateResult {
                enabled: false,
                reason: TapeGateReason::ExcludedDir,
                workspace: None,
                matched_exclude_dir: Some(
                    absolutize(Path::new(excluded_dir))
                        .to_string_lossy()
                        .to_string(),
                ),
            });
        }
    }

    let workspace = WorkspaceMeta::discover(&absolute_cwd, None)?;
    if tape.only_git.unwrap_or(true) && workspace.git_dir().is_none() {
        return Ok(TapeGateResult {
            enabled: false,
            reason: TapeGateReason::MissingGit,
            workspace: None,
            matched_exclude_dir: None,
        });
    }

    Ok(TapeGateResult {
        enabled: true,
        reason: TapeGateReason::Enabled,
        workspace: Some(workspace),
        matched_exclude_dir: None,
    })
}

pub fn normalize_tape_keywords(config: Option<&TapeKeywordConfig>) -> TapeKeywordConfig {
    TapeKeywordConfig {
        global: normalize_keyword_list(config.map(|config| config.global.as_slice())),
        project: normalize_keyword_list(config.map(|config| config.project.as_slice())),
    }
}

pub fn detect_keyword_anchor(
    prompt: &str,
    config: Option<&TapeKeywordConfig>,
) -> Option<KeywordAnchorInstruction> {
    let normalized_prompt = prompt.trim();
    if normalized_prompt.len() < MIN_KEYWORD_PROMPT_LENGTH
        || normalized_prompt.len() > MAX_KEYWORD_PROMPT_LENGTH
    {
        return None;
    }

    let normalized_prompt_lowercase = normalized_prompt.to_lowercase();
    let keywords = config
        .map(|config| [config.global.as_slice(), config.project.as_slice()].concat())
        .unwrap_or_default();
    let mut matched = normalize_keyword_list(Some(&keywords))
        .into_iter()
        .filter(|keyword| matches_keyword(&normalized_prompt_lowercase, keyword))
        .collect::<Vec<_>>();
    matched.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    matched.dedup();

    let primary = matched.first()?.clone();
    let anchor_name = format!(
        "anchor/keyword-{}-{}",
        slugify_keyword(&primary),
        format_time_suffix()
    );
    let message = build_keyword_anchor_message_text(&primary, &anchor_name);
    Some(KeywordAnchorInstruction {
        primary,
        matched,
        anchor_name,
        message,
    })
}

pub(crate) fn detect_unseen_keyword_context(
    state: &TapeMutationState,
    prompt: &str,
    config: Option<&TapeKeywordConfig>,
) -> Option<KeywordAnchorCandidate> {
    let instruction = detect_keyword_anchor(prompt, config)?;
    let signatures = keyword_context_signatures(prompt, &instruction.matched);
    if signatures
        .iter()
        .all(|signature| state.seen_keyword_contexts.contains(signature))
    {
        return None;
    }
    Some(KeywordAnchorCandidate {
        instruction,
        signatures,
    })
}

pub fn build_keyword_anchor_message(
    prompt: &str,
    config: Option<&TapeKeywordConfig>,
) -> Option<String> {
    detect_keyword_anchor(prompt, config).map(|instruction| instruction.message)
}

fn build_keyword_anchor_message_text(primary: &str, anchor_name: &str) -> String {
    [
        format!("Keyword detected: {primary}."),
        String::new(),
        "Before continuing, call tape_anchor with:".to_string(),
        format!("- name: \"{anchor_name}\""),
        "- summary: \"<brief intent summary of the user's current prompt in the user's language>\""
            .to_string(),
        "- purpose: \"<1-2 word label for the anchor's purpose>\"".to_string(),
        String::new(),
        "Constraints:".to_string(),
        "- Make the summary specific to the actual task.".to_string(),
        "- Do not use a generic keyword-only summary.".to_string(),
        "- Keep the summary under 18 words.".to_string(),
        String::new(),
        "Then continue the user's task normally.".to_string(),
    ]
    .join("\n")
}

fn normalize_keyword_list(keywords: Option<&[String]>) -> Vec<String> {
    let mut keywords = keywords
        .unwrap_or_default()
        .iter()
        .map(|keyword| keyword.trim().to_lowercase())
        .filter(|keyword| !keyword.is_empty())
        .collect::<Vec<_>>();
    keywords.sort();
    keywords.dedup();
    keywords
}

fn matches_keyword(prompt: &str, keyword: &str) -> bool {
    keyword_occurrences(prompt, keyword).next().is_some()
}

fn keyword_context_signatures(
    prompt: &str,
    keywords: &[String],
) -> HashSet<KeywordContextSignature> {
    let normalized_prompt = prompt.to_lowercase();
    let mut signatures = HashSet::new();
    for keyword in keywords {
        for (start, end) in keyword_occurrences(&normalized_prompt, keyword) {
            signatures.insert(keyword_context_signature(
                &normalized_prompt,
                keyword,
                start,
                end,
            ));
        }
    }
    signatures
}

fn keyword_occurrences<'a>(
    prompt: &'a str,
    keyword: &'a str,
) -> impl Iterator<Item = (usize, usize)> + 'a {
    prompt.match_indices(keyword).filter_map(|(start, value)| {
        let end = start + value.len();
        keyword_boundaries_match(prompt, keyword, start, end).then_some((start, end))
    })
}

fn keyword_boundaries_match(prompt: &str, keyword: &str, start: usize, end: usize) -> bool {
    keyword_edge_is_bounded(prompt[..start].chars().next_back(), keyword.chars().next())
        && keyword_edge_is_bounded(prompt[end..].chars().next(), keyword.chars().next_back())
}

fn keyword_edge_is_bounded(neighbor: Option<char>, keyword_edge: Option<char>) -> bool {
    neighbor.is_none_or(|neighbor| {
        if keyword_edge.is_some_and(|edge| edge.is_ascii_alphanumeric() || edge == '_') {
            !neighbor.is_ascii_alphanumeric() && neighbor != '_'
        } else {
            !neighbor.is_alphanumeric() && neighbor != '_'
        }
    })
}

fn keyword_context_signature(
    prompt: &str,
    keyword: &str,
    start: usize,
    end: usize,
) -> KeywordContextSignature {
    let mut left = context_units(&prompt[..start])
        .into_iter()
        .rev()
        .take(KEYWORD_CONTEXT_UNITS_PER_SIDE)
        .collect::<Vec<_>>();
    left.reverse();
    let right = context_units(&prompt[end..])
        .into_iter()
        .take(KEYWORD_CONTEXT_UNITS_PER_SIDE)
        .collect();
    KeywordContextSignature {
        keyword: keyword.to_string(),
        left,
        right,
    }
}

fn context_units(value: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut word = String::new();
    for character in value.chars() {
        if character.is_whitespace() {
            push_context_word(&mut units, &mut word);
        } else if is_han(character) {
            push_context_word(&mut units, &mut word);
            units.push(character.to_string());
        } else if character.is_alphanumeric() || character == '_' {
            word.extend(character.to_lowercase());
        } else {
            push_context_word(&mut units, &mut word);
            units.push(character.to_string());
        }
    }
    push_context_word(&mut units, &mut word);
    units
}

fn push_context_word(units: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        units.push(std::mem::take(word));
    }
}

fn is_han(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2fa1f}'
            | '\u{30000}'..='\u{323af}'
    )
}

fn slugify_keyword(value: &str) -> String {
    let slug = value
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(48)
        .collect::<String>();
    if slug.is_empty() {
        "detected".to_string()
    } else {
        slug
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::tape::anchor::TapeAnchorType;

    fn manual_settings() -> MemorySettings {
        serde_json::from_value(serde_json::json!({
            "enabled": true,
            "tape": { "anchor": { "mode": "manual" } }
        }))
        .unwrap()
    }

    fn keyword_config(keyword: &str) -> TapeKeywordConfig {
        TapeKeywordConfig {
            global: vec![keyword.to_string()],
            project: Vec::new(),
        }
    }

    #[test]
    fn keyword_context_uses_han_characters_words_and_punctuation() {
        let chinese = keyword_context_signatures("请看看 tape.md 文件", &["tape".to_string()])
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(chinese.left, ["看", "看"]);
        assert_eq!(chinese.right, [".", "md"]);

        let english =
            keyword_context_signatures("please look at tape.md now", &["tape".to_string()])
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(english.left, ["look", "at"]);
        assert_eq!(english.right, [".", "md"]);
    }

    #[test]
    fn keyword_detection_accepts_non_ascii_boundaries() {
        let config = keyword_config("tape");

        assert!(detect_keyword_anchor("这个项目的tape是干嘛的", Some(&config)).is_some());
        assert!(detect_keyword_anchor("这个项目的 tape 是干嘛的", Some(&config)).is_some());
        assert!(detect_keyword_anchor("这个项目的TaPe是干嘛的", Some(&config)).is_some());
        assert!(detect_keyword_anchor("这个项目的tapes是干嘛的", Some(&config)).is_none());
    }

    #[test]
    fn non_ascii_keywords_keep_unicode_word_boundaries() {
        let config = keyword_config("记忆");

        assert!(detect_keyword_anchor("这个项目的 记忆 是干嘛的", Some(&config)).is_some());
        assert!(detect_keyword_anchor("这个项目的记忆功能是干嘛的", Some(&config)).is_none());
    }

    fn assert_keyword_detections(cases: &[(&str, bool)]) {
        let config = keyword_config("tape");
        let mut state = TapeMutationState::default();
        for (prompt, expected) in cases {
            let candidate = detect_unseen_keyword_context(&state, prompt, Some(&config));
            assert_eq!(
                candidate.is_some(),
                *expected,
                "unexpected detection result for {prompt:?}"
            );
            if let Some(candidate) = candidate {
                candidate.accept(&mut state);
            }
        }
    }

    #[test]
    fn keyword_context_deduplicates_within_session() {
        assert_keyword_detections(&[
            ("请看看 tape.md 文件", true),
            ("麻烦再看看 tape.md 内容", false),
            ("请看看 tape.test 文件", true),
            ("what is tape.md used for", true),
        ]);
        assert_keyword_detections(&[("看 看 tape . md 文件", true), ("看看 tape.md 文件", false)]);
        assert_keyword_detections(&[("看看 tape.md 文件", true)]);
        assert_keyword_detections(&[("看看 TAPE.md 文件", true), ("看看 TaPe.md 文件", false)]);
        assert_keyword_detections(&[
            ("看看 tape.md 再检查 tape.test", true),
            ("看看 tape.md 文件", false),
            ("稍后再检查 tape.test", false),
            ("重新运行 tape.test 文件", true),
        ]);
    }

    #[test]
    fn staging_rejects_anchor_overwrite() {
        let mut state = TapeMutationState::default();
        state
            .stage_anchor(TapeAnchor::new("first", TapeAnchorType::Normal))
            .unwrap();
        assert!(
            state
                .stage_anchor(TapeAnchor::new("second", TapeAnchorType::Normal))
                .is_err()
        );

        assert!(matches!(
            state.pending_mutation,
            Some(ref anchor) if anchor.name == "first"
        ));
    }
}
