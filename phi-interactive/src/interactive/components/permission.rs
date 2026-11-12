use phi_tui::{
    BlockStatus, Choice, ChoiceAction, InputContext, InputOutcome, KeyInput, MountOptions,
    NotifyKind, Panel, RenderArea, RenderContext, StyledLine, ThemeColor, Widget,
};

use super::super::state::UiState;
use super::input::UiAction;
use super::panel::mcp::ElicitationState;
use crate::runtime::attachment::AgentAttachment;
use crate::session::Session;
use phi_sdk::interaction::{
    InteractionEnvelope, InteractionRequest, InteractionResponse, PermissionDecision,
};

#[derive(Debug, Clone)]
pub(crate) struct ConfirmState {
    pub(crate) title: String,
    pub(crate) message: String,
    pub(crate) choice: Choice,
    pub(crate) help: String,
}

impl Widget for ConfirmState {
    type Action = UiAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let width = context.area.width;
        let child = context.with_area(RenderArea {
            width: width.saturating_sub(4).min(48),
            ..context.area
        });
        let inner_width = child.area.width.saturating_sub(4);
        let mut lines = vec![
            StyledLine::new(
                phi_tui::text::truncate(
                    &format!("⚠️ {}: {}", self.title, self.message),
                    inner_width as usize,
                ),
                ThemeColor::Warning,
            )
            .bold(),
        ];
        lines.extend(self.choice.render(&child.with_area(RenderArea {
            width: inner_width,
            ..child.area
        })));
        lines.push(StyledLine::new(
            phi_tui::text::truncate(&self.help, inner_width as usize),
            ThemeColor::Dim,
        ));
        Panel {
            title: String::new(),
            lines,
            max_height: 5,
            border_color: ThemeColor::Dim,
        }
        .render(&child)
        .into_iter()
        .take(context.area.height.unwrap_or(usize::MAX))
        .map(|line| line.prepend_segment("   ", ThemeColor::Dim).fit(width))
        .collect()
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<UiAction> {
        match self.choice.handle_input(key) {
            InputOutcome::Action(ChoiceAction::Selected(1)) => {
                InputOutcome::Action(UiAction::PermissionAllowAlways)
            }
            InputOutcome::Action(ChoiceAction::Selected(2) | ChoiceAction::Cancelled) => {
                InputOutcome::Action(UiAction::PermissionDeny)
            }
            InputOutcome::Action(ChoiceAction::Selected(_)) => {
                InputOutcome::Action(UiAction::PermissionAllowOnce)
            }
            InputOutcome::Consumed => InputOutcome::Consumed,
            InputOutcome::Ignored => InputOutcome::Ignored,
        }
    }
}

pub(crate) fn confirm_hook_workspace(
    root: &std::path::Path,
) -> anyhow::Result<Option<crate::config::HookTrust>> {
    use phi_tui::{Frame, RuntimeEvent, TerminalGuard, Theme, TuiRenderer, TuiRuntime};
    use std::io::IsTerminal;

    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "workspace Hook trust requires a local terminal; start phi interactively in {} first",
        root.display()
    );
    let _guard = TerminalGuard::enter()?;
    let theme = Theme::default();
    let mut renderer = TuiRenderer::new(theme.clone());
    let mut runtime = TuiRuntime::new();
    runtime.mount_widget(
        HookTrustScreen::new(root),
        MountOptions::after_root(InputContext::Confirm).focused(),
    );
    const INPUT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    loop {
        runtime.render_if_requested(&mut renderer, &Frame::default(), &theme)?;
        if let RuntimeEvent::Key(key) = runtime.poll_event_with_timeout(INPUT_POLL_INTERVAL)?
            && let InputOutcome::Action(answer) = runtime.dispatch_key(key)
        {
            renderer.finish()?;
            return Ok(answer);
        }
    }
}

struct HookTrustScreen {
    root: String,
    confirmation: ConfirmState,
}

impl HookTrustScreen {
    fn new(root: &std::path::Path) -> Self {
        Self {
            root: root.display().to_string(),
            confirmation: ConfirmState {
                title: "Workspace trust".into(),
                message: "Run configured hooks?".into(),
                choice: Choice::new(["Disable hooks", "Trust"]),
                help: "Arrows: select, Enter: save, Esc: cancel".into(),
            },
        }
    }
}

impl Widget for HookTrustScreen {
    type Action = Option<crate::config::HookTrust>;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let mut lines = phi_tui::text::wrap(&self.root, context.area.width as usize)
            .into_iter()
            .map(|line| StyledLine::new(line, ThemeColor::Text))
            .collect::<Vec<_>>();
        lines.extend(phi_tui::text::wrap(
            "Hooks run outside Phi's sandbox with your user permissions, without Agent tool prompts. Trust only workspaces you control.",
            context.area.width as usize,
        ).into_iter().map(|line| StyledLine::new(line, ThemeColor::Dim)));
        lines.extend(self.confirmation.render(context));
        lines
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match self.confirmation.choice.handle_input(key) {
            InputOutcome::Action(ChoiceAction::Selected(1)) => {
                InputOutcome::Action(Some(crate::config::HookTrust::Trusted))
            }
            InputOutcome::Action(ChoiceAction::Selected(_)) => {
                InputOutcome::Action(Some(crate::config::HookTrust::Disabled))
            }
            InputOutcome::Action(ChoiceAction::Cancelled) => InputOutcome::Action(None),
            InputOutcome::Consumed => InputOutcome::Consumed,
            InputOutcome::Ignored => InputOutcome::Ignored,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum InteractionPrompt {
    Permission(ConfirmState),
    Elicitation(ElicitationState),
}

pub(crate) struct InteractionWidget {
    prompt: InteractionPrompt,
    bound_keys: Vec<KeyInput>,
}

impl Widget for InteractionWidget {
    type Action = UiAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        match &self.prompt {
            InteractionPrompt::Permission(confirm) => confirm.render(context),
            InteractionPrompt::Elicitation(elicitation) => elicitation.render(context),
        }
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<UiAction> {
        if self.bound_keys.contains(key) {
            return InputOutcome::Ignored;
        }
        match &mut self.prompt {
            InteractionPrompt::Permission(confirm) => match confirm.handle_input(key) {
                InputOutcome::Ignored => InputOutcome::Consumed,
                outcome => outcome,
            },
            InteractionPrompt::Elicitation(elicitation) => {
                use super::panel::mcp::ElicitationAction;
                match elicitation.handle_input(key) {
                    InputOutcome::Action(ElicitationAction::Submit(content)) => {
                        InputOutcome::Action(UiAction::McpElicitationSubmit(content))
                    }
                    InputOutcome::Action(ElicitationAction::Decline) => {
                        InputOutcome::Action(UiAction::McpElicitationDecline)
                    }
                    InputOutcome::Action(ElicitationAction::Cancel) => {
                        InputOutcome::Action(UiAction::McpElicitationCancel)
                    }
                    InputOutcome::Action(ElicitationAction::OpenUrl(url)) => {
                        InputOutcome::Action(UiAction::McpElicitationOpenUrl(url))
                    }
                    _ => InputOutcome::Consumed,
                }
            }
        }
    }
}

impl UiState {
    pub(crate) fn set_interaction(&mut self, prompt: InteractionPrompt) {
        self.clear_interaction();
        self.interaction = Some(self.runtime.mount_widget(
            InteractionWidget {
                prompt,
                bound_keys: Vec::new(),
            },
            MountOptions::input(InputContext::Confirm),
        ));
    }

    pub(crate) fn activate_interaction(&mut self) {
        let Some(handle) = self.interaction else {
            return;
        };
        let bound_keys = self
            .runtime
            .key_bindings()
            .bindings()
            .iter()
            .filter(|binding| binding.context == InputContext::Confirm)
            .map(|binding| binding.key.clone())
            .collect();
        self.runtime
            .update_widget(handle, |widget| widget.bound_keys = bound_keys);
        self.runtime.focus_widget(handle);
    }

    pub(crate) fn clear_interaction(&mut self) {
        if let Some(handle) = self.interaction.take() {
            self.runtime.remove_widget(handle);
        }
    }
}
pub(crate) async fn drain_permission_requests(
    client: &AgentAttachment,
    main_id: &phi_sdk::agent_instance::AgentInstanceId,
    requests: Vec<InteractionEnvelope>,
    pending_interaction: &mut Option<InteractionEnvelope>,
    state: &mut UiState,
) -> bool {
    if let Some(request) = pending_interaction.as_ref() {
        if requests
            .iter()
            .any(|pending| pending.request_id == request.request_id)
        {
            return false;
        }
        *pending_interaction = None;
        state.clear_interaction();
        state.notify(NotifyKind::Info, "interaction cancelled");
        return true;
    }
    let Some(request) = requests.into_iter().next() else {
        return false;
    };
    match &request.request {
        InteractionRequest::Permission(_) => {
            project_permission_request(&request, client.id(), main_id, state);
            *pending_interaction = Some(request);
        }
        InteractionRequest::McpElicitation(elicitation) => {
            match ElicitationState::from_request(elicitation) {
                Ok(elicitation) => {
                    state.set_interaction(InteractionPrompt::Elicitation(elicitation));
                    *pending_interaction = Some(request);
                }
                Err(error) => {
                    let _ = client
                        .respond_interaction(
                            &request.request_id,
                            InteractionResponse::McpElicitation(
                                phi_sdk::extension::mcp::McpElicitationResponse::Decline,
                            ),
                        )
                        .await;
                    state.notify(
                        NotifyKind::Warning,
                        format!("MCP elicitation declined: {error}"),
                    );
                }
            }
        }
    }
    true
}

pub(crate) async fn answer_permission(
    attachment: &AgentAttachment,
    pending_interaction: &mut Option<InteractionEnvelope>,
    state: &mut UiState,
    session: &mut Session,
    decision: PermissionDecision,
) {
    let Some(request) = pending_interaction.take() else {
        return;
    };
    let Ok(answer) = attachment
        .respond_interaction(
            &request.request_id,
            InteractionResponse::Permission(decision),
        )
        .await
    else {
        state.clear_interaction();
        state.notify(
            NotifyKind::Warning,
            "permission request is no longer active",
        );
        return;
    };
    if answer.allow_always.is_some() && attachment.id().as_str() == session.id.as_str() {
        match attachment.session().await {
            Ok(current) => session.permission_allowlist = current.permission_allowlist,
            Err(error) => state.notify(
                NotifyKind::Warning,
                format!("failed to refresh permissions: {error}"),
            ),
        }
    }
    if let Some(error) = answer.persistence_error {
        state.notify(
            NotifyKind::Warning,
            format!("permission allowed, but failed to persist allowlist: {error}"),
        );
    }
    state.clear_interaction();
}

pub(crate) async fn answer_mcp_elicitation(
    client: &AgentAttachment,
    pending_interaction: &mut Option<InteractionEnvelope>,
    state: &mut UiState,
    response: phi_sdk::extension::mcp::McpElicitationResponse,
) {
    let Some(request) = pending_interaction.take() else {
        return;
    };
    if let Err(error) = client
        .respond_interaction(
            &request.request_id,
            InteractionResponse::McpElicitation(response),
        )
        .await
    {
        state.notify(
            NotifyKind::Error,
            format!("MCP elicitation failed: {error}"),
        );
    }
    state.clear_interaction();
}

fn project_permission_request(
    envelope: &InteractionEnvelope,
    owner_id: &phi_sdk::agent_instance::AgentInstanceId,
    main_id: &phi_sdk::agent_instance::AgentInstanceId,
    state: &mut UiState,
) {
    let request = match &envelope.request {
        InteractionRequest::Permission(request) => request,
        InteractionRequest::McpElicitation(_) => return,
    };
    if owner_id == main_id
        && envelope
            .scope
            .agent_id
            .as_deref()
            .is_none_or(|id| id == main_id.as_str())
        && let Some(preview) = &request.preview
        && let Some(block_id) = state.conversation.live_active_tool_block()
    {
        state
            .conversation
            .set_tool_diff_status(block_id, preview, BlockStatus::Running);
    }
    let title = request
        .origin
        .as_ref()
        .map(|origin| format!("allow {} from {origin}?", request.tool))
        .unwrap_or_else(|| format!("allow {}?", request.tool));
    let confirm = ConfirmState {
        title,
        message: request.input.clone(),
        choice: Choice::new(["Allow once", "Always", "Deny"]),
        help: String::from("←→ / tab navigate  enter select  esc cancel"),
    };
    state.set_interaction(InteractionPrompt::Permission(confirm));
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::agent_instance::AgentInstanceId;
    use phi_sdk::interaction::PermissionInteraction;

    #[test]
    fn workspace_trust_requires_explicit_selection() {
        let root = std::path::Path::new("/tmp/workspace");
        assert_eq!(
            HookTrustScreen::new(root).handle_input(&KeyInput::Enter),
            InputOutcome::Action(Some(crate::config::HookTrust::Disabled))
        );
        for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
            assert_eq!(
                HookTrustScreen::new(root).handle_input(&key),
                InputOutcome::Action(None)
            );
        }
        let mut screen = HookTrustScreen::new(root);
        assert_eq!(
            screen.handle_input(&KeyInput::Right),
            InputOutcome::Consumed
        );
        assert_eq!(
            screen.handle_input(&KeyInput::Enter),
            InputOutcome::Action(Some(crate::config::HookTrust::Trusted))
        );
    }

    #[tokio::test]
    async fn permission_previews_only_update_the_owning_main_tool() {
        let main_id = AgentInstanceId::new("main");
        for (owner, agent, allowed) in [
            ("main", None, true),
            ("main", Some("child"), false),
            ("child", Some("child"), false),
        ] {
            let mut state = UiState::default();
            state.conversation.project_main_run_started();
            let block = state.conversation.project_tool_call_started(
                "call",
                "Edit",
                "{}",
                Default::default(),
            );
            let mut envelope = InteractionEnvelope {
                request_id: "permission".into(),
                emitted_at: chrono::Utc::now(),
                scope: Default::default(),
                request: InteractionRequest::Permission(PermissionInteraction {
                    tool: "edit".into(),
                    input: "file.rs".into(),
                    preview: Some("+private diff".into()),
                    origin: None,
                }),
            };
            envelope.scope.agent_id = agent.map(Into::into);
            project_permission_request(
                &envelope,
                &AgentInstanceId::new(owner),
                &main_id,
                &mut state,
            );
            assert!(state.interaction.is_some());
            assert_eq!(
                matches!(
                    &state
                        .conversation
                        .blocks
                        .iter()
                        .find(|entry| entry.id == block)
                        .unwrap()
                        .block
                        .body,
                    phi_tui::BlockBody::Diff(_)
                ),
                allowed
            );
        }
    }
}
