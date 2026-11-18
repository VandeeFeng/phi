use std::fmt::Write;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use compact_str::CompactString;
use futures::future::BoxFuture;
use rig::completion::{AssistantContent, CompletionRequest, Message};
use rig::message::{
    DocumentSourceKind, Image, ImageMediaType, MimeType, Reasoning, ReasoningContent, ToolCall,
    ToolCallId, ToolFunction, ToolResultContent, UserContent,
};
use smallvec::SmallVec;

use crate::agent::custom::AgentDefinition;
use crate::agent::options::{
    AVAILABLE_TOOLS_PLACEHOLDER, SYSTEM_PROMPT, TOOL_GUIDELINES_PLACEHOLDER,
};
use crate::config::{AgentMode, RuntimeConfig};
use crate::core::lifecycle::{
    BuildSystemPromptEvent, PromptContextEvent, UserPromptDispatch, UserPromptExpansionEvent,
    UserPromptExpansionOutcome, UserPromptSubmitAction, UserPromptSubmitEvent,
};
use crate::core::turn::{
    CoreTurnPreparation, ImageInput, PreparedTurnPrompt, TurnPrompt, TurnToolPolicy,
};
use crate::hooks::{
    HookError, HookRegistry, emit_build_system_prompt, emit_post_compact, emit_pre_compact,
    emit_prompt_expansion, emit_user_prompt_submit,
};
use crate::provider::AnyClient;
use crate::transcript::{
    AgentMessage, ContentBlock, ContextSource, ImageSource, ReasoningMetadata,
    ReasoningMetadataContent, Transcript, TranscriptEntry, TranscriptEntryKind, UserContentBlock,
};

#[derive(Clone)]
pub struct ContextFiles {
    pub(crate) agents: Option<String>,
    pub(crate) agent_catalog: crate::agent::custom::AgentCatalog,
    agent_diagnostics: Vec<crate::agent::custom::AgentDiagnostic>,
}

impl ContextFiles {
    pub fn append_agent_instructions(&mut self, instructions: impl Into<String>) {
        let instructions = instructions.into();
        self.agents = Some(match self.agents.take() {
            Some(agents) => format!("{agents}\n\n{instructions}"),
            None => instructions,
        });
    }

    pub fn validate_agent_tool_declarations(
        &mut self,
        validate: &dyn Fn(&crate::tools::ToolRule) -> Result<(), crate::tools::ToolRuleError>,
    ) -> Result<(), crate::tools::ToolRuleError> {
        self.agent_diagnostics
            .extend(self.agent_catalog.validate_tool_declarations(validate)?);
        Ok(())
    }

    pub fn agent_diagnostics(&self) -> &[crate::agent::custom::AgentDiagnostic] {
        &self.agent_diagnostics
    }

    pub fn reload(&mut self, no_context_files: bool) {
        *self = load(no_context_files);
    }

    pub fn reload_with_options(
        &mut self,
        no_context_files: bool,
        global_agents_path: Option<PathBuf>,
        custom_agent_roots: Vec<PathBuf>,
    ) {
        *self = load_with_options(no_context_files, global_agents_path, custom_agent_roots);
    }
}

pub fn load(no_context_files: bool) -> ContextFiles {
    load_with_options(no_context_files, None, Vec::new())
}

pub fn load_with_options(
    no_context_files: bool,
    global_agents_path: Option<PathBuf>,
    custom_agent_roots: Vec<PathBuf>,
) -> ContextFiles {
    load_with_working_dir(
        no_context_files,
        global_agents_path,
        custom_agent_roots,
        std::env::current_dir().ok(),
    )
}

pub fn load_with_options_for(
    no_context_files: bool,
    global_agents_path: Option<PathBuf>,
    custom_agent_roots: Vec<PathBuf>,
    working_dir: &std::path::Path,
) -> ContextFiles {
    load_with_working_dir(
        no_context_files,
        global_agents_path,
        custom_agent_roots,
        Some(working_dir.to_path_buf()),
    )
}

fn load_with_working_dir(
    no_context_files: bool,
    global_agents_path: Option<PathBuf>,
    custom_agent_roots: Vec<PathBuf>,
    working_dir: Option<PathBuf>,
) -> ContextFiles {
    let agents = if no_context_files {
        None
    } else {
        load_agents_from(global_agents_path, working_dir)
    };
    let loaded_agents = crate::agent::custom::load_from_roots(custom_agent_roots);
    ContextFiles {
        agents,
        agent_catalog: loaded_agents.catalog,
        agent_diagnostics: loaded_agents.diagnostics,
    }
}

fn load_file(path: &PathBuf) -> Option<String> {
    if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    }
}

fn load_agents_from(
    global_agents_path: Option<PathBuf>,
    working_dir: Option<PathBuf>,
) -> Option<String> {
    let mut parts: SmallVec<[String; 2]> = SmallVec::new();
    if let Some(global) = global_agents_path
        && let Some(content) = load_file(&global)
        && !content.trim().is_empty()
    {
        parts.push(format!("# Global AGENTS.md\n{content}"));
    }
    if let Some(cwd) = working_dir {
        let workspace = crate::utils::workspace::WorkspaceMeta::discover(&cwd, None).ok();
        if let Some((name, content)) =
            workspace.and_then(|workspace| project_instruction(&workspace.root))
        {
            parts.push(format!("# Project {name}\n{content}"));
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn project_instruction(root: &std::path::Path) -> Option<(&'static str, String)> {
    ["AGENTS.md", "CLAUDE.md"].into_iter().find_map(|name| {
        load_file(&root.join(name))
            .filter(|content| !content.trim().is_empty())
            .map(|content| (name, content))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InjectedContext {
    pub source: ContextSource,
    pub content: String,
}

pub fn collect_injected_context(
    injected_context: Vec<InjectedContext>,
    lifecycle_context: Vec<InjectedContext>,
) -> Vec<InjectedContext> {
    injected_context
        .into_iter()
        .chain(lifecycle_context)
        .filter(|context| !context.content.is_empty())
        .collect()
}

pub fn render_prompt_with_context(prompt: &str, contexts: &[InjectedContext]) -> String {
    let context = contexts
        .iter()
        .map(render_injected_context)
        .collect::<Vec<_>>()
        .join("\n\n");
    if context.is_empty() {
        return prompt.to_string();
    }
    format!("{context}\n\n<user_prompt>\n{prompt}\n</user_prompt>")
}

pub fn append_dynamic_context(mut history: Vec<Message>, context: Vec<String>) -> Vec<Message> {
    if !context.is_empty() {
        history.push(Message::system(format!(
            "[Dynamic context]\n{}",
            context.join("\n")
        )));
    }
    history
}

pub fn render_context(source: ContextSource, content: &str) -> String {
    format!(
        "<context source=\"{}\">\n{}\n</context>",
        source.as_str(),
        content
    )
}

fn render_injected_context(context: &InjectedContext) -> String {
    render_context(context.source.clone(), &context.content)
}

pub(crate) fn build_system_prompt(
    context: &ContextFiles,
    mode: AgentMode,
    mode_prompt_override: Option<&str>,
    available_tools: &str,
    tool_guidelines: &str,
    working_dir: &std::path::Path,
) -> String {
    let mut preamble = SYSTEM_PROMPT
        .replace(AVAILABLE_TOOLS_PLACEHOLDER, available_tools)
        .replace(TOOL_GUIDELINES_PLACEHOLDER, tool_guidelines);
    append_mode_prompt(&mut preamble, mode, mode_prompt_override);
    if let Some(agents) = &context.agents {
        preamble.push_str("\n\n");
        preamble.push_str(agents);
    }

    append_runtime_environment(&mut preamble, working_dir);

    preamble
}

pub(crate) async fn finalize_system_prompt(
    hooks: &HookRegistry,
    event: BuildSystemPromptEvent,
) -> String {
    let original_prompt = event.system_prompt.clone();
    emit_build_system_prompt(hooks, event)
        .await
        .system_prompt
        .unwrap_or(original_prompt)
}

pub fn build_subagent_system_prompt(
    definition: &AgentDefinition,
    mode: AgentMode,
    mode_prompt_override: Option<&str>,
    working_dir: &std::path::Path,
) -> String {
    let mut prompt = definition.prompt().to_string();
    append_mode_prompt(&mut prompt, mode, mode_prompt_override);
    append_runtime_environment(&mut prompt, working_dir);
    prompt
}

fn append_mode_prompt(prompt: &mut String, mode: AgentMode, prompt_override: Option<&str>) {
    if let Some(mode_prompt) = mode.prompt(prompt_override) {
        prompt.push_str("\n\n");
        prompt.push_str(mode_prompt);
    }
}

pub fn replace_prompt_section(prompt: &mut String, start_tag: &str, end_tag: &str, content: &str) {
    let Some(start) = prompt.find(start_tag) else {
        return;
    };
    let content_start = start + start_tag.len();
    let Some(relative_end) = prompt[content_start..].find(end_tag) else {
        return;
    };
    let content_end = content_start + relative_end;
    prompt.replace_range(content_start..content_end, &format!("\n{content}\n"));
}

fn append_runtime_environment(prompt: &mut String, working_dir: &std::path::Path) {
    let date = chrono::Local::now().date_naive();
    prompt.push_str(&format!("\nCurrent date: {date}"));
    prompt.push_str(&format!(
        "\nCurrent working directory: {}",
        working_dir.display()
    ));
}

/// Application-owned command preparation, separate from hook interception.
pub trait PromptExpander: Send + Sync {
    /// Recognizes a command without executing expansion or reading its body.
    fn describe(&self, event: &UserPromptSubmitEvent) -> Option<UserPromptExpansionEvent>;

    fn expand(
        self: Arc<Self>,
        event: UserPromptExpansionEvent,
    ) -> BoxFuture<'static, Result<PromptExpansion, String>>;
}

#[derive(Debug, Clone, Default)]
pub struct PromptExpansion {
    pub prompt: Option<String>,
    pub tool_policy: TurnToolPolicy,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PromptPreparationError {
    #[error("prompt hook failed: {0}")]
    Hook(#[from] HookError),
    #[error("transcript context failed: {0}")]
    Transcript(#[from] anyhow::Error),
}

#[derive(Clone)]
pub(crate) struct PromptPreparation {
    pub(crate) hooks: HookRegistry,
    pub(crate) expander: Option<Arc<dyn crate::context::PromptExpander>>,
    pub(crate) transcript: Arc<Transcript>,
    pub(crate) working_dir: PathBuf,
    pub(crate) delegated_child: bool,
}

impl PromptPreparation {
    pub(crate) async fn prepare(
        self,
        turn_prompt: TurnPrompt,
        dispatch: UserPromptDispatch,
    ) -> Result<CoreTurnPreparation, PromptPreparationError> {
        self.hooks.ready().await;
        let original_prompt = turn_prompt.prompt.clone();
        let event = UserPromptSubmitEvent {
            original_prompt: original_prompt.clone(),
            prompt: turn_prompt.prompt,
            source: turn_prompt.source,
            dispatch,
            delegated_child: self.delegated_child,
        };
        let (mut prompt, mut hook_context, contribution) =
            match emit_user_prompt_submit(&self.hooks, event).await {
                UserPromptSubmitAction::Continue {
                    prompt,
                    injected_context,
                    tool_policy,
                } => (prompt, injected_context, tool_policy),
                UserPromptSubmitAction::Handled { reason } => {
                    return Ok(CoreTurnPreparation::Handled { reason });
                }
            };
        let mut tool_policy = turn_prompt.tool_policy;
        tool_policy.merge(&contribution);
        if let Some(expander) = self.expander {
            let submission = UserPromptSubmitEvent {
                original_prompt: original_prompt.clone(),
                prompt: prompt.clone(),
                source: turn_prompt.source,
                dispatch,
                delegated_child: self.delegated_child,
            };
            if let Some(event) = expander.describe(&submission) {
                match emit_prompt_expansion(&self.hooks, &event).await? {
                    UserPromptExpansionOutcome::Block { reason } => {
                        return Ok(CoreTurnPreparation::Handled {
                            reason: Some(reason),
                        });
                    }
                    UserPromptExpansionOutcome::AddContext { contexts } => {
                        hook_context.extend(contexts)
                    }
                    UserPromptExpansionOutcome::Allow => {}
                }
                let expansion = match expander.expand(event).await {
                    Ok(expansion) => expansion,
                    Err(reason) => {
                        return Ok(CoreTurnPreparation::Handled {
                            reason: Some(reason),
                        });
                    }
                };
                if let Some(expanded) = expansion.prompt {
                    prompt = expanded;
                }
                tool_policy.merge(&expansion.tool_policy);
            }
        }
        let mut contexts = collect_injected_context(turn_prompt.injected_context, hook_context);
        let user_content = canonical_user_content(&prompt, turn_prompt.images);
        self.transcript.effective_context()?;
        let context_outcomes = self
            .hooks
            .prepare_context(PromptContextEvent {
                original_prompt,
                prompt: prompt.clone(),
                source: turn_prompt.source,
                dispatch,
                transcript: self.transcript,
                working_dir: self.working_dir,
                delegated_child: self.delegated_child,
            })
            .await?;
        contexts.extend(context_outcomes.contexts);
        Ok(CoreTurnPreparation::Ready(PreparedTurnPrompt {
            submission_id: None,
            prompt,
            source: turn_prompt.source,
            tool_policy,
            contexts,
            user_content,
            hook_notices: context_outcomes.notices,
        }))
    }
}

impl PreparedTurnPrompt {
    pub(crate) fn commit(&self, transcript: &mut Transcript) {
        transcript.append_user_content(self.user_content.clone());
        self.commit_contexts(transcript);
    }

    pub(crate) fn commit_contexts(&self, transcript: &mut Transcript) {
        for context in &self.contexts {
            transcript.append_context_message(
                context.source.clone(),
                crate::transcript::ContextScope::Turn,
                &context.content,
            );
        }
    }
}

fn canonical_user_content(prompt: &str, images: Vec<ImageInput>) -> Vec<UserContentBlock> {
    std::iter::once(UserContentBlock::Text {
        text: CompactString::new(prompt).into(),
    })
    .chain(images.into_iter().map(|image| UserContentBlock::Image {
        source: match image {
            ImageInput::Url { url, mime_type } => ImageSource::Url {
                url: url.into(),
                mime_type: mime_type.map(Into::into),
            },
            ImageInput::Base64 { data, mime_type } => ImageSource::Base64 {
                data: data.into(),
                mime_type: mime_type.into(),
            },
        },
    }))
    .collect()
}

#[derive(Clone)]
pub(crate) struct HistoryTarget {
    pub provider: String,
    pub api: Option<crate::provider::ProviderApi>,
    pub model: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtocolReplay {
    Preserve,
    Strip,
}

pub fn convert_history(session: &Transcript) -> anyhow::Result<Vec<Message>> {
    convert_history_for_target(session, None)
}

pub(crate) fn convert_history_for_target(
    session: &Transcript,
    target: Option<&HistoryTarget>,
) -> anyhow::Result<Vec<Message>> {
    let context = session.effective_context()?;
    let mut messages = Vec::new();

    if let Some(marker) = context.marker {
        match &marker.kind {
            TranscriptEntryKind::Compaction { summary, .. } => messages.push(Message::system(
                format!("[Previous conversation summary]\n{}", summary),
            )),
            TranscriptEntryKind::Reset {
                handoff: Some(handoff),
            } => messages.push(Message::user(handoff.to_string())),
            _ => {}
        }
    }

    let projected_entries = context.projected_entries();
    let projected = projected_entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry.as_ref()))
        .collect::<std::collections::HashMap<_, _>>();
    let branch = context.entries;
    let mut call_replay = std::collections::HashMap::new();
    for (index, source) in branch.iter().enumerate() {
        let Some(entry) = projected.get(source.id.as_str()) else {
            continue;
        };
        match &entry.kind {
            TranscriptEntryKind::Message { message } => match message {
                AgentMessage::User { content } => {
                    if let Some(message) =
                        render_user_with_context(content, &branch, &projected, index)
                    {
                        messages.push(message);
                    }
                }
                AgentMessage::Agent { content } => {
                    if let Some(text) = text_from_blocks(content) {
                        let content = [UserContentBlock::Text { text: text.into() }];
                        if let Some(message) =
                            render_user_with_context(&content, &branch, &projected, index)
                        {
                            messages.push(message);
                        }
                    }
                }
                AgentMessage::Context { .. } => {}
                AgentMessage::Assistant {
                    provider,
                    api,
                    model,
                    content,
                    stop_reason,
                    ..
                } => {
                    let same_model = target.is_none_or(|target| {
                        target.provider == provider.as_str()
                            && target.api.is_some()
                            && target.api == *api
                            && target.model == model.as_str()
                            // Interrupted content has no complete protocol replay state.
                            && !matches!(stop_reason.as_str(), "error" | "aborted")
                    });
                    let replay = if same_model {
                        ProtocolReplay::Preserve
                    } else {
                        ProtocolReplay::Strip
                    };
                    call_replay.extend(content.iter().filter_map(|block| match block {
                        ContentBlock::ToolCall { id, .. } => Some((id.to_string(), replay)),
                        _ => None,
                    }));
                    if let Some(message) = convert_agent_message(message, replay) {
                        messages.push(message);
                    }
                }
                AgentMessage::ToolResult { tool_call_id, .. } => {
                    if let Some(message) = convert_agent_message(
                        message,
                        call_replay
                            .get(tool_call_id.as_str())
                            .copied()
                            .unwrap_or(ProtocolReplay::Preserve),
                    ) {
                        messages.push(message);
                    }
                }
                _ => {
                    if let Some(message) = convert_agent_message(message, ProtocolReplay::Preserve)
                    {
                        messages.push(message);
                    }
                }
            },
            _ => {}
        }
    }

    Ok(if let Some(target) = target {
        project_tool_exchanges(messages, target)
    } else {
        messages
    })
}

fn project_tool_exchanges(messages: Vec<Message>, target: &HistoryTarget) -> Vec<Message> {
    let mut reserved_ids = messages
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { content, .. } => content.as_slice(),
            _ => &[],
        })
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.wire_call_id().to_owned()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut projected_ids = std::collections::HashMap::<ToolCallId, ToolCallId>::new();
    let mut completed = Vec::with_capacity(messages.len());
    let mut pending = Vec::<ToolCall>::new();
    let mut held_system = Vec::new();
    for mut message in messages {
        if let Message::User { content } = &mut message {
            for block in content {
                if let UserContent::ToolResult(result) = block
                    && result.provider.is_none()
                    && let Some(id) = projected_ids.get(&result.call)
                {
                    result.call = id.clone();
                }
            }
        }
        match &mut message {
            Message::System { .. } if !pending.is_empty() || !held_system.is_empty() => {
                held_system.push(message);
                continue;
            }
            Message::User { content }
                if content
                    .iter()
                    .all(|block| matches!(block, UserContent::ToolResult(_))) =>
            {
                content.retain(|block| {
                    let UserContent::ToolResult(result) = block else {
                        unreachable!();
                    };
                    let Some(index) = pending
                        .iter()
                        .position(|call| call.wire_call_id() == result.wire_call_id())
                    else {
                        return false;
                    };
                    pending.remove(index);
                    true
                });
                if content.is_empty() {
                    continue;
                }
            }
            _ => finish_tool_exchange(&mut completed, &mut pending, &mut held_system),
        }
        if let Message::Assistant { content, .. } = &mut message {
            for block in content {
                if let AssistantContent::ToolCall(call) = block {
                    let original_id = call.id.clone();
                    if call.provider.is_none() {
                        let mut id = crate::provider::generation::normalize_tool_call_id(
                            original_id.as_str(),
                            &target.provider,
                            target.api,
                        );
                        let mut collision = 0;
                        while id != original_id.as_str() && reserved_ids.contains(&id) {
                            collision += 1;
                            id = crate::provider::generation::normalize_tool_call_id(
                                &format!("{original_id}_{collision}"),
                                &target.provider,
                                target.api,
                            );
                        }
                        reserved_ids.insert(id.clone());
                        call.id = ToolCallId::new_or_mint(id);
                    }
                    projected_ids.insert(original_id, call.id.clone());
                    pending.push(call.clone());
                }
            }
        }
        completed.push(message);
    }
    finish_tool_exchange(&mut completed, &mut pending, &mut held_system);
    completed
}

fn finish_tool_exchange(
    messages: &mut Vec<Message>,
    pending: &mut Vec<ToolCall>,
    held_system: &mut Vec<Message>,
) {
    for call in pending.drain(..) {
        messages.push(Message::User {
            content: vec![UserContent::tool_result_for(
                call.id,
                call.provider,
                call.function.name,
                vec![ToolResultContent::text("No result provided")],
            )],
        });
    }
    messages.append(held_system);
}

fn render_user_with_context(
    user_content: &[UserContentBlock],
    branch: &[&TranscriptEntry],
    projected: &std::collections::HashMap<&str, &TranscriptEntry>,
    user_index: usize,
) -> Option<Message> {
    let prompt = text_from_user_blocks(user_content).unwrap_or_default();
    let contexts = following_context(branch, projected, user_index);
    convert_user_message(
        user_content,
        Some(render_prompt_with_context(&prompt, &contexts)),
    )
}

fn following_context(
    branch: &[&TranscriptEntry],
    projected: &std::collections::HashMap<&str, &TranscriptEntry>,
    user_index: usize,
) -> Vec<InjectedContext> {
    let mut parent_id = branch[user_index].id.as_str();
    branch
        .iter()
        .skip(user_index + 1)
        .map_while(|entry| {
            if entry.parent_id.as_deref() != Some(parent_id) {
                return None;
            }
            parent_id = entry.id.as_str();
            matches!(
                entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Context { .. }
                }
            )
            .then_some(entry.id.as_str())
        })
        .filter_map(|id| projected.get(id))
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message:
                    AgentMessage::Context {
                        source, content, ..
                    },
            } => text_from_blocks(content).map(|content| InjectedContext {
                source: source.clone(),
                content,
            }),
            _ => None,
        })
        .collect()
}

fn convert_agent_message(message: &AgentMessage, replay: ProtocolReplay) -> Option<Message> {
    match message {
        AgentMessage::User { content } => convert_user_message(content, None),
        AgentMessage::Context { .. } => None,
        AgentMessage::Agent { content } => text_from_blocks(content).map(Message::user),
        AgentMessage::System { content } => Some(Message::system(content.to_string())),
        AgentMessage::Assistant { id, content, .. } => {
            let blocks = content
                .iter()
                .filter_map(|block| assistant_content_from_block(block, replay))
                .collect::<Vec<_>>();
            (!blocks.is_empty()).then(|| Message::Assistant {
                id: (replay == ProtocolReplay::Preserve)
                    .then(|| id.as_ref().map(ToString::to_string))
                    .flatten(),
                content: blocks,
            })
        }
        AgentMessage::ToolResult {
            tool_call_id,
            provider_call_id,
            provider_item_id,
            tool_name,
            content,
            ..
        } => {
            let mut result = content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(ToolResultContent::text(text.to_string())),
                    ContentBlock::Image { source } => {
                        Some(ToolResultContent::Image(image_from_source(source)))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if result.is_empty() {
                result.push(ToolResultContent::text(""));
            }
            let content = match (provider_call_id, provider_item_id, replay) {
                (_, _, ProtocolReplay::Strip) | (None, None, ProtocolReplay::Preserve) => {
                    UserContent::tool_result_for(
                        ToolCallId::new_or_mint(tool_call_id.to_string()),
                        None,
                        tool_name.to_string(),
                        result,
                    )
                }
                (Some(call_id), Some(item_id), ProtocolReplay::Preserve) => {
                    UserContent::tool_result_with_call_id(
                        item_id.to_string(),
                        call_id.to_string(),
                        tool_name.to_string(),
                        result,
                    )
                }
                (Some(call_id), None, ProtocolReplay::Preserve) => {
                    UserContent::tool_result_from_wire(
                        call_id.to_string(),
                        tool_name.to_string(),
                        result,
                    )
                }
                (None, Some(_), ProtocolReplay::Preserve) => return None,
            };
            Some(Message::User {
                content: vec![content],
            })
        }
    }
}

fn assistant_content_from_block(
    block: &ContentBlock,
    replay: ProtocolReplay,
) -> Option<AssistantContent> {
    match block {
        ContentBlock::Text { text } => Some(AssistantContent::text(text.to_string())),
        ContentBlock::Image { source } => Some(AssistantContent::Image(image_from_source(source))),
        ContentBlock::Thinking { text, .. } if replay == ProtocolReplay::Strip => {
            (!text.trim().is_empty()).then(|| AssistantContent::text(text.to_string()))
        }
        ContentBlock::Thinking { text, metadata } => Some(AssistantContent::Reasoning(
            metadata
                .as_ref()
                .map(reasoning_from_metadata)
                .unwrap_or_else(|| Reasoning::new(text.as_str())),
        )),
        ContentBlock::ToolCall {
            id,
            provider_call_id,
            provider_item_id,
            name,
            args,
        } => {
            let function = ToolFunction {
                name: name.to_string(),
                arguments: args.clone(),
            };
            let tool_call = match (provider_call_id, provider_item_id, replay) {
                (_, _, ProtocolReplay::Strip) | (None, None, ProtocolReplay::Preserve) => {
                    ToolCall::new(ToolCallId::new_or_mint(id.to_string()), function)
                }
                (Some(call_id), Some(item_id), ProtocolReplay::Preserve) => {
                    ToolCall::from_dual_wire(item_id.to_string(), call_id.to_string(), function)
                }
                (Some(call_id), None, ProtocolReplay::Preserve) => {
                    ToolCall::from_wire(call_id.to_string(), function)
                }
                (None, Some(_), ProtocolReplay::Preserve) => return None,
            };
            Some(AssistantContent::ToolCall(tool_call))
        }
    }
}

fn image_from_source(source: &ImageSource) -> Image {
    let (data, mime_type) = match source {
        ImageSource::Url { url, mime_type } => (
            DocumentSourceKind::Url(url.to_string()),
            mime_type.as_deref().and_then(image_media_type),
        ),
        ImageSource::Base64 { data, mime_type } => (
            DocumentSourceKind::Base64(data.to_string()),
            image_media_type(mime_type),
        ),
    };
    Image {
        data,
        media_type: mime_type,
        detail: None,
        additional_params: None,
    }
}

fn reasoning_from_metadata(metadata: &ReasoningMetadata) -> Reasoning {
    let mut reasoning = Reasoning::new("");
    reasoning.id = metadata.id.as_ref().map(ToString::to_string);
    reasoning.content = metadata
        .content
        .iter()
        .map(|content| match content {
            ReasoningMetadataContent::Text { text, signature } => ReasoningContent::Text {
                text: text.to_string(),
                signature: signature.as_ref().map(ToString::to_string),
            },
            ReasoningMetadataContent::Encrypted { data } => {
                ReasoningContent::Encrypted(data.to_string())
            }
            ReasoningMetadataContent::Redacted { data } => ReasoningContent::Redacted {
                data: data.to_string(),
            },
            ReasoningMetadataContent::Summary { text } => {
                ReasoningContent::Summary(text.to_string())
            }
        })
        .collect();
    reasoning
}

pub(crate) fn convert_user_message(
    blocks: &[UserContentBlock],
    text_override: Option<String>,
) -> Option<Message> {
    let text = text_override.or_else(|| text_from_user_blocks(blocks));
    let content = text
        .into_iter()
        .map(UserContent::text)
        .chain(blocks.iter().filter_map(|block| match block {
            UserContentBlock::Text { .. } => None,
            UserContentBlock::Image {
                source: ImageSource::Url { url, mime_type },
            } => Some(UserContent::image_url(
                url.to_string(),
                mime_type.as_deref().and_then(image_media_type),
                None,
            )),
            UserContentBlock::Image {
                source: ImageSource::Base64 { data, mime_type },
            } => Some(UserContent::image_base64(
                data.to_string(),
                image_media_type(mime_type),
                None,
            )),
        }))
        .collect::<Vec<_>>();
    (!content.is_empty()).then(|| Message::User { content })
}

fn image_media_type(mime_type: &str) -> Option<ImageMediaType> {
    ImageMediaType::from_mime_type(mime_type)
}

fn text_from_user_blocks(blocks: &[UserContentBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text { text } => Some(text.as_str()),
            UserContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn text_from_blocks(blocks: &[ContentBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

const ESTIMATED_IMAGE_TOKENS: u64 = 1_200;

#[derive(Clone, Copy)]
pub(crate) struct RequestBudget {
    context_window: u64,
    reserve_tokens: u64,
}

impl RequestBudget {
    pub(crate) fn from_config(config: &RuntimeConfig) -> Self {
        Self {
            context_window: config.resolve_context_window(),
            reserve_tokens: config.resolve_reserve_tokens(),
        }
    }

    pub(crate) fn exceeds(&self, request: &CompletionRequest) -> anyhow::Result<bool> {
        if self.context_window == 0 {
            return Ok(false);
        }
        let reserved = self.reserve_tokens.max(request.max_tokens.unwrap_or(0));
        Ok(estimate_request(request)?.saturating_add(reserved) > self.context_window)
    }
}

fn estimate_request(request: &CompletionRequest) -> anyhow::Result<u64> {
    let mut tokens = request
        .preamble
        .as_deref()
        .map(Transcript::estimate_tokens)
        .unwrap_or(0);
    for message in &request.chat_history {
        let estimated = match message {
            Message::System { content } => Transcript::estimate_tokens(content),
            Message::User { content } => {
                let mut total = 0_u64;
                for block in content {
                    let estimated = match block {
                        UserContent::Text(text) => Transcript::estimate_tokens(&text.text),
                        UserContent::Image(_) => ESTIMATED_IMAGE_TOKENS,
                        UserContent::ToolResult(result) => {
                            let mut total = 0_u64;
                            for block in &result.content {
                                total = total.saturating_add(match block {
                                    ToolResultContent::Text(text) => {
                                        Transcript::estimate_tokens(&text.text)
                                    }
                                    ToolResultContent::Image(_) => ESTIMATED_IMAGE_TOKENS,
                                    ToolResultContent::Json { value } => estimate_json(value)?,
                                });
                            }
                            total
                        }
                        _ => estimate_json(block)?,
                    };
                    total = total.saturating_add(estimated);
                }
                total
            }
            Message::Assistant { content, .. } => {
                let mut total = 0_u64;
                for block in content {
                    total = total.saturating_add(match block {
                        AssistantContent::Text(text) => Transcript::estimate_tokens(&text.text),
                        AssistantContent::Image(_) => ESTIMATED_IMAGE_TOKENS,
                        _ => estimate_json(block)?,
                    });
                }
                total
            }
        };
        tokens = tokens.saturating_add(estimated);
    }
    tokens = tokens.saturating_add(estimate_json(&request.tools)?);
    tokens = tokens.saturating_add(estimate_json(&request.documents)?);
    if let Some(schema) = &request.output_schema {
        tokens = tokens.saturating_add(estimate_json(schema)?);
    }
    Ok(tokens)
}

fn estimate_json(value: &impl serde::Serialize) -> anyhow::Result<u64> {
    Ok(Transcript::estimate_tokens(&serde_json::to_string(value)?))
}

const MAX_TOOL_RESULT_CHARS: usize = 2_000;
const IMAGE_SUMMARY_PLACEHOLDER: &str = "[Image content not represented in text summary]";

#[derive(Clone, Copy)]
pub(crate) enum CompactionTrigger {
    Threshold,
    Budget,
    Overflow,
}

pub(crate) struct PreparedCompaction {
    pub(crate) head: compact_str::CompactString,
    pub(crate) summary: String,
    boundary: usize,
}

impl PreparedCompaction {
    pub(crate) async fn publish(&self, hooks: &HookRegistry) {
        emit_post_compact(hooks, self.boundary, &self.summary).await;
    }
}

pub(crate) async fn prepare_compaction(
    client: &AnyClient,
    model_name: &str,
    cfg: &RuntimeConfig,
    session: &Transcript,
    hooks: &HookRegistry,
    trigger: CompactionTrigger,
) -> anyhow::Result<Option<PreparedCompaction>> {
    if !cfg.resolve_compact_enabled() {
        anyhow::ensure!(
            matches!(trigger, CompactionTrigger::Threshold),
            "context budget or overflow recovery requires automatic compaction"
        );
        return Ok(None);
    }
    if matches!(trigger, CompactionTrigger::Threshold)
        && !session.needs_compaction(cfg.resolve_context_window(), cfg.resolve_reserve_tokens())
    {
        return Ok(None);
    }
    let prepared = summarize_context(
        session,
        cfg.resolve_keep_recent_tokens(),
        hooks,
        |conversation, previous| async move {
            client
                .compress_conversation(model_name, &conversation, previous.as_deref(), None)
                .await
        },
    )
    .await?;
    anyhow::ensure!(
        prepared.is_some() || matches!(trigger, CompactionTrigger::Threshold),
        "context budget or overflow recovery has no safe compaction boundary"
    );
    Ok(prepared)
}

#[cfg(test)]
async fn compact_context<F, Fut>(
    session: &mut Transcript,
    keep_recent_tokens: u64,
    hooks: &HookRegistry,
    summarize: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(String, Option<String>) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let Some(compaction) = summarize_context(session, keep_recent_tokens, hooks, summarize).await?
    else {
        return Ok(false);
    };
    session.compress(compaction.summary.clone(), &compaction.head)?;
    compaction.publish(hooks).await;
    Ok(true)
}

async fn summarize_context<F, Fut>(
    session: &Transcript,
    keep_recent_tokens: u64,
    hooks: &HookRegistry,
    summarize: F,
) -> anyhow::Result<Option<PreparedCompaction>>
where
    F: FnOnce(String, Option<String>) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let context = session.effective_context()?;
    let boundary = context.compaction_boundary(keep_recent_tokens);
    if boundary == 0 {
        return Ok(None);
    }
    let head = context.entries[boundary].id.clone();
    let projected = context.projected_entries();
    let summarized = context.entries[..boundary]
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let entries = projected
        .iter()
        .filter(|entry| summarized.contains(entry.id.as_str()))
        .map(|entry| entry.as_ref())
        .collect::<Vec<_>>();
    let conversation = serialize_conversation(&entries);
    let previous = context.preamble_text().map(str::to_owned);
    emit_pre_compact(hooks, boundary).await;
    let summary = summarize(conversation, previous).await?;
    anyhow::ensure!(!summary.trim().is_empty(), "compaction summary is empty");
    Ok(Some(PreparedCompaction {
        head,
        summary,
        boundary,
    }))
}

fn serialize_conversation(entries: &[&TranscriptEntry]) -> String {
    let mut output = String::new();
    for entry in entries {
        let TranscriptEntryKind::Message { message } = &entry.kind else {
            continue;
        };
        let (role, content) = serialize_message(message);
        let _ = writeln!(output, "[{role}]\n{content}\n");
    }
    output
}

fn serialize_message(message: &AgentMessage) -> (&'static str, String) {
    match message {
        AgentMessage::User { content } => ("User", serialize_user_blocks(content)),
        AgentMessage::System { content } => ("System", content.to_string()),
        AgentMessage::Assistant { content, .. } => ("Assistant", serialize_blocks(content)),
        AgentMessage::Agent { content } => ("Agent feedback", serialize_blocks(content)),
        AgentMessage::Context {
            source, content, ..
        } => (
            "Injected context",
            format!("{}\n{}", source.as_str(), serialize_blocks(content)),
        ),
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            is_error,
            ..
        } => (
            "Tool result",
            serialize_tool_result(tool_call_id, tool_name, content, *is_error),
        ),
    }
}

fn serialize_user_blocks(content: &[UserContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            UserContentBlock::Text { text } => text.to_string(),
            UserContentBlock::Image { .. } => IMAGE_SUMMARY_PLACEHOLDER.into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialize_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    content: &[ContentBlock],
    is_error: bool,
) -> String {
    let text = serialize_blocks(content);
    let mut limited: String = text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    if limited.len() < text.len() {
        limited.push_str("\n[Tool result truncated]");
    }
    format!("{tool_name} ({tool_call_id}), error={is_error}\n{limited}")
}

fn serialize_blocks(content: &[ContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.to_string(),
            ContentBlock::Thinking { text, .. } => format!("[Thinking] {text}"),
            ContentBlock::Image { .. } => IMAGE_SUMMARY_PLACEHOLDER.into(),
            ContentBlock::ToolCall { id, name, args, .. } => {
                format!("[Tool call] {name} ({id}) {args}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compaction_does_not_initialize_a_session() {
        let mut hooks = HookRegistry::new();
        hooks.register_hook(crate::hooks::Hook::session_start(|_| async {
            panic!("compaction is not a SessionStart boundary")
        }));
        PreparedCompaction {
            head: "head".into(),
            summary: "summary".into(),
            boundary: 1,
        }
        .publish(&hooks)
        .await;
    }

    mod instructions {
        use super::*;

        #[test]
        fn loads_project_root_instructions_without_parent_recursion() {
            let base =
                std::env::temp_dir().join(format!("phi-instructions-{}", uuid::Uuid::new_v4()));
            let project = base.join("project");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(base.join("AGENTS.md"), "parent rules").unwrap();
            std::fs::write(project.join("AGENTS.md"), "project agents").unwrap();
            std::fs::write(project.join("CLAUDE.md"), "project claude").unwrap();

            let loaded = load_agents_from(None, Some(project.clone())).unwrap();

            assert!(loaded.contains("project agents"));
            assert!(!loaded.contains("project claude"));
            assert!(!loaded.contains("parent rules"));
            std::fs::remove_file(project.join("AGENTS.md")).unwrap();
            let claude_only = load_agents_from(None, Some(project)).unwrap();
            assert!(claude_only.contains("project claude"));
            assert!(!claude_only.contains("project agents"));
            std::fs::remove_dir_all(base).unwrap();
        }

        #[test]
        fn explicit_working_dir_loads_child_instructions_without_changing_cwd() {
            let root =
                std::env::temp_dir().join(format!("phi-child-context-{}", uuid::Uuid::new_v4()));
            let child = root.join("child");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(child.join("AGENTS.md"), "child-only instructions").unwrap();

            let context = load_with_options_for(false, None, Vec::new(), &child);
            assert!(context.agents.unwrap().contains("child-only instructions"));
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn loads_git_root_instructions_from_subdirectory() {
            let root =
                std::env::temp_dir().join(format!("phi-instructions-{}", uuid::Uuid::new_v4()));
            let nested = root.join("src");
            std::fs::create_dir_all(&nested).unwrap();
            assert!(
                std::process::Command::new("git")
                    .args(["init", "--quiet"])
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
            std::fs::write(root.join("AGENTS.md"), "root agents").unwrap();
            std::fs::write(root.join("CLAUDE.md"), "root claude").unwrap();
            std::fs::write(nested.join("AGENTS.md"), "nested agents").unwrap();
            std::fs::write(nested.join("CLAUDE.md"), "nested claude").unwrap();

            let loaded = load_agents_from(None, Some(nested.clone())).unwrap();
            assert!(loaded.contains("root agents"));
            assert!(!loaded.contains("root claude"));
            assert!(!loaded.contains("nested agents"));
            assert!(!loaded.contains("nested claude"));
            std::fs::remove_file(root.join("AGENTS.md")).unwrap();
            let claude_only = load_agents_from(None, Some(nested)).unwrap();
            assert!(claude_only.contains("root claude"));
            assert!(!claude_only.contains("nested agents"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    mod attachments {
        use super::*;

        #[test]
        fn renders_context_before_the_user_prompt() {
            let rendered = render_prompt_with_context(
                "continue",
                &[InjectedContext {
                    source: ContextSource::FileMention,
                    content: "src/lib.rs".to_string(),
                }],
            );

            assert_eq!(
                rendered,
                "<context source=\"file_mention\">\nsrc/lib.rs\n</context>\n\n<user_prompt>\ncontinue\n</user_prompt>"
            );
        }
    }

    mod system {
        use super::*;
        use crate::agent::options::PLAN_MODE_PROMPT;

        #[test]
        fn runtime_environment_uses_explicit_child_directory() {
            let child = std::path::Path::new("/tmp/phi-child-worktree");
            let mut prompt = String::new();
            append_runtime_environment(&mut prompt, child);
            assert!(prompt.contains("Current working directory: /tmp/phi-child-worktree"));
            assert!(!prompt.contains(&std::env::current_dir().unwrap().display().to_string()));
        }

        #[test]
        fn appends_resolved_mode_prompt_after_base_prompt() {
            let cases = [
                (AgentMode::Default, None, None),
                (AgentMode::Plan, None, Some(PLAN_MODE_PROMPT)),
                (
                    AgentMode::Default,
                    Some("custom default"),
                    Some("custom default"),
                ),
                (AgentMode::Plan, Some("custom plan"), Some("custom plan")),
            ];

            for (mode, prompt_override, expected_mode_prompt) in cases {
                let mut prompt = "agent".to_string();
                append_mode_prompt(&mut prompt, mode, prompt_override);

                let expected = expected_mode_prompt
                    .map(|mode_prompt| format!("agent\n\n{mode_prompt}"))
                    .unwrap_or_else(|| "agent".to_string());
                assert_eq!(prompt, expected);
            }
        }

        #[test]
        fn replaces_only_the_contents_of_a_prompt_section() {
            let mut prompt = "before <tools>\nold\n</tools> after".to_string();

            replace_prompt_section(&mut prompt, "<tools>", "</tools>", "new");

            assert_eq!(prompt, "before <tools>\nnew\n</tools> after");
        }
    }

    mod turn {
        use super::*;
        use crate::core::lifecycle::{
            PromptContextOutcome, UserPromptSubmitOutcome, UserPromptSubmitSource,
        };
        use crate::core::turn::{InjectedContext, TurnToolPolicy};
        use crate::hooks::Hook;
        use crate::transcript::ContextSource;
        use std::sync::Mutex;

        struct TestExpander(Arc<Mutex<Vec<&'static str>>>);

        impl PromptExpander for TestExpander {
            fn describe(&self, event: &UserPromptSubmitEvent) -> Option<UserPromptExpansionEvent> {
                self.0.lock().unwrap().push("describe");
                Some(UserPromptExpansionEvent {
                    prompt: event.prompt.clone(),
                    command_name: "review".into(),
                    command_args: "main".into(),
                    command_source: "test".into(),
                })
            }

            fn expand(
                self: Arc<Self>,
                event: UserPromptExpansionEvent,
            ) -> BoxFuture<'static, Result<PromptExpansion, String>> {
                Box::pin(async move {
                    assert_eq!(event.prompt, "/review main");
                    self.0.lock().unwrap().push("expand");
                    Ok(PromptExpansion {
                        prompt: Some("expanded body".into()),
                        ..Default::default()
                    })
                })
            }
        }

        #[derive(Clone, Copy)]
        enum GateResult {
            Allow,
            SubmitBlock,
            ExpansionBlock,
            Error,
            Timeout,
        }

        #[tokio::test(start_paused = true)]
        async fn expansion_runs_only_after_both_gates_accept() {
            for result in [
                GateResult::Allow,
                GateResult::SubmitBlock,
                GateResult::ExpansionBlock,
                GateResult::Error,
                GateResult::Timeout,
            ] {
                for dispatch in [
                    UserPromptDispatch::Immediate,
                    UserPromptDispatch::Steer,
                    UserPromptDispatch::FollowUp,
                ] {
                    let calls = Arc::new(Mutex::new(Vec::new()));
                    let mut hooks = HookRegistry::new();
                    let observed = calls.clone();
                    hooks.register_hook(Hook::user_prompt_submit(move |event| {
                        observed.lock().unwrap().push("submit");
                        async move {
                            assert_eq!(event.prompt, "/review main");
                            Ok(if matches!(result, GateResult::SubmitBlock) {
                                UserPromptSubmitOutcome::Handled {
                                    reason: Some("submission blocked".into()),
                                }
                            } else {
                                UserPromptSubmitOutcome::Continue {
                                    notices: Vec::new(),
                                    prompt: event.prompt,
                                    injected_context: Vec::new(),
                                }
                            })
                        }
                    }));
                    let observed = calls.clone();
                    hooks.register_hook(
                        Hook::user_prompt_expansion(move |event| {
                            observed.lock().unwrap().push("expansion gate");
                            async move {
                                assert_eq!(event.command_name, "review");
                                assert_eq!(event.command_args, "main");
                                match result {
                                    GateResult::ExpansionBlock => {
                                        Ok(UserPromptExpansionOutcome::Block {
                                            reason: "expansion blocked".into(),
                                        })
                                    }
                                    GateResult::Error => Err(HookError::new("gate unavailable")),
                                    GateResult::Timeout => std::future::pending().await,
                                    _ => Ok(UserPromptExpansionOutcome::AddContext {
                                        contexts: vec![crate::context::InjectedContext {
                                            source: ContextSource::Hook,
                                            content: "review context".into(),
                                        }],
                                    }),
                                }
                            }
                        })
                        .with_timeout(std::time::Duration::from_secs(1)),
                    );
                    let observed = calls.clone();
                    hooks.register_hook(Hook::prompt_context(move |event| {
                        observed.lock().unwrap().push("context");
                        async move {
                            assert_eq!(event.original_prompt, "/review main");
                            assert_eq!(event.prompt, "expanded body");
                            Ok(PromptContextOutcome::default())
                        }
                    }));
                    let preparation = PromptPreparation {
                        hooks,
                        expander: Some(Arc::new(TestExpander(calls.clone()))),
                        transcript: Arc::new(Transcript::new()),
                        working_dir: "/tmp".into(),
                        delegated_child: false,
                    };
                    let prepared = preparation
                        .prepare(
                            TurnPrompt {
                                prompt: "/review main".into(),
                                source: UserPromptSubmitSource::Sdk,
                                injected_context: Vec::new(),
                                images: Vec::new(),
                                tool_policy: Default::default(),
                            },
                            dispatch,
                        )
                        .await;
                    match result {
                        GateResult::Allow => {
                            let CoreTurnPreparation::Ready(prepared) = prepared.unwrap() else {
                                panic!("expected expansion");
                            };
                            assert_eq!(prepared.prompt, "expanded body");
                            assert_eq!(prepared.contexts[0].content, "review context");
                            assert_eq!(
                                *calls.lock().unwrap(),
                                ["submit", "describe", "expansion gate", "expand", "context"]
                            );
                        }
                        GateResult::SubmitBlock => {
                            assert!(matches!(
                                prepared.unwrap(),
                                CoreTurnPreparation::Handled { .. }
                            ));
                            assert_eq!(*calls.lock().unwrap(), ["submit"]);
                        }
                        _ => {
                            if matches!(result, GateResult::ExpansionBlock) {
                                assert!(matches!(
                                    prepared.unwrap(),
                                    CoreTurnPreparation::Handled { .. }
                                ));
                            } else {
                                assert!(prepared.is_err());
                            }
                            assert_eq!(
                                *calls.lock().unwrap(),
                                ["submit", "describe", "expansion gate"]
                            );
                        }
                    }
                }
            }
        }

        fn preparation(hooks: &HookRegistry) -> PromptPreparation {
            PromptPreparation {
                hooks: hooks.clone(),
                expander: None,
                transcript: Arc::new(Transcript::new()),
                working_dir: PathBuf::from("/tmp"),
                delegated_child: false,
            }
        }

        #[tokio::test]
        async fn transcript_preparation_failure_preserves_its_source() {
            let hooks = HookRegistry::new();
            let mut preparation = preparation(&hooks);
            Arc::make_mut(&mut preparation.transcript).leaf_id = Some("missing".into());
            let result = preparation
                .prepare(
                    TurnPrompt {
                        prompt: "inspect".into(),
                        source: UserPromptSubmitSource::Sdk,
                        injected_context: Vec::new(),
                        images: Vec::new(),
                        tool_policy: Default::default(),
                    },
                    UserPromptDispatch::Immediate,
                )
                .await;
            let error = result
                .err()
                .expect("invalid transcript must fail preparation");
            assert!(matches!(&error, PromptPreparationError::Transcript(_)));
            assert!(std::error::Error::source(&error).is_some());
            assert!(error.to_string().contains("ancestry"));
        }

        #[test]
        fn memory_timeout_preserves_prompt_without_rearming_automatic_injection() {
            use crate::extension::memory::{
                HookAction, HookConfig, MemoryConfig, MemoryDirSettings, MemoryHooks,
            };
            use crate::hooks::HookNotice;

            for actions in [Vec::new(), vec![HookAction::Pull]] {
                for input in ["ordinary input", "/memory-inject inspect"] {
                    let root = PathBuf::from("/tmp")
                        .join(format!("phi-memory-timeout-{}", uuid::Uuid::new_v4()));
                    let working_dir = root.join("workspace");
                    let memory_dir = root.join("memory/workspace/core/project");
                    std::fs::create_dir_all(&working_dir).unwrap();
                    std::fs::create_dir_all(&memory_dir).unwrap();
                    std::fs::write(
                        memory_dir.join("context.md"),
                        "---\ndescription: timeout recovery\n---\nrecovered memory",
                    )
                    .unwrap();
                    let settings = crate::extension::memory::settings_from_config(&MemoryConfig {
                        injection: Some(
                            crate::extension::memory::MemoryInjectionMode::MessageAppend,
                        ),
                        memory_dir: Some(MemoryDirSettings {
                            local_path: Some(root.join("memory").display().to_string()),
                            ..Default::default()
                        }),
                        hooks: Some(HookConfig {
                            session_start: Vec::new(),
                            user_prompt_submit: actions.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                    let mut hooks = HookRegistry::new();
                    for hook in MemoryHooks::new(settings).prompt_hooks() {
                        hooks.register_hook(hook);
                    }
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .start_paused(true)
                        .max_blocking_threads(1)
                        .build()
                        .unwrap();
                    runtime.block_on(async {
                        let (started, running) = std::sync::mpsc::channel();
                        let (release, waiting) = std::sync::mpsc::channel();
                        let blocker = tokio::task::spawn_blocking(move || {
                            started.send(()).unwrap();
                            waiting.recv().unwrap();
                        });
                        running.recv().unwrap();
                        let mut preparation = preparation(&hooks);
                        preparation.working_dir = working_dir;
                        let prompt = TurnPrompt {
                            prompt: input.into(),
                            source: UserPromptSubmitSource::Sdk,
                            injected_context: Vec::new(),
                            images: Vec::new(),
                            tool_policy: Default::default(),
                        };
                        let mut preparing = Box::pin(
                            preparation
                                .clone()
                                .prepare(prompt.clone(), UserPromptDispatch::Immediate),
                        );
                        assert!(futures::poll!(preparing.as_mut()).is_pending());
                        tokio::time::advance(std::time::Duration::from_secs(11)).await;
                        let result = preparing.await;
                        release.send(()).unwrap();
                        blocker.await.unwrap();
                        tokio::time::resume();
                        // Flush timed-out blocking work before verifying a fresh attempt.
                        tokio::task::spawn_blocking(|| ()).await.unwrap();
                        let mut transcript = Transcript::new();
                        if !actions.is_empty() {
                            assert!(matches!(result.unwrap(), CoreTurnPreparation::Handled { reason: Some(reason) } if reason.contains("timed out")));
                        } else if input.starts_with("/memory-inject") {
                            let Err(error) = result else {
                                panic!("explicit Memory timeout must reject preparation");
                            };
                            assert!(matches!(error, PromptPreparationError::Hook(source) if source.message().contains("timed out")));
                        } else {
                            let CoreTurnPreparation::Ready(prepared) = result.unwrap() else {
                                panic!("automatic Memory timeout must preserve input");
                            };
                            assert_eq!(prepared.prompt, input);
                            assert!(prepared.contexts.is_empty());
                            prepared.commit(&mut transcript);
                            hooks.record_notices(prepared.hook_notices);
                            assert!(matches!(
                                hooks.take_notices().as_slice(),
                                [HookNotice::Warning { message }]
                                    if message.contains("Memory context failed")
                                        && message.contains("timed out")
                            ));
                        }
                        preparation.transcript = Arc::new(transcript.clone());
                        let CoreTurnPreparation::Ready(recovered) = preparation.clone()
                            .prepare(prompt.clone(), UserPromptDispatch::Immediate)
                            .await
                            .unwrap()
                        else {
                            panic!("Memory preparation must recover after timeout");
                        };
                        assert_eq!(
                            recovered.contexts.iter().any(|context| context.content.contains("timeout recovery")),
                            input.starts_with("/memory-inject") || !actions.is_empty()
                        );
                        recovered.commit(&mut transcript);
                        hooks.record_notices(recovered.hook_notices);
                        preparation.transcript = Arc::new(transcript.clone());
                        let CoreTurnPreparation::Ready(next) = preparation
                            .prepare(TurnPrompt { prompt: "next input".into(), ..prompt }, UserPromptDispatch::Immediate)
                            .await.unwrap() else { panic!("ordinary input must remain available"); };
                        assert_eq!(next.contexts.is_empty(), !input.starts_with("/memory-inject"));
                    });
                    std::fs::remove_dir_all(root).unwrap();
                }
            }
        }

        #[tokio::test]
        async fn prepared_prompt_retains_policy_across_rewrites_and_handling() {
            let mut hooks = HookRegistry::new();
            hooks.register_hook(crate::hooks::Hook::user_prompt_submit(|event| async move {
                if event.prompt == "handled" {
                    Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
                } else {
                    Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: "rewritten".into(),
                        injected_context: Vec::new(),
                    })
                }
            }));
            hooks.register_hook(crate::hooks::Hook::user_prompt_submit(|event| async move {
                assert!(matches!(
                    event.original_prompt.as_str(),
                    "original" | "handled"
                ));
                Ok(
                    crate::core::lifecycle::UserPromptSubmitOutcome::ContinueWithPolicy {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                        tool_policy: TurnToolPolicy {
                            allowances: vec![
                                ("read".into(), "**".into()),
                                ("Write".into(), "**".into()),
                            ],
                            removals: vec!["Edit".into()],
                        },
                    },
                )
            }));
            let policy = TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into())],
                removals: vec!["Bash".into()],
            };
            let expected_policy = TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into()), ("Write".into(), "**".into())],
                removals: vec!["Bash".into(), "Edit".into()],
            };
            for dispatch in [
                UserPromptDispatch::Immediate,
                UserPromptDispatch::Steer,
                UserPromptDispatch::FollowUp,
            ] {
                let prompt = TurnPrompt {
                    prompt: "original".into(),
                    source: UserPromptSubmitSource::Sdk,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: policy.clone(),
                };
                let CoreTurnPreparation::Ready(prepared) = preparation(&hooks)
                    .prepare(prompt.clone(), dispatch)
                    .await
                    .unwrap()
                else {
                    panic!("expected prepared prompt");
                };
                assert_eq!(prepared.prompt, "rewritten");
                assert_eq!(prepared.tool_policy, expected_policy);
                assert!(matches!(
                    preparation(&hooks)
                        .prepare(
                            TurnPrompt {
                                prompt: "handled".into(),
                                ..prompt
                            },
                            dispatch,
                        )
                        .await
                        .unwrap(),
                    CoreTurnPreparation::Handled { .. }
                ));
            }
        }

        #[tokio::test]
        async fn preparation_preserves_content_and_context() {
            for source in [
                UserPromptSubmitSource::Sdk,
                UserPromptSubmitSource::Internal,
                UserPromptSubmitSource::Extension,
            ] {
                for dispatch in [
                    UserPromptDispatch::Immediate,
                    UserPromptDispatch::Steer,
                    UserPromptDispatch::FollowUp,
                ] {
                    for images in [
                        Vec::new(),
                        vec![ImageInput::url("https://example.com/image.png")],
                    ] {
                        for hook_context in [
                            Vec::new(),
                            vec![InjectedContext {
                                source: crate::transcript::ContextSource::Hook,
                                content: "hook context".into(),
                            }],
                        ] {
                            let mut hooks = HookRegistry::new();
                            hooks.register_hook(crate::hooks::Hook::user_prompt_submit({
                                let hook_context = hook_context.clone();
                                move |_| {
                                    let hook_context = hook_context.clone();
                                    async move {
                                        Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                                notices: Vec::new(),
                                            prompt: "rewritten".into(),
                                            injected_context: hook_context,
                                        })
                                    }
                                }
                            }));
                            let contexts = vec![InjectedContext {
                                source: crate::transcript::ContextSource::FileMention,
                                content: "file context".into(),
                            }];
                            let expected_contexts =
                                collect_injected_context(contexts.clone(), hook_context);
                            let CoreTurnPreparation::Ready(prepared) = preparation(&hooks)
                                .prepare(
                                    TurnPrompt {
                                        prompt: "original".into(),
                                        source,
                                        injected_context: contexts,
                                        images: images.clone(),
                                        tool_policy: TurnToolPolicy::default(),
                                    },
                                    dispatch,
                                )
                                .await
                                .unwrap()
                            else {
                                panic!("expected prepared prompt");
                            };
                            assert_eq!(prepared.source.as_str(), source.as_str());
                            assert_eq!(prepared.prompt, "rewritten");
                            assert_eq!(prepared.contexts, expected_contexts);
                            assert_eq!(
                                serde_json::to_value(&prepared.user_content).unwrap(),
                                serde_json::to_value(canonical_user_content(
                                    "rewritten",
                                    images.clone()
                                ))
                                .unwrap()
                            );
                            assert!(prepared.submission_id.is_none());
                        }
                    }
                }
            }
        }
    }

    mod history {
        use super::*;
        use crate::transcript::ContextScope;
        use rig::client::CompletionClient;

        fn responses_request(transcript: &Transcript, target: &HistoryTarget) -> serde_json::Value {
            let client = rig::providers::openai::Client::new("test-key").unwrap();
            let model = client.completion_model(&target.model);
            let messages = convert_history_for_target(transcript, Some(target)).unwrap();
            let request = crate::provider::generation::request_builder(
                &model,
                messages,
                crate::provider::ProviderHistoryPolicy::Native,
            )
            .unwrap()
            .build();
            let request = rig::providers::openai::responses_api::CompletionRequest::try_from((
                target.model.clone(),
                request,
            ))
            .unwrap();
            serde_json::to_value(request).unwrap()
        }

        fn serialize_target_history(
            messages: Vec<Message>,
            api: crate::provider::ProviderApi,
        ) -> String {
            use crate::provider::ProviderApi;
            let wire = messages
                .into_iter()
                .map(|message| match api {
                    ProviderApi::AnthropicMessages => serde_json::to_value(
                        rig::providers::anthropic::completion::Message::try_from(message).unwrap(),
                    )
                    .unwrap(),
                    ProviderApi::OpenAiCompletions => serde_json::to_value(
                        Vec::<rig::providers::openai::completion::Message>::try_from(message)
                            .unwrap(),
                    )
                    .unwrap(),
                    ProviderApi::OpenAiResponses | ProviderApi::OpenAiCodexResponses => {
                        serde_json::to_value(
                            Vec::<rig::providers::openai::responses_api::InputItem>::try_from(
                                message,
                            )
                            .unwrap(),
                        )
                        .unwrap()
                    }
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>();
            serde_json::to_string(&wire).unwrap()
        }

        proptest::proptest! {
            #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]

            #[test]
            fn context_edits_preserve_injected_context_associations(
                omitted_mask in 0_u8..8,
                replaced_mask in 0_u8..8,
                owner_edit in 0_u8..3,
                agent_owner in proptest::bool::ANY,
            ) {
                use crate::transcript::{ContextContent, ContextReplacement};
                let mut transcript = Transcript::new();
                if agent_owner {
                    transcript.append_agent_message("original owner");
                } else {
                    transcript.append_user_message("original owner");
                }
                let owner = transcript.leaf_id.clone().unwrap();
                let mut targets = Vec::new();
                for index in 0..3 {
                    transcript.append_context_message(ContextSource::Hook, ContextScope::Turn, &format!("original context {index}"));
                    targets.push(transcript.leaf_id.clone().unwrap());
                }
                for (index, target) in targets.iter().enumerate() {
                    if omitted_mask & (1 << index) != 0 {
                        transcript.append_context_edit(target, None).unwrap();
                    } else if replaced_mask & (1 << index) != 0 {
                        transcript.append_context_edit(target, Some(ContextReplacement {
                            content: ContextContent::Text(format!("replacement context {index}").into()),
                        })).unwrap();
                    }
                }
                match owner_edit {
                    1 => { transcript.append_context_edit(&owner, None).unwrap(); }
                    2 => { transcript.append_context_edit(&owner, Some(ContextReplacement {
                        content: ContextContent::Text("replacement owner".into()),
                    })).unwrap(); }
                    _ => {}
                }
                transcript.append_user_message("next question");
                let before = serde_json::to_value(&transcript).unwrap();
                let messages = convert_history(&transcript).unwrap();
                let rendered = serde_json::to_string(&messages).unwrap();
                for index in 0..3 {
                    let retained = owner_edit != 1 && omitted_mask & (1 << index) == 0;
                    proptest::prop_assert_eq!(rendered.contains(&format!("original context {index}")), retained && replaced_mask & (1 << index) == 0);
                    proptest::prop_assert_eq!(rendered.contains(&format!("replacement context {index}")), retained && replaced_mask & (1 << index) != 0);
                }
                proptest::prop_assert_eq!(messages.len(), if owner_edit == 1 { 1 } else { 2 });
                proptest::prop_assert!(!serde_json::to_string(messages.last().unwrap()).unwrap().contains("context"));
                proptest::prop_assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
            }

            #[test]
            fn context_edits_preserve_request_tool_pairing(
                assistant_edit in 0_u8..4,
                retained_calls in 0_u8..8,
                omitted_results in 0_u8..8,
                same_source in proptest::bool::ANY,
                api in proptest::sample::select(vec![
                    crate::provider::ProviderApi::AnthropicMessages,
                    crate::provider::ProviderApi::OpenAiResponses,
                    crate::provider::ProviderApi::OpenAiCodexResponses,
                    crate::provider::ProviderApi::OpenAiCompletions,
                ]),
            ) {
                use crate::transcript::{ContextContent, ContextReplacement};
                let mut transcript = Transcript::new();
                let calls = (0..3).map(|index| ContentBlock::ToolCall {
                    id: format!("phi-{index}").into(),
                    provider_call_id: Some(format!("call_{index}").into()),
                    provider_item_id: Some(format!("fc_{index}").into()),
                    name: "Read".into(), args: serde_json::json!({"index": index}),
                }).collect::<Vec<_>>();
                for round in 0..2 {
                    transcript.append_user_message("run tools");
                    transcript.append_assistant_message(
                        None, None, calls.clone(),
                        if same_source { "target" } else { "foreign" }.into(),
                        api, "model".into(), Default::default(), "tool_use".into(),
                    );
                    let assistant = transcript.leaf_id.clone().unwrap();
                    for index in 0..3 {
                        transcript.append_tool_result(&format!("phi-{index}"), &format!("output-{round}-{index}"), false, None);
                        if round == 0 && omitted_results & (1 << index) != 0 {
                            let result = transcript.leaf_id.clone().unwrap();
                            transcript.append_context_edit(&result, None).unwrap();
                        }
                    }
                    if round == 0 {
                        match assistant_edit {
                            1 => { transcript.append_context_edit(&assistant, None).unwrap(); }
                            2 | 3 => {
                                let content = if assistant_edit == 2 {
                                    ContextContent::Text("replacement answer".into())
                                } else {
                                    ContextContent::Blocks(calls.iter().enumerate().filter(|(index, _)| retained_calls & (1 << index) != 0).map(|(_, call)| call.clone()).collect())
                                };
                                transcript.append_context_edit(&assistant, Some(ContextReplacement { content })).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                transcript.append_user_message("continue");
                let before = serde_json::to_value(&transcript).unwrap();
                let target = HistoryTarget { provider: "target".into(), api: Some(api), model: "model".into() };
                let messages = convert_history_for_target(&transcript, Some(&target)).unwrap();
                let mut pending = std::collections::HashSet::new();
                let mut call_count = 0;
                let mut result_count = 0;
                for message in &messages {
                    match message {
                        Message::Assistant { content, .. } => {
                            proptest::prop_assert!(pending.is_empty());
                            for block in content {
                                if let AssistantContent::ToolCall(call) = block {
                                    proptest::prop_assert!(pending.insert(call.wire_call_id().to_owned()));
                                    call_count += 1;
                                }
                            }
                        }
                        Message::User { content } => {
                            for block in content {
                                if let UserContent::ToolResult(result) = block {
                                    proptest::prop_assert!(pending.remove(result.wire_call_id()), "orphan or duplicate result: {:?}", result);
                                    result_count += 1;
                                } else {
                                    proptest::prop_assert!(pending.is_empty());
                                }
                            }
                        }
                        _ => { proptest::prop_assert!(pending.is_empty()); }
                    }
                }
                proptest::prop_assert!(pending.is_empty());
                let first_calls = match assistant_edit {
                    0 => 3,
                    3 => retained_calls.count_ones() as usize,
                    _ => 0,
                };
                proptest::prop_assert_eq!(call_count, first_calls + 3);
                proptest::prop_assert_eq!(result_count, call_count);
                let wire = serialize_target_history(messages, api);
                for index in 0..3 {
                    let retained = assistant_edit == 0 || (assistant_edit == 3 && retained_calls & (1 << index) != 0);
                    proptest::prop_assert_eq!(wire.contains(&format!("output-0-{index}")), retained && omitted_results & (1 << index) == 0);
                    proptest::prop_assert!(wire.contains(&format!("output-1-{index}")), "missing unedited result");
                }
                proptest::prop_assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
            }
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum ContextBoundary {
            Branch,
            Compaction,
            Reset,
        }

        proptest::proptest! {
            #[test]
            fn request_projection_completes_tool_exchanges(
                result_masks in proptest::collection::vec(0_u8..8, 1..5),
                insert_system in proptest::bool::ANY,
                same_source in proptest::bool::ANY,
                ending in 0_u8..3,
                stop_reason in proptest::sample::select(vec!["tool_use", "error", "aborted"]),
            ) {
                let mut transcript = Transcript::new();
                for (round, mask) in result_masks.iter().enumerate() {
                    transcript.append_user_message("run tools");
                    transcript.append_assistant_message(
                        Some("msg_source".into()),
                        None,
                        (0..3).map(|index| ContentBlock::ToolCall {
                            id: format!("phi-{index}").into(),
                            provider_call_id: Some(format!("call_{index}").into()),
                            provider_item_id: Some(format!("fc_{index}").into()),
                            name: "Read".into(),
                            args: serde_json::json!({"index": index}),
                        }).collect(),
                        if same_source { "openai" } else { "other" }.into(),
                        crate::provider::ProviderApi::OpenAiResponses,
                        "gpt-one".into(),
                        Default::default(),
                        stop_reason.into(),
                    );
                    if insert_system {
                        transcript.append_system_instruction("keep this instruction");
                    }
                    for index in (0..3).rev() {
                        if mask & (1 << index) != 0 {
                            transcript.append_tool_result(&format!("phi-{index}"), &format!("output-{round}-{index}"), false, None);
                        }
                    }
                }
                match ending {
                    0 => { transcript.append_user_message("continue"); }
                    1 => {
                        transcript.append_assistant_message(
                            None, None,
                            vec![ContentBlock::Text { text: "assistant boundary".into() }],
                            "openai".into(),
                            crate::provider::ProviderApi::OpenAiResponses,
                            "gpt-one".into(), Default::default(), "stop".into(),
                        );
                    }
                    _ => {}
                }
                let before = serde_json::to_value(&transcript).unwrap();
                let target = HistoryTarget {
                    provider: "openai".into(),
                    api: Some(crate::provider::ProviderApi::OpenAiResponses),
                    model: "gpt-one".into(),
                };
                let request = responses_request(&transcript, &target);
                let input = request["input"].as_array().unwrap();
                let mut pending = std::collections::HashMap::new();
                let mut call_count = 0;
                let mut result_count = 0;
                for item in input {
                    match item["type"].as_str() {
                        Some("function_call") => {
                            let arguments: serde_json::Value = serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
                            let index = arguments["index"].as_u64().unwrap() as usize;
                            let round = call_count / 3;
                            let expected = if result_masks[round] & (1 << index) != 0 {
                                format!("output-{round}-{index}")
                            } else {
                                "No result provided".into()
                            };
                            proptest::prop_assert!(pending.insert(item["call_id"].as_str().unwrap(), expected).is_none());
                            call_count += 1;
                        }
                        Some("function_call_output") => {
                            let expected = pending.remove(item["call_id"].as_str().unwrap());
                            proptest::prop_assert!(expected.is_some(), "unmatched or duplicate result");
                            proptest::prop_assert_eq!(item["output"].as_str(), expected.as_deref());
                            result_count += 1;
                        }
                        _ => { proptest::prop_assert!(pending.is_empty(), "a message interrupted a tool exchange"); }
                    }
                }
                proptest::prop_assert!(pending.is_empty());
                proptest::prop_assert_eq!(call_count, result_masks.len() * 3);
                proptest::prop_assert_eq!(result_count, call_count);
                proptest::prop_assert_eq!(
                    request.to_string().matches("keep this instruction").count(),
                    if insert_system { result_masks.len() } else { 0 },
                );
                proptest::prop_assert!(!serde_json::to_string(&convert_history(&transcript).unwrap()).unwrap().contains("No result provided"));
                proptest::prop_assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
            }

            #[test]
            fn foreign_tool_ids_fit_target_and_keep_pairing(
                arbitrary_id in ".{1,100}",
                result_mask in 0_u8..128,
                api in proptest::sample::select(vec![
                    crate::provider::ProviderApi::AnthropicMessages,
                    crate::provider::ProviderApi::OpenAiResponses,
                    crate::provider::ProviderApi::OpenAiCodexResponses,
                    crate::provider::ProviderApi::OpenAiCompletions,
                ]),
            ) {
                let source_ids = [
                    "read:file".to_owned(),
                    "read/file".to_owned(),
                    format!("{}a", "x".repeat(100)),
                    format!("{}b", "x".repeat(100)),
                    "already_valid".to_owned(),
                    "call|item".to_owned(),
                    arbitrary_id,
                ];
                proptest::prop_assume!(source_ids.iter().collect::<std::collections::HashSet<_>>().len() == source_ids.len());
                let target = HistoryTarget { provider: "openai".into(), api: Some(api), model: "target".into() };
                let mut transcript = Transcript::new();
                for round in 0..2 {
                    transcript.append_user_message("run tools");
                    transcript.append_assistant_message(
                        None, None,
                        source_ids.iter().map(|id| ContentBlock::ToolCall {
                            id: id.as_str().into(),
                            provider_call_id: Some(format!("foreign-{id}").into()),
                            provider_item_id: None,
                            name: "Read".into(), args: serde_json::json!({}),
                        }).collect(),
                        "foreign".into(), crate::provider::ProviderApi::OpenAiCompletions,
                        "source".into(), Default::default(), "tool_use".into(),
                    );
                    for (index, id) in source_ids.iter().enumerate() {
                        if result_mask & (1 << index) != 0 {
                            transcript.append_tool_result(id, &format!("output-{round}-{index}"), false, None);
                        }
                    }
                }
                let before = serde_json::to_value(&transcript).unwrap();
                let messages = convert_history_for_target(&transcript, Some(&target)).unwrap();
                let wire = serialize_target_history(messages.clone(), api);
                let max_length = if api == crate::provider::ProviderApi::OpenAiCompletions { 40 } else { 64 };
                let mut pending = std::collections::HashMap::new();
                let mut call_count = 0;
                let mut result_count = 0;
                for message in &messages {
                    match message {
                        Message::Assistant { content, .. } => {
                            proptest::prop_assert!(pending.is_empty());
                            for block in content {
                                if let AssistantContent::ToolCall(call) = block {
                                    let id = call.wire_call_id();
                                    proptest::prop_assert!(!id.is_empty() && id.len() <= max_length);
                                    proptest::prop_assert!(id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
                                    let index = call_count % source_ids.len();
                                    let round = call_count / source_ids.len();
                                    let expected = if result_mask & (1 << index) != 0 {
                                        format!("output-{round}-{index}")
                                    } else {
                                        "No result provided".into()
                                    };
                                    proptest::prop_assert!(pending.insert(id.to_owned(), expected).is_none(), "target IDs collided");
                                    proptest::prop_assert!(call.provider.is_none());
                                    let source_id = &source_ids[index];
                                    if source_id.len() <= max_length && source_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
                                        proptest::prop_assert_eq!(id, source_id);
                                    }
                                    proptest::prop_assert!(wire.contains(&serde_json::to_string(id).unwrap()));
                                    call_count += 1;
                                }
                            }
                        }
                        Message::User { content } => {
                            for block in content {
                                if let UserContent::ToolResult(result) = block {
                                    let expected = pending.remove(result.wire_call_id()).expect("unmatched or duplicate result");
                                    proptest::prop_assert!(matches!(result.content.as_slice(), [ToolResultContent::Text(text)] if text.text == expected));
                                    proptest::prop_assert!(result.provider.is_none());
                                    result_count += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                proptest::prop_assert!(pending.is_empty());
                proptest::prop_assert_eq!(call_count, source_ids.len() * 2);
                proptest::prop_assert_eq!(result_count, call_count);
                proptest::prop_assert_eq!(
                    serde_json::to_value(&messages).unwrap(),
                    serde_json::to_value(convert_history_for_target(&transcript, Some(&target)).unwrap()).unwrap(),
                );
                proptest::prop_assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
            }

            #[test]
            fn responses_projection_preserves_scope_and_source(
                same_provider in proptest::bool::ANY,
                same_model in proptest::bool::ANY,
                source_api in proptest::sample::select(vec![
                    None,
                    Some(crate::provider::ProviderApi::OpenAiResponses),
                    Some(crate::provider::ProviderApi::OpenAiCompletions),
                ]),
                message_id in proptest::sample::select(vec![
                    None,
                    Some("msg_native"),
                    Some("95e2df97-6454-4737-ab6c-800e96dbc1ca"),
                ]),
                stop_reason in proptest::sample::select(vec!["tool_use", "error", "aborted"]),
                boundary in proptest::sample::select(vec![
                    ContextBoundary::Branch,
                    ContextBoundary::Compaction,
                    ContextBoundary::Reset,
                ]),
            ) {
                let mut transcript = Transcript::new();
                transcript.append_user_message("old ancestor");
                let ancestor = transcript.leaf_id.clone().unwrap();
                transcript.append_agent_message("unselected branch");
                transcript.select_branch(Some(&ancestor)).unwrap();
                if boundary == ContextBoundary::Reset {
                    transcript.reset_context(Some("reset handoff".into())).unwrap();
                }
                transcript.append_user_message("retained prompt");
                let retained = transcript.leaf_id.clone().unwrap();
                transcript.append_assistant_with_switch(
                    message_id.map(Into::into),
                    None,
                    vec![
                        ContentBlock::Thinking {
                            text: "visible reasoning".into(),
                            metadata: Some(ReasoningMetadata {
                                id: Some("rs_native".into()),
                                content: vec![
                                    ReasoningMetadataContent::Summary { text: "visible reasoning".into() },
                                    ReasoningMetadataContent::Encrypted { data: "opaque reasoning".into() },
                                ],
                            }),
                        },
                        ContentBlock::Text { text: "visible answer".into() },
                        ContentBlock::ToolCall {
                            id: "phi-call".into(),
                            provider_call_id: Some("call_native".into()),
                            provider_item_id: Some("fc_native".into()),
                            name: "Read".into(),
                            args: serde_json::json!({"path": "README.md"}),
                        },
                    ],
                    if same_provider { "openai" } else { "other" }.into(),
                    source_api,
                    if same_model { "gpt-one" } else { "gpt-two" }.into(),
                    None,
                    crate::transcript::Usage::default(),
                    stop_reason.into(),
                );
                transcript.append_tool_result("phi-call", "tool output", false, None);
                if boundary == ContextBoundary::Compaction {
                    transcript.compress("active summary".into(), &retained).unwrap();
                }
                transcript.append_user_message("next prompt");
                let mut stored = serde_json::to_value(&transcript).unwrap();
                if source_api.is_none() {
                    for entry in stored["entries"].as_array_mut().unwrap() {
                        if let Some(message) = entry.get_mut("message")
                            && message["role"] == "assistant"
                        {
                            proptest::prop_assert_eq!(message.as_object_mut().unwrap().remove("api"), Some(serde_json::Value::Null));
                        }
                    }
                }
                let restored: Transcript = serde_json::from_value(stored).unwrap();
                let before = serde_json::to_value(&restored).unwrap();
                let target = HistoryTarget {
                    provider: "openai".into(),
                    api: Some(crate::provider::ProviderApi::OpenAiResponses),
                    model: "gpt-one".into(),
                };
                let request = responses_request(&restored, &target);
                let input = request["input"].as_array().unwrap();
                let wire = request.to_string();
                let matching_source = same_provider && same_model && source_api == target.api
                    && stop_reason == "tool_use";
                proptest::prop_assert!(!wire.contains("unselected branch"));
                proptest::prop_assert_eq!(wire.contains("old ancestor"), boundary == ContextBoundary::Branch);
                proptest::prop_assert_eq!(wire.contains("active summary"), boundary == ContextBoundary::Compaction);
                proptest::prop_assert_eq!(wire.contains("reset handoff"), boundary == ContextBoundary::Reset);
                for expected in ["retained prompt", "visible reasoning", "visible answer", "tool output", "next prompt"] {
                    proptest::prop_assert!(wire.contains(expected));
                }
                proptest::prop_assert_eq!(wire.contains("opaque reasoning"), matching_source);
                proptest::prop_assert_eq!(wire.contains("rs_native"), matching_source);
                let assistant = input.iter().find(|item| item["role"] == "assistant").unwrap();
                proptest::prop_assert_eq!(
                    assistant.get("id").and_then(serde_json::Value::as_str),
                    message_id.filter(|_| matching_source),
                );
                proptest::prop_assert_eq!(input.iter().any(|item| item["type"] == "reasoning"), matching_source);
                let call = input.iter().find(|item| item["type"] == "function_call").unwrap();
                let result = input.iter().find(|item| item["type"] == "function_call_output").unwrap();
                proptest::prop_assert_eq!(&call["call_id"], &result["call_id"]);
                proptest::prop_assert_eq!(call["call_id"].as_str(), Some(if matching_source { "call_native" } else { "phi-call" }));
                proptest::prop_assert_eq!(call.get("id").and_then(serde_json::Value::as_str), matching_source.then_some("fc_native"));
                proptest::prop_assert_eq!(serde_json::to_value(&restored).unwrap(), before);
            }
        }

        #[test]
        fn foreign_tool_ids_do_not_collide_with_native_ids() {
            let target = HistoryTarget {
                provider: "openai".into(),
                api: Some(crate::provider::ProviderApi::OpenAiResponses),
                model: "target".into(),
            };
            let mut transcript = Transcript::new();
            transcript.append_user_message("first");
            transcript.append_assistant_message(
                None,
                None,
                vec![ContentBlock::ToolCall {
                    id: "read:file".into(),
                    provider_call_id: None,
                    provider_item_id: None,
                    name: "Read".into(),
                    args: serde_json::json!({}),
                }],
                "foreign".into(),
                crate::provider::ProviderApi::AnthropicMessages,
                "source".into(),
                Default::default(),
                "tool_use".into(),
            );
            let projected = convert_history_for_target(&transcript, Some(&target)).unwrap();
            let Message::Assistant { content, .. } = &projected[1] else {
                panic!("missing assistant")
            };
            let AssistantContent::ToolCall(call) = &content[0] else {
                panic!("missing call")
            };
            let reserved_id = call.wire_call_id().to_owned();
            transcript.append_user_message("next");
            transcript.append_assistant_message(
                None,
                None,
                vec![ContentBlock::ToolCall {
                    id: "native-phi".into(),
                    provider_call_id: Some(reserved_id.as_str().into()),
                    provider_item_id: Some("fc_native".into()),
                    name: "Read".into(),
                    args: serde_json::json!({}),
                }],
                target.provider.as_str().into(),
                target.api.unwrap(),
                target.model.as_str().into(),
                Default::default(),
                "tool_use".into(),
            );
            let before = serde_json::to_value(&transcript).unwrap();
            let request = responses_request(&transcript, &target);
            let input = request["input"].as_array().unwrap();
            let calls = input
                .iter()
                .filter(|item| item["type"] == "function_call")
                .collect::<Vec<_>>();
            let results = input
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 2);
            assert_eq!(results.len(), 2);
            assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
            assert_eq!(calls[1]["call_id"], reserved_id);
            assert_eq!(calls[1]["id"], "fc_native");
            for (call, result) in calls.iter().zip(results) {
                assert_eq!(call["call_id"], result["call_id"]);
            }
            assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
        }

        #[test]
        fn responses_keeps_partial_reasoning_as_text() {
            for stop_reason in ["error", "aborted"] {
                let mut transcript = Transcript::new();
                transcript.append_user_message("first prompt");
                transcript.append_assistant_with_switch(
                    None,
                    None,
                    vec![ContentBlock::Thinking {
                        text: "retained partial reasoning".into(),
                        metadata: None,
                    }],
                    "openai".into(),
                    Some(crate::provider::ProviderApi::OpenAiResponses),
                    "gpt-one".into(),
                    None,
                    crate::transcript::Usage::default(),
                    stop_reason.into(),
                );
                transcript.append_user_message("continue");
                let before = serde_json::to_value(&transcript).unwrap();
                let request = responses_request(
                    &transcript,
                    &HistoryTarget {
                        provider: "openai".into(),
                        api: Some(crate::provider::ProviderApi::OpenAiResponses),
                        model: "gpt-one".into(),
                    },
                );
                assert!(request.to_string().contains("retained partial reasoning"));
                assert!(
                    !request["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["type"] == "reasoning")
                );
                assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
            }
        }

        #[test]
        fn compacted_provider_history_excludes_prefix_and_restores_retained_context() {
            let mut transcript = Transcript::new();
            transcript.append_user_message("summarized prefix");
            let root = transcript.leaf_id.clone().unwrap();
            transcript.append_user_message("retained prompt");
            let retained = transcript.leaf_id.clone().unwrap();
            transcript.append_context_message(
                crate::transcript::ContextSource::FileMention,
                ContextScope::Turn,
                "retained context",
            );
            transcript
                .compress("branch summary".into(), &retained)
                .unwrap();
            transcript.append_agent_message("new feedback");
            let restored: Transcript =
                serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
            let messages = convert_history(&restored).unwrap();
            assert!(matches!(messages.last(), Some(Message::User { content })
                if matches!(content.as_slice(), [UserContent::Text(text)] if text.text == "new feedback")));
            let history = serde_json::to_string(&messages).unwrap();
            assert!(!history.contains("summarized prefix"));
            for expected in [
                "branch summary",
                "retained prompt",
                "retained context",
                "new feedback",
            ] {
                assert!(history.contains(expected));
            }
            transcript.select_branch(Some(&root)).unwrap();
            let history = serde_json::to_string(&convert_history(&transcript).unwrap()).unwrap();
            assert!(!history.contains("branch summary"));
            assert!(history.contains("summarized prefix"));
        }

        #[test]
        fn reset_handoff_is_user_content_and_never_restores_old_messages() {
            for handoff in [
                None,
                Some("".into()),
                Some(" ".into()),
                Some("continue here".into()),
            ] {
                let mut transcript = Transcript::new();
                transcript.append_user_message("excluded history");
                transcript.reset_context(handoff.clone()).unwrap();
                let history = convert_history(&transcript).unwrap();
                match handoff.as_deref().filter(|text| !text.trim().is_empty()) {
                    Some(text) => {
                        assert!(matches!(history.as_slice(), [Message::User { .. }]));
                        let serialized = serde_json::to_string(&history).unwrap();
                        assert!(serialized.contains(text));
                        assert!(!serialized.contains("excluded history"));
                    }
                    None => assert!(history.is_empty()),
                }
            }
        }

        #[test]
        fn persisted_image_url_is_restored_to_rig_history() {
            let mut transcript = Transcript::new();
            transcript.append_user_content(vec![
                UserContentBlock::Text {
                    text: "inspect".into(),
                },
                UserContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/image.png".into(),
                        mime_type: None,
                    },
                },
            ]);
            transcript.append_context_message(
                crate::transcript::ContextSource::FileMention,
                ContextScope::Turn,
                "attached context",
            );
            let transcript: Transcript =
                serde_json::from_str(&serde_json::to_string(&transcript).unwrap()).unwrap();

            let history = convert_history(&transcript).unwrap();
            let Message::User { content } = &history[0] else {
                panic!("expected user message");
            };

            assert_eq!(content.len(), 2);
            assert!(matches!(
                content.iter().nth(1),
                Some(UserContent::Image(image))
                    if image.data.to_string() == "https://example.com/image.png"
            ));
        }
    }

    mod budget {
        use super::*;
        use rig::completion::{Document, ToolDefinition};

        fn request() -> CompletionRequest {
            CompletionRequest {
                model: None,
                preamble: None,
                chat_history: vec![Message::user("prompt")],
                documents: Vec::new(),
                tools: Vec::new(),
                temperature: None,
                max_tokens: Some(128),
                tool_choice: None,
                additional_params: None,
                output_schema: None,
                record_telemetry_content: false,
            }
        }

        #[test]
        fn request_inputs_and_output_reservation_share_one_budget() {
            for source in ["system", "tools", "documents", "dynamic", "output"] {
                let mut request = request();
                let budget = RequestBudget {
                    context_window: estimate_request(&request).unwrap() + 128,
                    reserve_tokens: 64,
                };
                assert!(!budget.exceeds(&request).unwrap());
                let text = "界".repeat(100);
                match source {
                    "system" => request.chat_history.insert(0, Message::system(text)),
                    "tools" => request.tools.push(ToolDefinition {
                        name: "tool".into(),
                        description: text,
                        parameters: serde_json::json!({"type":"object"}),
                    }),
                    "documents" => request.documents.push(Document {
                        id: "document".into(),
                        text,
                        additional_props: Default::default(),
                    }),
                    "dynamic" => request.chat_history.push(Message::system(text)),
                    "output" => request.max_tokens = Some(129),
                    _ => unreachable!(),
                }
                assert!(budget.exceeds(&request).unwrap(), "{source}");
            }
            let mut request = request();
            request.max_tokens = Some(64);
            let budget = RequestBudget {
                context_window: estimate_request(&request).unwrap() + 128,
                reserve_tokens: 129,
            };
            assert!(budget.exceeds(&request).unwrap());
        }

        #[test]
        fn image_estimates_do_not_count_base64_as_text() {
            use rig::message::{DocumentSourceKind, Image};
            let mut estimates = Vec::new();
            for size in [4, 4_000, 400_000] {
                let mut request = request();
                request.chat_history = vec![Message::User {
                    content: vec![UserContent::Image(Image {
                        data: DocumentSourceKind::Base64("a".repeat(size)),
                        media_type: Some(rig::message::ImageMediaType::PNG),
                        detail: None,
                        additional_params: None,
                    })],
                }];
                estimates.push(estimate_request(&request).unwrap());
            }
            assert!(estimates.windows(2).all(|pair| pair[0] == pair[1]));
            assert!(estimates[0] >= ESTIMATED_IMAGE_TOKENS);
        }
    }

    mod compaction {
        use super::*;
        use crate::transcript::{ContextScope, ContextSource, Usage};

        #[tokio::test]
        async fn context_edits_feed_requests_estimates_and_compaction_without_changing_history() {
            use crate::transcript::{ContextContent, ContextReplacement};
            let mut transcript = Transcript::new();
            transcript.append_user_message(&"omit this large payload".repeat(500));
            let omitted = transcript.leaf_id.clone().unwrap();
            transcript.append_user_message(&"replace this large payload".repeat(500));
            let replaced = transcript.leaf_id.clone().unwrap();
            let before_tokens = transcript.estimated_context_tokens();
            transcript.append_context_edit(&omitted, None).unwrap();
            transcript
                .append_context_edit(
                    &replaced,
                    Some(ContextReplacement {
                        content: ContextContent::Text("small replacement".into()),
                    }),
                )
                .unwrap();
            transcript.append_user_message("retained prompt");
            let raw = serde_json::to_value(&transcript).unwrap();
            assert!(transcript.estimated_context_tokens() < before_tokens);
            let target = HistoryTarget {
                provider: "test".into(),
                api: Some(crate::provider::ProviderApi::OpenAiCompletions),
                model: "test".into(),
            };
            let request = serde_json::to_string(
                &convert_history_for_target(&transcript, Some(&target)).unwrap(),
            )
            .unwrap();
            assert!(request.contains("small replacement"));
            assert!(!request.contains("large payload"));
            let summary = summarize_context(
                &transcript,
                1,
                &HookRegistry::new(),
                |conversation, _| async move {
                    assert!(conversation.contains("small replacement"));
                    assert!(!conversation.contains("large payload"));
                    assert!(!conversation.contains("retained prompt"));
                    Ok("edited summary".into())
                },
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(serde_json::to_value(&transcript).unwrap(), raw);
            transcript.compress(summary.summary, &summary.head).unwrap();
            let projected = serde_json::to_string(
                &convert_history_for_target(&transcript, Some(&target)).unwrap(),
            )
            .unwrap();
            assert!(projected.contains("edited summary"));
            assert!(projected.contains("retained prompt"));
            assert!(!projected.contains("large payload"));
        }

        fn history() -> Transcript {
            let mut transcript = Transcript::new();
            transcript.append_user_message("old prompt");
            transcript.append_context_message(
                ContextSource::FileMention,
                ContextScope::Turn,
                "attached file",
            );
            transcript.append_assistant_message(
                None,
                None,
                vec![
                    ContentBlock::Thinking {
                        text: "reasoning".into(),
                        metadata: None,
                    },
                    ContentBlock::ToolCall {
                        id: "call".into(),
                        provider_call_id: None,
                        provider_item_id: None,
                        name: "read".into(),
                        args: serde_json::json!({"path":"src/file.rs"}),
                    },
                ],
                "test".into(),
                crate::provider::ProviderApi::OpenAiCompletions,
                "test".into(),
                Usage::default(),
                "tool_use".into(),
            );
            transcript.append_tool_result(
                "call",
                &"界".repeat(MAX_TOOL_RESULT_CHARS + 1),
                false,
                None,
            );
            transcript.append_agent_message("delegated result");
            transcript.append_user_message("retained prompt");
            transcript
        }

        #[tokio::test]
        async fn compaction_summarizes_structured_context_and_iteratively_updates_summary() {
            let mut transcript = history();
            let hooks = HookRegistry::default();
            assert!(
                compact_context(&mut transcript, 1, &hooks, |text, previous| async move {
                    assert!(previous.is_none());
                    for expected in [
                        "old prompt",
                        "attached file",
                        "reasoning",
                        "src/file.rs",
                        "delegated result",
                        "[Tool result truncated]",
                    ] {
                        assert!(text.contains(expected), "missing {expected}");
                    }
                    assert!(!text.contains("retained prompt"));
                    Ok("first summary".into())
                })
                .await
                .unwrap()
            );
            let first_count = transcript.entries.len();
            transcript.append_user_message("new retained prompt");
            assert!(
                compact_context(&mut transcript, 1, &hooks, |text, previous| async move {
                    assert_eq!(previous.as_deref(), Some("first summary"));
                    assert!(text.contains("retained prompt"));
                    assert!(!text.contains("old prompt"));
                    Ok("updated summary".into())
                })
                .await
                .unwrap()
            );
            assert_eq!(transcript.entries.len(), first_count + 2);
            assert_eq!(
                transcript.effective_context().unwrap().preamble_text(),
                Some("updated summary")
            );
        }

        #[tokio::test]
        async fn compaction_after_reset_consumes_only_the_active_handoff_and_messages() {
            let mut transcript = history();
            transcript
                .reset_context(Some("handoff context".into()))
                .unwrap();
            transcript.append_user_message("new prefix");
            transcript.append_user_message("new retained prompt");
            assert!(
                compact_context(
                    &mut transcript,
                    1,
                    &HookRegistry::default(),
                    |text, previous| async move {
                        assert_eq!(previous.as_deref(), Some("handoff context"));
                        assert!(text.contains("new prefix"));
                        assert!(!text.contains("old prompt"));
                        assert!(!text.contains("new retained prompt"));
                        Ok("new summary".into())
                    }
                )
                .await
                .unwrap()
            );
            let context = transcript.effective_context().unwrap();
            assert_eq!(context.preamble_text(), Some("new summary"));
            assert_eq!(context.entries.len(), 1);
        }

        #[tokio::test]
        async fn failed_empty_or_cancelled_summary_does_not_mutate_history() {
            let mut transcript = history();
            let before = serde_json::to_value(&transcript).unwrap();
            let hooks = HookRegistry::default();
            assert!(
                compact_context(&mut transcript, 1, &hooks, |_, _| async {
                    anyhow::bail!("provider failure")
                })
                .await
                .is_err()
            );
            assert!(
                compact_context(&mut transcript, 1, &hooks, |_, _| async { Ok(" ".into()) })
                    .await
                    .is_err()
            );
            let cancelled = tokio::time::timeout(
                std::time::Duration::from_millis(1),
                compact_context(&mut transcript, 1, &hooks, |_, _| {
                    std::future::pending::<anyhow::Result<String>>()
                }),
            );
            assert!(cancelled.await.is_err());
            assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
        }
    }
}
