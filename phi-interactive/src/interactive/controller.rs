use crate::runtime::attachment::{AgentAttachment, AgentCapabilities};
use crate::runtime::preparation::BranchNavigation;
use crate::session::Session;
use phi_sdk::agent_instance::AgentInstanceStatus;

use super::commands;
use super::components::format::text_from_user_blocks;
use super::components::panel::mcp;
use super::components::picker::{model, session, switch};
use super::state::{UiState, state_from_session};
use super::{materialize_transcript_snapshot, release_transcript_snapshot};
use crate::commands::{BuiltinCommand, CommandInvocation, ModelCommand};

pub(super) struct InteractiveController<'a> {
    pub memory: Option<&'a super::MemoryAdministration>,
    pub navigation: Option<&'a crate::runtime::preparation::SessionWorkflows>,
    pub mcp_auth: Option<&'a crate::config::McpAuthDiagnostics>,
    pub mcp_read_only: bool,
    pub authentication: Option<&'a mut commands::Authentication>,
    pub models: &'a mut model::ModelPicker,
    pub switches: &'a switch::SwitchPicker,
    pub sessions: &'a session::SessionPicker,
    pub agent_instance: &'a mut AgentAttachment,
    pub session: &'a mut Session,
    pub capabilities: &'a mut AgentCapabilities,
    pub extension_tasks: Option<&'a super::extensions::ExtensionTaskQueue>,
    pub state: &'a mut UiState,
}

impl InteractiveController<'_> {
    fn require_idle(&mut self, operation: &str) -> bool {
        if !matches!(
            self.agent_instance.snapshot().status,
            AgentInstanceStatus::Idle | AgentInstanceStatus::Failed
        ) {
            self.state.notify(
                phi_tui::NotifyKind::Warning,
                format!("Cannot {operation} while the agent is running."),
            );
            return false;
        }
        true
    }

    pub async fn cycle_switch(&mut self, direction: i32) {
        let names = match self.switches.names(self.agent_instance).await {
            Ok(names) => names,
            Err(error) => {
                self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("failed to load Switches: {error}"),
                );
                return;
            }
        };
        self.select_switch(switch::cycle_switch_name(
            &names,
            self.session.current_switch.as_str(),
            direction,
        ))
        .await;
    }

    pub(super) async fn select_picker_value(&mut self, value: String) {
        if !self
            .state
            .features
            .action(&super::components::input::UiAction::Pick(value.clone()))
        {
            return;
        }
        if let Some(provider) = value.strip_prefix("login:") {
            if let Some(authentication) = self.authentication.as_mut() {
                authentication.execute(provider, self.state).await;
            }
        } else if value.starts_with("switch:") {
            if let Some(intent) = self.switches.pick(&value, self.state, self.session) {
                self.apply_switch_intent(intent).await;
            }
        } else if let Some(id) = value.strip_prefix("resume:") {
            self.resume_session(id).await;
        } else if let Some(id) = value.strip_prefix("rewind:") {
            self.rewind_session(id).await;
        } else {
            self.select_model(value).await;
        }
    }

    pub(super) async fn handle_picker_action(
        &mut self,
        action: super::components::input::UiAction,
    ) {
        if !self.state.features.action(&action) {
            return;
        }
        if let super::components::input::UiAction::DeletePick(value) = &action
            && let Some(id) = value.strip_prefix("resume:")
        {
            self.sessions
                .delete(id, self.agent_instance, self.state, self.session)
                .await;
        } else if let Some(intent) = self.switches.action(action, self.state, self.session) {
            self.apply_switch_intent(intent).await;
        }
    }

    async fn apply_switch_intent(&mut self, intent: switch::SwitchIntent) {
        match intent {
            switch::SwitchIntent::Updated => {}
            switch::SwitchIntent::Select(name) => self.select_switch(name).await,
            switch::SwitchIntent::SelectThenConfigure(name) => {
                self.select_switch(name).await;
                self.switches.open_configuration(self.state);
            }
        }
    }

    pub async fn select_switch(&mut self, name: String) {
        if !self.require_idle("switch modes") {
            return;
        }
        self.configure_runtime(crate::runtime::preparation::RuntimeSelection::Switch { name })
            .await;
    }

    pub async fn select_model(&mut self, selection: String) {
        if selection.starts_with("model-incomplete:") {
            self.state.notify(
                phi_tui::NotifyKind::Warning,
                "Discovered model metadata is incomplete. Add a descriptor to ~/.phi/models.json.",
            );
            return;
        }
        if !self.require_idle("change models") {
            return;
        }
        self.configure_runtime(crate::runtime::preparation::RuntimeSelection::Model { selection })
            .await;
    }

    pub async fn adjust_thinking_level(&mut self, direction: i32) {
        if !self.require_idle("change thinking level") {
            return;
        }
        let Some(level) = adjacent_thinking_level(
            &self.capabilities.supported_thinking_levels,
            self.capabilities.thinking_level.as_deref(),
            direction,
        ) else {
            return;
        };
        self.configure_runtime(
            crate::runtime::preparation::RuntimeSelection::ThinkingLevel { level },
        )
        .await;
    }

    pub async fn reload_resources(&mut self) {
        if !self.require_idle("reload resources") {
            return;
        }

        let accepted = match self.agent_instance.reload_resources().await {
            Ok(result) => result,
            Err(error) => {
                self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("resource reload failed: {error}"),
                );
                return;
            }
        };
        install_runtime_update(
            self.capabilities,
            self.state,
            self.session,
            accepted,
            "reload",
        );
        self.state
            .notify(phi_tui::NotifyKind::Info, "Resources reloaded");
    }

    pub async fn resume_session(&mut self, id: &str) {
        let result = self
            .sessions
            .selection
            .resume_session(self.agent_instance, id)
            .await;
        let transition = match result {
            Ok(result) => result,
            Err(error) => {
                self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("resume failed: {error}"),
                );
                return;
            }
        };
        self.apply_session_transition(
            transition.attachment,
            transition.accepted,
            "resumed session",
        )
        .await;
    }

    async fn apply_session_transition(
        &mut self,
        attachment: AgentAttachment,
        accepted: crate::runtime::RuntimeView,
        notice: &str,
    ) {
        if attachment.id() == self.agent_instance.id() {
            return;
        }
        *self.agent_instance = attachment;
        *self.session = accepted.session;
        self.restore_session_state(accepted.declarations).await;
        if let Some(tasks) = self.extension_tasks {
            tasks.ready_hook_notices(self.agent_instance.clone());
            let _ = self.agent_instance.extension_notify("info", notice).await;
        }
        self.state.notify(phi_tui::NotifyKind::Info, notice);
    }

    pub async fn execute_command(&mut self, prompt: &str, command: CommandInvocation) {
        if !self.state.features.command(&command) {
            self.state.notify(
                phi_tui::NotifyKind::Warning,
                "This command is unavailable in this interface.",
            );
            return;
        }
        let command = match command {
            CommandInvocation::Builtin { command, .. } => command,
            CommandInvocation::Extension { name, args } => {
                if let Some(tasks) = self.extension_tasks {
                    tasks.run_command(name, args, self.agent_instance.clone());
                }
                return;
            }
        };
        if self.extension_tasks.is_some()
            && let Ok(Some(reason)) = self.agent_instance.extension_before_command(prompt).await
        {
            self.state.notify(phi_tui::NotifyKind::Warning, reason);
            let _ = self
                .agent_instance
                .extension_after_command(prompt, true)
                .await;
            return;
        }
        match command {
            BuiltinCommand::Model(ModelCommand::Select) => {
                self.models
                    .open(self.agent_instance, self.state, self.session)
                    .await;
            }
            BuiltinCommand::Model(ModelCommand::Update) => {
                self.models.update_catalog(self.state);
            }
            BuiltinCommand::Tasks => super::overlays::open_tasks(self.state),
            BuiltinCommand::Workflow => {
                if self
                    .state
                    .slash_commands
                    .iter()
                    .any(|command| command.name == "workflow")
                {
                    self.state.notify(
                        phi_tui::NotifyKind::Info,
                        "Use /workflow <task> to start a workflow",
                    );
                } else {
                    self.state.notify(
                        phi_tui::NotifyKind::Warning,
                        "Workflow is unavailable; enable agent.workflow in settings",
                    );
                }
            }
            BuiltinCommand::Switch(value) => {
                if value.is_empty() {
                    self.switches
                        .open(self.agent_instance, self.state, self.session)
                        .await;
                } else {
                    self.select_switch(value).await;
                }
            }
            BuiltinCommand::NewSession => self.start_new_session().await,
            BuiltinCommand::Reload => self.reload_resources().await,
            BuiltinCommand::ResumePicker(query) => {
                self.sessions
                    .open(self.agent_instance, &query, self.state, self.session)
                    .await
            }
            BuiltinCommand::Login(command) => {
                if let Some(authentication) = self.authentication.as_mut() {
                    authentication.execute(&command, self.state).await;
                }
            }
            BuiltinCommand::McpAuth(command) => {
                if let Some(auth) = self.mcp_auth {
                    commands::handle_mcp_auth_command(&command, auth, self.state);
                }
            }
            BuiltinCommand::Mcp => self.open_mcp_picker().await,
            BuiltinCommand::RewindPicker => match self.agent_instance.transcript().await {
                Ok(transcript) => {
                    materialize_transcript_snapshot(self.session, transcript);
                    let annotations = self
                        .memory
                        .map(|memory| memory.rewind_annotations(self.session.id.as_str()))
                        .unwrap_or_default();
                    session::open_rewind_picker(self.state, self.session, &annotations);
                    release_transcript_snapshot(self.session);
                }
                Err(error) => self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("failed to load rewind history: {error}"),
                ),
            },
            BuiltinCommand::MemoryCheck(command) => {
                if let Some(memory) = self.memory {
                    commands::handle_memory_check_command(&command, memory, self.state);
                }
            }
            BuiltinCommand::MemoryRefresh => {
                if let Some(memory) = self.memory {
                    commands::handle_memory_refresh_command(memory, self.state);
                }
            }
            BuiltinCommand::MemoryReview(command) => {
                if let Some(memory) = self.memory {
                    commands::handle_memory_review_command(command, memory, self.state);
                }
            }
        }
        if self.extension_tasks.is_some() {
            let _ = self
                .agent_instance
                .extension_after_command(prompt, true)
                .await;
        }
    }

    async fn start_new_session(&mut self) {
        match self
            .sessions
            .selection
            .new_session(self.agent_instance)
            .await
        {
            Ok(transition) => {
                self.apply_session_transition(
                    transition.attachment,
                    transition.accepted,
                    "new session started",
                )
                .await
            }
            Err(error) => self.state.notify(
                phi_tui::NotifyKind::Error,
                format!("new session failed: {error}"),
            ),
        }
    }

    pub async fn open_memory_anchor(&mut self, anchor_id: &str) {
        if !self.require_idle("open a memory anchor") {
            return;
        }
        let Some(anchor) = super::overlays::memory_review(self.state)
            .and_then(|review| review.anchors.iter().find(|anchor| anchor.id == anchor_id))
            .cloned()
        else {
            self.state.notify(
                phi_tui::NotifyKind::Warning,
                format!("memory anchor not found: {anchor_id}"),
            );
            return;
        };
        let Some(navigation) = self.navigation else {
            return;
        };
        let result = navigation
            .jump_anchor(
                self.agent_instance.id(),
                &anchor.session_id,
                &anchor.session_entry_id,
            )
            .await;
        let crate::runtime::preparation::AcceptedTransition {
            attachment,
            accepted,
            navigation_error,
        } = match result {
            Ok(result) => result,
            Err(error) => {
                self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("memory anchor jump failed: {error}"),
                );
                return;
            }
        };
        super::overlays::close_memory_review(self.state);
        *self.agent_instance = attachment;
        self.apply_navigation_session(accepted).await;
        if let Some(error) = navigation_error {
            self.state.notify(
                phi_tui::NotifyKind::Error,
                format!("Session selected but anchor navigation failed: {error}"),
            );
            return;
        }
        self.state.notify(
            phi_tui::NotifyKind::Info,
            "Jumped to selected memory anchor",
        );
        if self.extension_tasks.is_some() {
            let _ = self
                .agent_instance
                .extension_notify("info", "Jumped to selected memory anchor")
                .await;
        }
    }

    pub async fn rewind_session(&mut self, entry_id: &str) {
        if !self.require_idle("rewind the session") || !self.apply_branch_navigation(entry_id).await
        {
            return;
        }
        self.state
            .notify(phi_tui::NotifyKind::Info, "Navigated to selected point");
        if self.extension_tasks.is_some() {
            let _ = self
                .agent_instance
                .extension_notify("info", "Navigated to selected point")
                .await;
        }
    }

    async fn apply_branch_navigation(&mut self, entry_id: &str) -> bool {
        let accepted = match self
            .agent_instance
            .navigate_branch(BranchNavigation::Rewind {
                entry_id: entry_id.into(),
            })
            .await
        {
            Ok(result) => result,
            Err(error) => {
                self.state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("branch navigation failed: {error}"),
                );
                return false;
            }
        };
        let editor_text = accepted
            .session
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .and_then(|entry| rewind_editor_text(&entry.kind));
        self.apply_navigation_session(accepted).await;
        if let Some(text) = editor_text {
            self.state.editor.set_text(text);
        }
        true
    }

    async fn apply_navigation_session(&mut self, accepted: crate::runtime::RuntimeView) {
        *self.session = accepted.session;
        self.restore_session_state(accepted.declarations).await;
        if let Some(error) = accepted.snapshot.persistence_error {
            self.state.notify(
                phi_tui::NotifyKind::Warning,
                format!("navigation committed but checkpoint failed: {error}"),
            );
        }
    }

    async fn open_mcp_picker(&mut self) {
        match self.agent_instance.mcp_servers().await {
            Ok(servers) => super::overlays::open_mcp(
                self.state,
                mcp::McpPanelState {
                    servers: mcp::panel_servers(servers),
                    read_only: self.mcp_read_only,
                    ..Default::default()
                },
            ),
            Err(error) => self.state.notify(
                phi_tui::NotifyKind::Error,
                format!("failed to load MCP servers: {error}"),
            ),
        }
    }

    async fn configure_runtime(
        &mut self,
        selection: crate::runtime::preparation::RuntimeSelection,
    ) {
        let thinking_selection = matches!(
            selection,
            crate::runtime::preparation::RuntimeSelection::ThinkingLevel { .. }
        );
        match self.agent_instance.configure_runtime(selection).await {
            Ok(accepted) => {
                install_runtime_update(
                    self.capabilities,
                    self.state,
                    self.session,
                    accepted,
                    "runtime",
                );
                if !thinking_selection {
                    self.state.editor.clear();
                }
            }
            Err(error) => self.state.notify(
                phi_tui::NotifyKind::Error,
                format!("runtime configuration failed: {error}"),
            ),
        }
    }

    async fn restore_session_state(&mut self, declarations: crate::runtime::RuntimeDeclarations) {
        let declarations = self.state.features.filter(declarations);
        *self.capabilities = declarations.capabilities;
        let commands = declarations.commands;
        let tool_names = if self.state.features.mcp {
            mcp::tool_display_name_projection_for_session(self.session, self.agent_instance).await
        } else {
            Ok(Default::default())
        };
        let mut warnings = Vec::new();
        if let Err(error) = &tool_names {
            warnings.push(format!("failed to load MCP tool titles: {error}"));
        }
        *self.state = state_from_session(
            self.state.features,
            self.session,
            self.capabilities,
            &super::components::format::tool_display_formatter,
            commands,
            tool_names.unwrap_or_default(),
        );
        release_transcript_snapshot(self.session);
        for warning in warnings {
            self.state.notify(phi_tui::NotifyKind::Warning, warning);
        }
    }
}

pub(super) fn install_runtime_update(
    capabilities: &mut AgentCapabilities,
    state: &mut UiState,
    session: &mut Session,
    accepted: crate::runtime::RuntimeView,
    operation: &str,
) {
    *session = accepted.session;
    release_transcript_snapshot(session);
    install_runtime_declarations(capabilities, state, accepted.declarations);
    if let Some(error) = accepted.snapshot.persistence_error {
        state.notify(
            phi_tui::NotifyKind::Warning,
            format!("{operation} committed but checkpoint failed: {error}"),
        );
    }
}

pub(super) fn install_runtime_declarations(
    capabilities: &mut AgentCapabilities,
    state: &mut UiState,
    declarations: crate::runtime::RuntimeDeclarations,
) {
    let declarations = state.features.filter(declarations);
    *capabilities = declarations.capabilities;
    state.slash_commands = declarations.commands;
    state.install_capabilities(capabilities);
}

fn adjacent_thinking_level(
    supported: &[phi_sdk::models::ThinkingLevel],
    current: Option<&str>,
    direction: i32,
) -> Option<phi_sdk::models::ThinkingLevel> {
    use phi_sdk::models::ThinkingLevel;

    if supported.len() <= 1 || direction == 0 {
        return None;
    }
    let current = current
        .and_then(ThinkingLevel::parse)
        .unwrap_or(ThinkingLevel::Off);
    let index = supported
        .iter()
        .position(|level| *level >= current)
        .unwrap_or(supported.len() - 1);
    let next = index.checked_add_signed(direction.signum() as isize)?;
    supported.get(next).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::SlashCommand;

    #[test]
    fn thinking_adjustment_uses_only_adjacent_supported_levels_without_wrapping() {
        use phi_sdk::models::ThinkingLevel;

        for mask in 0..(1 << ThinkingLevel::ALL.len()) {
            let supported = ThinkingLevel::ALL
                .into_iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, level)| level)
                .collect::<Vec<_>>();
            for (index, current) in supported.iter().enumerate() {
                for direction in [-1, 1] {
                    let expected = index
                        .checked_add_signed(direction as isize)
                        .and_then(|next| supported.get(next))
                        .copied();
                    assert_eq!(
                        adjacent_thinking_level(&supported, Some(current.as_str()), direction),
                        expected,
                    );
                }
            }
        }
        assert_eq!(
            adjacent_thinking_level(&ThinkingLevel::ALL, None, 1),
            Some(ThinkingLevel::Minimal)
        );
        assert_eq!(
            adjacent_thinking_level(&ThinkingLevel::ALL, Some("high"), 0),
            None
        );
        assert_eq!(
            adjacent_thinking_level(
                &[ThinkingLevel::Off, ThinkingLevel::High],
                Some("medium"),
                -1
            ),
            Some(ThinkingLevel::Off)
        );
    }

    #[test]
    fn runtime_update_keeps_ui_state() {
        use super::super::assembly::InteractiveFeatures;
        let operation = "runtime";
        for (features, command_count) in [
            (InteractiveFeatures::all_enabled(), 2),
            (InteractiveFeatures::default(), 1),
        ] {
            for persistence_error in [None, Some("disk unavailable".to_string())] {
                let mut session =
                    crate::session::new("local", "old", 128_000, &std::env::current_dir().unwrap());
                let mut accepted = session.clone();
                accepted.model = "accepted".into();
                accepted.append_user_message("canonical history");
                let snapshot = phi_sdk::agent_instance::AgentInstanceSnapshot {
                    id: phi_sdk::agent_instance::AgentInstanceId::new(accepted.id.as_str()),
                    definition: None,
                    working_dir: Default::default(),
                    status: AgentInstanceStatus::Idle,
                    active_turn_id: None,
                    live_run: None,
                    progress: None,
                    bash_executions: Vec::new(),
                    pending_interactions: Vec::new(),
                    latest_error: None,
                    revision: 0,
                };
                let mut state = UiState {
                    features,
                    ..UiState::default()
                };
                state.editor.set_text("unsent draft");
                state.active_menu = Some(super::super::state::MenuKind::Other);
                state.notify(phi_tui::NotifyKind::Info, "retained notice");
                let block_id = state.conversation.blocks[0].id;
                let mut capabilities = AgentCapabilities::default();
                install_runtime_update(
                    &mut capabilities,
                    &mut state,
                    &mut session,
                    crate::runtime::RuntimeView {
                        snapshot: crate::runtime::AgentSessionSnapshot {
                            instance: snapshot,
                            runtime_generation_id: "accepted".into(),
                            persistence_error: persistence_error.clone(),
                        },
                        session: accepted,
                        declarations: crate::runtime::RuntimeDeclarations {
                            generation_id: "accepted".into(),
                            capabilities: AgentCapabilities {
                                compact_enabled: Some(false),
                                ..Default::default()
                            },
                            commands: vec![
                                SlashCommand::builtin("reload", "Reload"),
                                SlashCommand::builtin("login", "Login"),
                            ],
                        },
                    },
                    operation,
                );
                assert_eq!(session.model, "accepted");
                assert!(session.entries.is_empty());
                assert_eq!(state.editor.text(), "unsent draft");
                assert_eq!(
                    state.active_menu,
                    Some(super::super::state::MenuKind::Other)
                );
                assert_eq!(state.conversation.blocks[0].id, block_id);
                assert_eq!(state.compact_enabled, Some(false));
                assert_eq!(state.slash_commands.len(), command_count);
                assert_eq!(
                    state.conversation.blocks.len(),
                    1 + usize::from(persistence_error.is_some())
                );
                if let Some(error) = persistence_error {
                    assert!(matches!(
                        &state.conversation.blocks.last().unwrap().block.body,
                        phi_tui::BlockBody::Notify(notice)
                            if notice.kind == phi_tui::NotifyKind::Warning
                                && notice.message == format!("{operation} committed but checkpoint failed: {error}")
                    ));
                }
            }
        }
    }

    #[test]
    fn accepted_declarations_replace_dispatch_and_display_without_resetting_drafts() {
        let binding = phi_sdk::extension::RegisteredKeybinding {
            id: serde_json::from_value(serde_json::json!({"runtime": 1, "binding": 1})).unwrap(),
            context: "picker.rewind".into(),
            key: "ctrl+e".into(),
        };
        let mut capabilities = AgentCapabilities {
            thinking_level: Some("old".into()),
            compact_enabled: Some(true),
            extension_keybindings: vec![binding.clone()],
            ..Default::default()
        };
        let mut state = state_from_session(
            super::super::assembly::InteractiveFeatures {
                extensions: true,
                ..Default::default()
            },
            &crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap()),
            &capabilities,
            &super::super::components::format::tool_display_formatter,
            Vec::new(),
            Default::default(),
        );
        assert_eq!(state.active_thinking_level.as_deref(), Some("old"));
        assert_eq!(state.compact_enabled, Some(true));
        assert!(
            state
                .runtime
                .key_bindings()
                .resolve(
                    phi_tui::InputContext::custom("picker.rewind"),
                    &phi_tui::KeyInput::Ctrl('e'),
                )
                .is_some()
        );
        state.editor.set_text("unsent draft");
        let commands = vec![SlashCommand::builtin("reload", "Reload")];
        install_runtime_declarations(
            &mut capabilities,
            &mut state,
            crate::runtime::RuntimeDeclarations {
                generation_id: "accepted".into(),
                capabilities: AgentCapabilities::default(),
                commands,
            },
        );
        assert!(capabilities.thinking_level.is_none());
        assert!(capabilities.extension_keybindings.is_empty());
        assert!(
            state
                .runtime
                .key_bindings()
                .resolve(
                    phi_tui::InputContext::custom("picker.rewind"),
                    &phi_tui::KeyInput::Ctrl('e')
                )
                .is_none()
        );
        assert!(state.active_thinking_level.is_none());
        assert!(state.compact_enabled.is_none());
        assert_eq!(state.slash_commands.len(), 1);

        assert_eq!(state.editor.text(), "unsent draft");
        install_runtime_declarations(
            &mut capabilities,
            &mut state,
            crate::runtime::RuntimeDeclarations {
                generation_id: "next".into(),
                capabilities: AgentCapabilities {
                    compact_enabled: Some(false),
                    extension_keybindings: vec![phi_sdk::extension::RegisteredKeybinding {
                        key: "ctrl+j".into(),
                        ..binding
                    }],
                    agent_diagnostics: vec!["candidate warning".into()],
                    ..Default::default()
                },
                commands: Vec::new(),
            },
        );
        assert_eq!(state.compact_enabled, Some(false));
        for (key, present) in [('e', false), ('j', true)] {
            assert_eq!(
                state
                    .runtime
                    .key_bindings()
                    .resolve(
                        phi_tui::InputContext::custom("picker.rewind"),
                        &phi_tui::KeyInput::Ctrl(key),
                    )
                    .is_some(),
                present,
            );
        }
        assert!(state.conversation.blocks.is_empty());
        assert_eq!(state.editor.text(), "unsent draft");
    }
}

fn rewind_editor_text(kind: &crate::session::TranscriptEntryKind) -> Option<String> {
    let crate::session::TranscriptEntryKind::Message {
        message: crate::session::AgentMessage::User { content },
    } = kind
    else {
        return None;
    };
    Some(text_from_user_blocks(content).unwrap_or_default())
}
