use phi_tui::{
    EditorState, NotifyKind, PickerState as TuiPickerState, RenderContext, SelectMenuState,
    StyledLine, TuiRuntime, Widget, WidgetHandle,
};
use std::collections::{HashMap, VecDeque};

#[cfg(test)]
use super::components::conversation::project_transcript_message;
use super::components::conversation::{self, ConversationSource, ConversationView};
use super::components::format::text_from_user_blocks;
use super::components::input::{InputSurfaceHandle, UiAction, key_bindings, mount_input_surface};
use super::components::mentions::MentionState;
use super::components::panel::activities::{ActivitySelection, BashSelector, BashView, TaskPanel};
use super::components::permission::InteractionWidget;
use super::components::workflow::WorkflowStatusView;
use super::overlays::ActiveOverlay;
use crate::commands::SlashCommand;
use crate::runtime::attachment::AgentCapabilities;
#[cfg(test)]
use crate::session::ContentBlock;
use crate::session::{AgentMessage, Session, TranscriptEntryKind};
use phi_sdk::tools::ToolDisplayFormatter;
#[cfg(test)]
use phi_sdk::transcript::{ImageSource, UserContentBlock};
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PickerKind {
    #[default]
    Generic,
    Login,
    Model,
    Resume,
    Rewind,
    SwitchSelect,
    RuntimeSwitch,
    SwitchConfigure,
    SwitchEdit {
        switch_name: String,
    },
    SwitchName,
    SwitchModel {
        switch_name: String,
    },
}

pub(crate) type PickerState = TuiPickerState<PickerKind>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCatalogNotice {
    pub(crate) kind: NotifyKind,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Running,
    Success,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) alias: Option<String>,
    pub(crate) mode: Option<phi_sdk::agent_manager::DelegationMode>,
    pub(crate) status: SubagentStatus,
}

impl From<&crate::session::SubagentChildLink> for SubagentView {
    fn from(link: &crate::session::SubagentChildLink) -> Self {
        use crate::session::SubagentOutcome;
        let status = match &link.outcome {
            SubagentOutcome::Running => SubagentStatus::Running,
            SubagentOutcome::Finished {
                is_error: false, ..
            } => SubagentStatus::Success,
            SubagentOutcome::Finished { is_error: true, .. } => SubagentStatus::Failed,
            SubagentOutcome::Cancelled => SubagentStatus::Cancelled,
            SubagentOutcome::Interrupted => SubagentStatus::Interrupted,
        };
        Self {
            id: link.child_agent_id.clone(),
            name: link.display_name(),
            alias: link.alias.clone(),
            mode: link.mode,
            status,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuKind {
    Switch,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct QueuedSubmissionView {
    pub(crate) submission_id: phi_sdk::turn::SubmissionId,
    // SDK-accepted prompt projected only after canonical prompt commit.
    pub(crate) prompt: String,
    // Compact editor representation used only for queue chrome.
    pub(crate) display: String,
    // Original expanded prompt restored when its queued submission is cancelled.
    pub(crate) editor_prompt: String,
    pub(crate) dispatch: phi_sdk::turn::SubmissionDispatch,
}

// Screen view model for interactive TUI.
// Source of truth: AgentInstance owns behavior, Transcript owns history,
// ToolRegistry owns capabilities. UiState only owns render projection and
// transient interaction state such as input, focus, menus, overlays, and live views.
pub(crate) struct UiState {
    pub(super) features: super::assembly::InteractiveFeatures,
    pub(crate) runtime: TuiRuntime<UiAction>,
    pub(crate) input_surface: InputSurfaceHandle,
    pub(crate) active_overlay: Option<WidgetHandle<ActiveOverlay>>,
    pub(crate) active_menu: Option<MenuKind>,
    pub(super) conversation: ConversationView,
    pub(super) painted_conversation: Option<ConversationSource>,
    pub(crate) editor: EditorState,
    pub(crate) mcp_status: Option<String>,
    pub(super) tasks: TaskPanel,
    pub(super) agent_recovery_pending: bool,
    pub(super) recovered_declarations: Option<crate::runtime::RuntimeDeclarations>,
    pub(crate) slash_commands: Vec<SlashCommand>,
    pub(crate) slash_menu: SelectMenuState,
    pub(crate) model_arguments: Vec<crate::commands::CommandArgument>,
    pub(crate) argument_hint: super::components::input::ArgumentHintState,
    pub(crate) queued_submissions: VecDeque<QueuedSubmissionView>,
    pub(crate) file_mentions: MentionState,
    pub(super) workspace: Option<crate::runtime::WorkspaceView>,
    pub(crate) naming_switch: bool,
    pub(crate) active_thinking_level: Option<String>,
    pub(crate) compact_enabled: Option<bool>,
    pub(crate) picker: Option<PickerState>,
    pub(crate) model_catalog_notice: Option<ModelCatalogNotice>,
    pub(crate) picker_confirming_delete: Option<String>,
    pub(crate) interaction: Option<WidgetHandle<InteractionWidget>>,
    pub(super) bash_selector: Option<WidgetHandle<BashSelector>>,
    pub(crate) subagent_views: Vec<SubagentView>,
    pub(super) activity_agents: Vec<SubagentView>,
    pub(super) bash_views: Vec<BashView>,
    pub(super) workflows: WorkflowStatusView,
    pub(super) activity_selection: Option<ActivitySelection>,
}

impl UiState {
    pub(super) fn install_capabilities(&mut self, capabilities: &AgentCapabilities) {
        let bindings = super::extensions::compile(&capabilities.extension_keybindings);
        self.runtime
            .set_key_bindings(self.features.bindings(bindings.resolver));
        if !bindings.diagnostics.is_empty() {
            self.notify(NotifyKind::Warning, bindings.diagnostics.join("\n"));
        }
        self.active_thinking_level = capabilities.thinking_level.clone();
        self.compact_enabled = capabilities.compact_enabled;
    }

    pub(super) fn interaction_lines(&self, context: &RenderContext<'_>) -> Option<Vec<StyledLine>> {
        self.interaction
            .and_then(|handle| self.runtime.widget(handle))
            .map(|widget| widget.render(context))
    }

    pub(super) fn notify(&mut self, kind: NotifyKind, message: impl Into<String>) {
        self.conversation.notify(kind, message);
    }

    pub(crate) fn reconcile_pending_submissions(
        &mut self,
        pending_ids: &[phi_sdk::turn::SubmissionId],
    ) {
        self.queued_submissions
            .retain(|input| pending_ids.contains(&input.submission_id));
    }

    pub(super) fn recover_history(&mut self, session: &Session) {
        self.conversation.recover_history(session);
        self.restore_history_interaction(session);
    }

    #[cfg(test)]
    pub(super) fn rebuild_history(&mut self, session: &Session) {
        self.conversation.rebuild_history(session);
        self.restore_history_interaction(session);
    }

    fn restore_history_interaction(&mut self, session: &Session) {
        self.subagent_views.clear();
        self.activity_agents.clear();
        for entry in session.current_branch() {
            if let TranscriptEntryKind::Message {
                message: AgentMessage::User { content },
            } = &entry.kind
                && let Some(text) = text_from_user_blocks(content)
            {
                self.editor.add_to_history(text);
            }
        }
    }

    pub(crate) fn project_run_cancelled(&mut self) {
        self.conversation.project_run_cancelled();
        self.restore_queued_submissions();
    }

    fn restore_queued_submissions(&mut self) {
        let mut queued = self
            .queued_submissions
            .drain(..)
            .map(|input| input.editor_prompt)
            .collect::<Vec<_>>()
            .join("\n\n");
        if queued.is_empty() {
            return;
        }
        let draft = self.editor.text();
        if !draft.is_empty() {
            queued.push_str("\n\n");
            queued.push_str(&draft);
        }
        self.editor.replace_text(queued);
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
        self.picker_confirming_delete = None;
        self.active_menu = None;
    }

    pub(crate) fn open_picker(&mut self, kind: MenuKind, picker: PickerState) {
        self.active_menu = Some(kind);
        self.picker = Some(picker);
    }
}

impl Default for UiState {
    fn default() -> Self {
        let mut runtime = TuiRuntime::default();
        runtime.set_key_bindings(key_bindings());
        let input_surface = mount_input_surface(&mut runtime);
        input_surface.activate(
            &mut runtime,
            super::components::input::InputSurfaceKind::Editor,
            phi_tui::InputContext::Editor,
        );
        Self {
            features: super::assembly::InteractiveFeatures::default(),
            runtime,
            input_surface,
            active_overlay: None,
            active_menu: None,
            conversation: ConversationView::default(),
            painted_conversation: None,
            editor: EditorState::default(),
            mcp_status: None,
            tasks: TaskPanel::default(),
            agent_recovery_pending: false,
            recovered_declarations: None,
            slash_commands: Vec::new(),
            slash_menu: SelectMenuState::default(),
            argument_hint: Default::default(),
            queued_submissions: VecDeque::new(),
            file_mentions: MentionState::default(),
            workspace: None,
            naming_switch: false,
            active_thinking_level: None,
            compact_enabled: None,
            picker: None,
            model_catalog_notice: None,
            model_arguments: Vec::new(),
            picker_confirming_delete: None,
            interaction: None,
            bash_selector: None,
            subagent_views: Vec::new(),
            activity_agents: Vec::new(),
            bash_views: Vec::new(),
            workflows: WorkflowStatusView::default(),
            activity_selection: None,
        }
    }
}

pub(crate) fn state_from_session(
    features: super::assembly::InteractiveFeatures,
    session: &Session,
    capabilities: &AgentCapabilities,
    tool_display: &dyn ToolDisplayFormatter,
    commands: Vec<SlashCommand>,
    tool_display_names: HashMap<String, String>,
) -> UiState {
    let mut state = UiState {
        features,
        slash_commands: commands,
        file_mentions: MentionState::for_directory(std::path::Path::new(
            session.working_dir.as_str(),
        )),
        conversation: ConversationView::with_resources(session, tool_display_names),
        ..UiState::default()
    };
    state.install_capabilities(capabilities);
    state.input_surface.activate(
        &mut state.runtime,
        super::components::input::InputSurfaceKind::Editor,
        phi_tui::InputContext::Editor,
    );
    state.restore_history_interaction(session);
    state.conversation.project_history(session, tool_display);
    state
}

pub(super) fn push_user_message(state: &mut UiState, prompt: &str) {
    conversation::push_user_message(&mut state.conversation, prompt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_delivery_is_not_displayed_but_parent_response_is() {
        let mut state = UiState::default();
        let formatter = super::super::components::format::tool_display_formatter;
        let mut calls = HashMap::new();
        let outcome = AgentMessage::Agent {
            content: vec![ContentBlock::Text {
                text: "private child result".into(),
            }],
        };
        project_transcript_message(&mut state.conversation, &outcome, &formatter, &mut calls);
        project_transcript_message(&mut state.conversation, &outcome, &formatter, &mut calls);
        assert!(state.conversation.blocks.is_empty());
        let parent = AgentMessage::Assistant {
            id: None,
            turn_id: None,
            content: vec![ContentBlock::Text {
                text: "parent response".into(),
            }],
            provider: "test".into(),
            api: None,
            model: "test".into(),
            switch: None,
            usage: Default::default(),
            stop_reason: "stop".into(),
        };
        project_transcript_message(&mut state.conversation, &parent, &formatter, &mut calls);
        assert_eq!(state.conversation.blocks.len(), 1);
        assert!(
            matches!(&state.conversation.blocks[0].block.body, phi_tui::BlockBody::Text(text) if text == "parent response")
        );
    }

    #[test]
    fn replay_tool_result_only_updates_matching_call() {
        let mut state = UiState::default();
        let formatter = super::super::components::format::tool_display_formatter;
        let mut tool_calls = HashMap::new();
        let call = |id: &str| ContentBlock::ToolCall {
            id: id.into(),
            provider_call_id: Some(id.into()),
            provider_item_id: None,
            name: "Read".into(),
            args: serde_json::json!({}),
        };
        project_transcript_message(
            &mut state.conversation,
            &AgentMessage::Assistant {
                id: None,
                turn_id: None,
                content: vec![call("first"), call("second")],
                provider: "test".into(),
                api: None,
                model: "test".into(),
                switch: None,
                usage: Default::default(),
                stop_reason: "stop".into(),
            },
            &formatter,
            &mut tool_calls,
        );
        assert_eq!(
            state.conversation.blocks[0].tool_call_id.as_deref(),
            Some("first")
        );
        assert_eq!(
            state.conversation.blocks[1].tool_call_id.as_deref(),
            Some("second")
        );
        let result = |id: &str, text: &str| AgentMessage::ToolResult {
            tool_call_id: id.into(),
            provider_call_id: None,
            provider_item_id: None,
            tool_name: "Read".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
            is_error: false,
            details: None,
        };
        project_transcript_message(
            &mut state.conversation,
            &result("unknown", "orphan"),
            &formatter,
            &mut tool_calls,
        );
        project_transcript_message(
            &mut state.conversation,
            &result("first", "matched"),
            &formatter,
            &mut tool_calls,
        );
        assert!(
            matches!(&state.conversation.blocks[0].block.body, phi_tui::BlockBody::Output(text) if text == "matched")
        );
        assert!(
            matches!(&state.conversation.blocks[1].block.body, phi_tui::BlockBody::Output(text) if text.is_empty())
        );
        assert!(
            matches!(&state.conversation.blocks[2].block.body, phi_tui::BlockBody::Text(text) if text == "orphan")
        );
    }

    #[test]
    fn usage_view_initializes_from_history_and_accumulates_live_usage() {
        let mut session = crate::session::new(
            "openai-codex",
            "gpt-test",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.append_assistant_message(
            None,
            None,
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
            "openai-codex".into(),
            phi_sdk::providers::ProviderApi::OpenAiCodexResponses,
            "gpt-test".into(),
            phi_sdk::transcript::Usage {
                input: 100,
                output: 20,
                total_tokens: 125,
                cost: 0.0,
                cache_read: 5,
                cache_write: 0,
            },
            "stop".into(),
        );
        let mut view = ConversationView::with_resources(&session, HashMap::new());
        view.record_usage(&phi_sdk::PhiUsage {
            input_tokens: 50,
            output_tokens: 10,
            cache_read: 2,
            cost: 0.25,
            ..Default::default()
        });

        assert_eq!(view.usage.input, 150);
        assert_eq!(view.usage.output, 30);
        assert_eq!(view.usage.cache_read, 7);
        assert_eq!(view.usage.cost, 0.25);
    }

    #[test]
    fn projects_mixed_user_content_as_joined_blocks() {
        let mut state = UiState::default();
        let content = vec![
            UserContentBlock::Text {
                text: "inspect".into(),
            },
            UserContentBlock::Image {
                source: ImageSource::Url {
                    url: "https://example.com/image.png".into(),
                    mime_type: Some("image/png".into()),
                },
            },
        ];

        conversation::push_user_content(&mut state.conversation, &content);

        assert_eq!(state.conversation.blocks.len(), 2);
        assert_eq!(
            state.conversation.blocks[0].group_id,
            state.conversation.blocks[1].group_id
        );
        assert!(matches!(
            &state.conversation.blocks[0].block.body,
            phi_tui::BlockBody::Text(text) if &**text == "inspect"
        ));
        assert!(matches!(
            state.conversation.blocks[1].block.body,
            phi_tui::BlockBody::Image(_)
        ));
    }

    #[test]
    fn projects_skill_and_custom_command_as_collapsed_blocks() {
        let mut state = UiState::default();

        let skill = "<skill name=\"commit\" location=\"/skills/commit/SKILL.md\">\ninstructions\n</skill>\n\nfix this";
        let command = "<command name=\"review\">\nreview src/main.rs\n</command>";
        push_user_message(&mut state, skill);
        push_user_message(&mut state, command);

        assert_eq!(state.conversation.blocks[0].block.title, "[skill] commit");
        assert!(!state.conversation.blocks[0].block.is_expanded());
        assert!(matches!(
            state.conversation.blocks[0].block.body,
            phi_tui::BlockBody::Prompt(_)
        ));
        assert!(matches!(
            state.conversation.blocks[1].block.body,
            phi_tui::BlockBody::Text(ref text) if text == "fix this"
        ));
        assert_eq!(state.conversation.blocks[2].block.title, "[command] review");
        assert!(!state.conversation.blocks[2].block.is_expanded());
    }
}
