//! Terminal attachment, interaction and event-to-view projection for the application.
//! Application runtime/SDK own durable facts and execution; phi-tui owns reusable terminal mechanics.

use std::time::{Duration, Instant};

use phi_tui::{NotifyKind, RenderPriority, TerminalGuard, TuiRenderer};

#[cfg(test)]
use crate::config::Settings;
use crate::runtime::attachment::{AgentAttachment, SaveNotice, SaveObservationCursor};
use crate::session::Session;
use phi_sdk::interaction::{InteractionEnvelope, InteractionRequest, PermissionDecision};
use phi_sdk::transcript::Transcript;

mod agent;
mod assembly;
mod commands;
mod completion;
mod components;
mod controller;
mod diagnostics;
mod extensions;
mod memory;
mod overlays;
mod render;
mod state;
mod workspace;

use components::format;
pub(crate) use components::format::tool_display_formatter;
use components::panel::{activities, mcp};
pub(crate) use components::permission::confirm_hook_workspace;
pub(crate) use components::picker::session::{
    RESUME_PICKER_MAX_VISIBLE, ResumeDeleteOutcome, ResumePickerAction, delete_resume_candidate,
    handle_resume_picker_input, resume_picker, session_label, short_time,
};
pub(crate) use memory::{MemoryAdministration, MemoryNotice, RewindAnnotations, TapeReview};
pub(crate) use state::PickerState as UiPickerState;

use crate::commands::CommandRegistry;
#[cfg(test)]
use crate::runtime::selection::SessionSelection;
#[cfg(test)]
use agent::handle_subagent_key;
use agent::{
    ActiveSubagentAction, SubagentProjection, directed_agent_invocation, drain_agent_events,
    handle_active_subagent_input, submit_prompt,
};
use assembly::InteractiveComponents;
#[cfg(test)]
use assembly::InteractiveFeatures;
pub(crate) use assembly::run_gateway;
pub use assembly::run_interactive;
use components::input::{UiAction, poll_action};
use components::permission::answer_mcp_elicitation;
use components::permission::{answer_permission, drain_permission_requests};
#[cfg(test)]
use components::picker::switch::{save_switches, set_switch_alias};
use controller::InteractiveController;
use diagnostics::report_resource_diagnostics;
#[cfg(test)]
use phi_tui::KeyInput;
use render::render_current_target;
use state::{UiState, state_from_session};

const SAVE_STATUS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct SaveStatus {
    cursor: SaveObservationCursor,
    next_poll: Option<Instant>,
    instance_id: Option<String>,
}

impl SaveStatus {
    async fn poll(&mut self, instance: &AgentAttachment, state: &mut UiState) -> bool {
        let now = Instant::now();
        let id = instance.id().to_string();
        if self.instance_id.as_ref() == Some(&id) && self.next_poll.is_some_and(|next| now < next) {
            return false;
        }
        self.instance_id = Some(id);
        let notice = self.cursor.observe(instance.save_observation().await);
        self.next_poll = Some(Instant::now() + SAVE_STATUS_INTERVAL);
        match notice {
            Some(SaveNotice::Warning(message)) => {
                state.notify(phi_tui::NotifyKind::Warning, message)
            }
            Some(SaveNotice::Recovered) => {
                state.notify(phi_tui::NotifyKind::Info, "Session saving recovered")
            }
            None => return false,
        }
        true
    }
}

fn materialize_transcript_snapshot(session: &mut Session, transcript: Transcript) {
    session.set_transcript(transcript);
}

fn release_transcript_snapshot(session: &mut Session) {
    session.transcript.entries.clear();
    session.transcript.switch_snapshots.clear();
}

fn sync_transcript_metadata(session: &mut Session, transcript: &Transcript) {
    session.transcript.id = transcript.id.clone();
    session.transcript.leaf_id = transcript.leaf_id.clone();
    session.transcript.created_at = transcript.created_at.clone();
    session.transcript.updated_at = transcript.updated_at.clone();
    session.transcript.total_tokens = transcript.total_tokens;
    session.transcript.total_cost = transcript.total_cost;
    session.transcript.total_cache_read = transcript.total_cache_read;
    session.transcript.total_cache_write = transcript.total_cache_write;
    session.total_estimated_tokens = transcript.estimated_context_tokens();
}

async fn run(
    mut components: InteractiveComponents<'_>,
    owned_sessions: Option<std::sync::Arc<crate::runtime::SessionAssociations>>,
    mut agent_instance: AgentAttachment,
    session: &mut Session,
    startup_warning: Option<String>,
    startup_error: Option<String>,
) -> anyhow::Result<()> {
    let features = components.features();
    let tool_display_names = if features.mcp {
        mcp::tool_display_name_projection_for_session(session, &agent_instance).await?
    } else {
        Default::default()
    };
    let tool_display = format::tool_display_formatter;
    let mut agent_events = agent_instance.subscribe();
    let mut startup_recovery = agent_instance.recover_events(&mut agent_events).await?;
    startup_recovery.declarations = features.filter(startup_recovery.declarations);
    let mut capabilities = startup_recovery.declarations.capabilities;
    report_resource_diagnostics(&capabilities).await?;
    let mut terminal_guard = TerminalGuard::enter()?;
    let commands = startup_recovery.declarations.commands;
    *session = startup_recovery.session;
    let mut state = state_from_session(
        features,
        session,
        &capabilities,
        &tool_display,
        commands,
        tool_display_names,
    );
    state
        .conversation
        .restore_live_run(&startup_recovery.snapshot.instance);
    if let Some(message) = startup_warning {
        state.notify(NotifyKind::Warning, message);
    }
    if let Some(message) = startup_error {
        state.notify(NotifyKind::Error, message);
    }
    let mut observed_instance_id = agent_instance.id().clone();
    let mut subagents = SubagentProjection::default();
    let mut save_status = SaveStatus::default();
    let mut workspace = workspace::WorkspaceRefresh::default();
    subagents.restore_summaries(&mut state, session);
    release_transcript_snapshot(session);
    let mut mcp_event_rx = if features.mcp {
        agent_instance.subscribe_mcp()?
    } else {
        None
    };
    let mcp_show_status = components.mcp_show_status;
    let mcp_notify = components.mcp_notify;
    let mut last_ctrl_c: Option<Instant> = None;
    let mut pending_interaction: Option<InteractionEnvelope> = None;
    let mut interaction_owner = agent_instance.clone();
    let mut renderer = TuiRenderer::default();
    components.models.start_updates(&agent_instance);
    let mut mcp_actions = mcp::PanelActions::default();

    render_current_target(
        &mut renderer,
        &mut subagents,
        &mut state,
        session,
        &agent_instance,
    )
    .await?;
    let mut needs_render = false;

    if let Some(extensions) = &components.extensions {
        extensions.ready_hook_notices(agent_instance.clone());
    }

    macro_rules! interactive_controller {
        () => {{
            InteractiveController {
                memory: components.memory,
                navigation: components.navigation,
                mcp_auth: components.mcp_auth,
                mcp_read_only: components.mcp_read_only,
                authentication: components.authentication.as_mut(),
                models: &mut components.models,
                switches: &components.switches,
                sessions: &components.sessions,
                agent_instance: &mut agent_instance,
                session,
                capabilities: &mut capabilities,
                extension_tasks: components.extensions.as_ref(),
                state: &mut state,
            }
        }};
    }

    loop {
        if let Some(authentication) = &mut components.authentication {
            needs_render |= authentication.poll(&mut state).await;
        }
        if agent_instance.id() != &observed_instance_id {
            agent_events = agent_instance.subscribe();
            state.agent_recovery_pending = true;
            observed_instance_id = agent_instance.id().clone();
            {
                mcp_event_rx = if features.mcp {
                    agent_instance.subscribe_mcp()?
                } else {
                    None
                };
            }
        }
        needs_render |=
            drain_agent_events(&agent_instance, &mut agent_events, &mut state, session).await;
        if let Some(sessions) = &owned_sessions {
            needs_render |= agent::project_session_notices(
                &mut state,
                sessions.take_notices()?,
                agent_instance.id(),
            );
        }
        needs_render |= save_status.poll(&agent_instance, &mut state).await;
        if let Some(declarations) = state.recovered_declarations.take() {
            controller::install_runtime_declarations(&mut capabilities, &mut state, declarations);
            {
                mcp::refresh_panel(&mut state, &agent_instance).await;
                mcp::refresh_tool_display_projection(&mut state, &agent_instance).await;
            }
            needs_render = true;
        }
        if state.subagent_views.is_empty() {
            subagents.restore_summaries(&mut state, session);
        }
        match subagents.poll(&agent_instance, &mut state).await {
            Ok(true) => state
                .runtime
                .request_render_with_priority(RenderPriority::Background),
            Ok(false) => {}
            Err(error) => state.notify(
                NotifyKind::Warning,
                format!("failed to observe subagents: {error}"),
            ),
        }
        let tasks_visible =
            if subagents.active_subagent().is_none() || activities::tasks_open(&state) {
                activities::TaskVisibility::Visible
            } else {
                activities::TaskVisibility::Hidden
            };
        if let Some(activities) = &mut components.activities
            && activities
                .poll(&agent_instance, &mut state, tasks_visible)
                .await
        {
            state
                .runtime
                .request_render_with_priority(RenderPriority::Background);
        }
        if let Some(workflows) = &mut components.workflow_observation
            && workflows.poll(&agent_instance, &mut state)
        {
            state
                .runtime
                .request_render_with_priority(RenderPriority::Background);
        }
        match subagents
            .interaction_owner(&agent_instance, pending_interaction.as_ref())
            .await
        {
            Ok((owner, requests)) => {
                interaction_owner = owner;
                needs_render |= drain_permission_requests(
                    &interaction_owner,
                    agent_instance.id(),
                    requests,
                    &mut pending_interaction,
                    &mut state,
                )
                .await;
            }
            Err(error) => {
                state.notify(
                    NotifyKind::Warning,
                    format!("failed to observe pending interactions: {error}"),
                );
                needs_render = true;
            }
        }
        if let Some(extensions) = &mut components.extensions {
            needs_render |= extensions.poll(&mut state);
        }
        {
            let mcp_changed = mcp::drain_mcp_events(
                &mut mcp_event_rx,
                &agent_instance,
                &mut state,
                mcp_show_status,
                mcp_notify,
            )
            .await;
            needs_render |= mcp_changed;
            if mcp_changed {
                needs_render |= mcp::refresh_panel(&mut state, &agent_instance).await;
            }
            needs_render |= mcp_actions.poll(&agent_instance, &mut state).await;
        }
        needs_render |= components.models.poll(&mut state, session);
        needs_render |= workspace
            .poll_target(
                &agent_instance,
                subagents.render_target(&mut state, session),
            )
            .await;
        if subagents.advance_clocks(&mut state, Instant::now()) {
            state
                .runtime
                .request_render_with_priority(RenderPriority::Animation);
        }
        state.runtime.terminal_resized()?;

        state.runtime.request_render_when(needs_render);
        needs_render = false;
        if state.runtime.render_due() {
            render_current_target(
                &mut renderer,
                &mut subagents,
                &mut state,
                session,
                &agent_instance,
            )
            .await?;
        }

        let input_timeout = subagents.input_timeout(&state, Instant::now());
        if let Some(action) =
            handle_active_subagent_input(&mut subagents, &agent_instance, &mut state, input_timeout)
                .await?
        {
            match action {
                ActiveSubagentAction::Quit => break,
                ActiveSubagentAction::Render => needs_render = true,
                ActiveSubagentAction::None => {}
            }
            state.runtime.request_render_when(needs_render);
            tokio::task::yield_now().await;
            continue;
        }

        let no_agents = Default::default();
        let supported_agents = if let Some(mentions) = &mut components.agent_mentions {
            mentions.refresh(&agent_instance).await?
        } else {
            &no_agents
        };
        let input_timeout = subagents.input_timeout(&state, Instant::now());
        let action = poll_action(
            &mut state,
            &mut last_ctrl_c,
            supported_agents,
            input_timeout,
        )?;
        if !features.action(&action) {
            let message = match &action {
                UiAction::DeletePick(value) if value.starts_with("resume:") => {
                    "Session deletion is unavailable in this interface. Use embedded local mode."
                }
                _ => "This action is unavailable in this interface.",
            };
            state.notify(NotifyKind::Info, message);
            continue;
        }
        match action {
            UiAction::None | UiAction::SurfaceKey { .. } => {}
            UiAction::Resize => needs_render = true,
            UiAction::RunExtensionKeybinding(id) => {
                if state
                    .picker
                    .as_ref()
                    .is_some_and(|picker| matches!(picker.kind, state::PickerKind::Rewind))
                {
                    materialize_transcript_snapshot(session, agent_instance.transcript().await?);
                }
                if let Some(extensions) = &components.extensions {
                    extensions.run_keybinding(id, agent_instance.clone(), &state, session);
                }
                release_transcript_snapshot(session);
                needs_render = true;
            }
            UiAction::Quit => break,
            UiAction::CancelSubagent(id) => {
                if let Err(error) = subagents.cancel(&id, &agent_instance).await {
                    state.notify(
                        NotifyKind::Warning,
                        format!("failed to cancel subagent: {error}"),
                    );
                }
                needs_render = true;
            }
            UiAction::EnterSubagent(id) => {
                match subagents.open(id, &agent_instance).await {
                    Ok(true) => activities::close_tasks(&mut state),
                    Ok(false) => {
                        state.notify(NotifyKind::Warning, "subagent session is not available")
                    }
                    Err(error) => state.notify(
                        NotifyKind::Warning,
                        format!("failed to open subagent: {error}"),
                    ),
                }
                needs_render = true;
            }
            UiAction::ToggleTasks => {
                activities::toggle_tasks(&mut state);
                needs_render = true;
            }
            UiAction::OpenTaskOwner(id) => {
                activities::open_owner(id, &mut subagents, &agent_instance, &mut state).await;
                needs_render = true;
            }
            UiAction::OpenMemoryAnchor(id) => {
                let mut controller = interactive_controller!();
                controller.open_memory_anchor(&id).await;
                needs_render = true;
            }
            UiAction::CloseActiveOverlay => {
                overlays::close_active(&mut state);
                needs_render = true;
            }
            UiAction::CancelRun => {
                cancel_run(&agent_instance, &mut state).await;
                needs_render = true;
            }
            UiAction::ToggleAllBlocksExpanded => {
                needs_render |= state.conversation.toggle_all_collapsible_blocks();
            }
            UiAction::BackgroundRunningBash => {
                activities::open_current_bash(
                    &agent_instance,
                    agent_instance.id().as_str(),
                    &mut state,
                )
                .await;
                needs_render = true;
            }
            UiAction::CloseBashSelector => {
                state.close_bash_selector();
                needs_render = true;
            }
            UiAction::BackgroundBash { owner, id } => {
                activities::background_bash(&agent_instance, &owner, &id, &mut state).await;
                needs_render = true;
            }
            UiAction::StopBash { owner, id } => {
                activities::stop_bash(&agent_instance, &owner, &id, &mut state).await;
                needs_render = true;
            }
            UiAction::OpenBashOutput { owner, id } => {
                activities::open_bash_output(&agent_instance, owner, &id, &mut state).await;
                needs_render = true;
            }
            UiAction::PermissionAllowOnce => {
                answer_permission(
                    &interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    session,
                    PermissionDecision::AllowOnce,
                )
                .await;
                needs_render = true;
            }
            UiAction::PermissionAllowAlways => {
                let pattern = pending_permission_pattern(&pending_interaction);
                answer_permission(
                    &interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    session,
                    PermissionDecision::AllowAlways(pattern),
                )
                .await;
                needs_render = true;
            }
            UiAction::PermissionDeny => {
                answer_permission(
                    &interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    session,
                    PermissionDecision::Deny,
                )
                .await;
                needs_render = true;
            }
            UiAction::McpElicitationSubmit(content) => {
                answer_mcp_elicitation(
                    &mut interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    phi_sdk::extension::mcp::McpElicitationResponse::Accept { content },
                )
                .await;
                needs_render = true;
            }
            UiAction::McpElicitationDecline => {
                answer_mcp_elicitation(
                    &mut interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    phi_sdk::extension::mcp::McpElicitationResponse::Decline,
                )
                .await;
                needs_render = true;
            }
            UiAction::McpElicitationCancel => {
                answer_mcp_elicitation(
                    &mut interaction_owner,
                    &mut pending_interaction,
                    &mut state,
                    phi_sdk::extension::mcp::McpElicitationResponse::Cancel,
                )
                .await;
                needs_render = true;
            }
            UiAction::McpElicitationOpenUrl(url) => {
                if let Err(error) = mcp::open_url(&url) {
                    state.notify(
                        NotifyKind::Warning,
                        format!("failed to open MCP URL: {error}"),
                    );
                }
                needs_render = true;
            }
            UiAction::Pick(value) => {
                let previous_session_id = session.id.clone();
                if !mcp_actions
                    .handle_pick(&value, &agent_instance, &mut state)
                    .await
                {
                    interactive_controller!().select_picker_value(value).await;
                }
                if session.id != previous_session_id {
                    // History revisions are view-local; a new Session must replace old scrollback.
                    // finish() is exit cleanup and would expose an unsynchronized intermediate frame.
                    renderer.invalidate_history();
                }
                needs_render = true;
            }
            action @ (UiAction::SetSwitchAlias { .. }
            | UiAction::SaveSwitchConfig
            | UiAction::CycleSwitchField { .. }
            | UiAction::ApplySwitchConfig(_)
            | UiAction::DeletePick(_)) => {
                interactive_controller!().handle_picker_action(action).await;
                needs_render = true;
            }
            UiAction::CycleSwitch(direction) => {
                interactive_controller!().cycle_switch(direction).await;
                needs_render = true;
            }
            UiAction::AdjustThinkingLevel(direction) => {
                interactive_controller!()
                    .adjust_thinking_level(direction)
                    .await;
                needs_render = true;
            }
            UiAction::Submit {
                query_prompt,
                display,
                history_entry,
                dispatch,
            } => {
                if display.starts_with('!') && components.shell.is_none() {
                    state.notify(
                        NotifyKind::Warning,
                        "Shell commands are unavailable in this interface.",
                    );
                    continue;
                }
                if let Some(shell) = &components.shell
                    && shell.execute(&display, &mut state, &agent_instance).await
                {
                    state.editor.add_history_entry(history_entry);
                    needs_render = true;
                    continue;
                }
                let command = match CommandRegistry::new(&capabilities.extension_commands)
                    .resolve(&display)
                {
                    Ok(command) => command,
                    Err(error) => {
                        state.notify(NotifyKind::Warning, error.to_string());
                        needs_render = true;
                        continue;
                    }
                };
                if let Some(command) = command {
                    let previous_session_id = session.id.clone();
                    interactive_controller!()
                        .execute_command(&display, command)
                        .await;
                    if session.id != previous_session_id {
                        render_current_target(
                            &mut renderer,
                            &mut subagents,
                            &mut state,
                            session,
                            &agent_instance,
                        )
                        .await?;
                        needs_render = false;
                    } else {
                        needs_render = true;
                    }
                    continue;
                }
                let directed = match directed_agent_invocation(&display, supported_agents) {
                    Some(Ok(constraint)) => Some(constraint),
                    Some(Err(error)) => {
                        state.notify(NotifyKind::Error, error);
                        needs_render = true;
                        continue;
                    }
                    None => None,
                };
                state.editor.add_history_entry(history_entry);
                let mut turn_prompt = phi_sdk::turn::TurnPrompt {
                    prompt: query_prompt.user_prompt.clone(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    injected_context: query_prompt.injected_context,
                    images: Vec::new(),
                    tool_policy: Default::default(),
                };
                if directed.is_some() {
                    turn_prompt.prompt = display.clone();
                }
                submit_prompt(
                    turn_prompt,
                    display,
                    query_prompt.user_prompt,
                    &mut state,
                    session,
                    &mut agent_instance,
                    dispatch,
                    directed,
                )
                .await;
                needs_render = true;
            }
        }

        state.runtime.request_render_when(needs_render);
        tokio::task::yield_now().await;
    }

    drop(mcp_actions);
    let input_cleanup = terminal_guard.stop_input();
    if let Some(sessions) = &owned_sessions
        && let Ok(stopped) = sessions
            .stop(
                agent_instance.id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
    {
        *session = stopped;
    }
    if let Some(sessions) = &owned_sessions
        && agent::project_session_notices(&mut state, sessions.take_notices()?, agent_instance.id())
    {
        subagents.switch_to_main();
        render_current_target(
            &mut renderer,
            &mut subagents,
            &mut state,
            session,
            &agent_instance,
        )
        .await?;
    }
    renderer.finish()?;
    input_cleanup?;
    Ok(())
}

fn pending_permission_pattern(pending: &Option<InteractionEnvelope>) -> String {
    match pending.as_ref().map(|envelope| &envelope.request) {
        Some(InteractionRequest::Permission(request)) => request.input.clone(),
        Some(InteractionRequest::McpElicitation(_)) | None => String::new(),
    }
}

#[cfg(test)]
pub(crate) async fn verify_subagent_view(
    main: &AgentAttachment,
    task_id: &str,
    approve_permission: bool,
) {
    let mut parent = main.session().await.unwrap();
    let mut state = state::UiState::default();
    state.features.activities = true;
    state.editor.replace_text("retained parent draft");
    let mut manager = SubagentProjection::default();
    manager.poll(main, &mut state).await.unwrap();
    select_subagent_view(main, &mut state, task_id).await;
    overlays::open_tasks(&mut state);
    activities::open_owner(task_id.into(), &mut manager, main, &mut state).await;
    assert_eq!(manager.active_subagent(), Some(task_id));
    assert!(state.active_overlay.is_none());
    let target = manager.render_target(&mut state, &parent);
    assert_ne!(target.session().id, parent.id);
    assert_eq!(
        target.session().creator_id().unwrap().as_str(),
        parent.id.as_str()
    );
    handle_subagent_key(Some(KeyInput::Ctrl('o')), &mut manager, main)
        .await
        .unwrap();
    verify_subagent_permission(
        main,
        &mut manager,
        &mut state,
        &mut parent,
        approve_permission,
    )
    .await;
    assert_eq!(state.editor.text(), "retained parent draft");
}

#[cfg(test)]
pub(crate) async fn verify_subagent_interaction_scope(
    main: &AgentAttachment,
    pending: &phi_sdk::interaction::InteractionEnvelope,
) {
    let parent = main.session().await.unwrap();
    let mut state = state::UiState::default();
    state.features.activities = true;
    let mut manager = SubagentProjection::default();
    manager.poll(main, &mut state).await.unwrap();
    let (owner, requests) = manager
        .interaction_owner(main, Some(pending))
        .await
        .unwrap();
    assert_eq!(
        owner.id().as_str(),
        pending.scope.session_id.as_deref().unwrap()
    );
    let mut projected = None;
    assert!(
        drain_permission_requests(&owner, main.id(), requests, &mut projected, &mut state).await
    );
    assert!(state.interaction.is_some());
    let task = main
        .subagents()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.child_agent_id == owner.id().as_str())
        .unwrap();
    assert!(manager.open(task.child_agent_id, main).await.unwrap());
    assert_eq!(
        manager.render_target(&mut state, &parent).session().id,
        parent.id
    );
    manager.switch_to_main();
    assert_eq!(
        owner.pending_interactions().await.unwrap()[0].request_id,
        pending.request_id
    );
}

#[cfg(test)]
async fn select_subagent_view(
    attachment: &AgentAttachment,
    state: &mut state::UiState,
    task_id: &str,
) {
    activities::TaskRefresh::default()
        .poll(attachment, state, activities::TaskVisibility::Hidden)
        .await;
    assert!(matches!(
        state.runtime.dispatch_key(KeyInput::Ctrl('t')),
        phi_tui::InputOutcome::Action(UiAction::ToggleTasks)
    ));
    overlays::open_tasks(state);
    let index = state
        .activity_agents
        .iter()
        .position(|view| view.id == task_id)
        .unwrap();
    for _ in 0..index {
        state.runtime.dispatch_key(KeyInput::Down);
    }
    assert!(matches!(state.runtime.dispatch_key(KeyInput::Enter),
        phi_tui::InputOutcome::Action(UiAction::EnterSubagent(id)) if id == task_id));
}

#[cfg(test)]
async fn verify_subagent_permission(
    main: &AgentAttachment,
    manager: &mut SubagentProjection,
    state: &mut state::UiState,
    parent: &mut Session,
    approve: bool,
) {
    let (owner, requests) = manager.interaction_owner(main, None).await.unwrap();
    let mut pending = None;
    drain_permission_requests(&owner, main.id(), requests, &mut pending, state).await;
    if pending.is_some() {
        assert_eq!(manager.render_target(state, parent).session().id, parent.id);
        assert_eq!(
            manager
                .interaction_owner(main, pending.as_ref())
                .await
                .unwrap()
                .0
                .id(),
            owner.id()
        );
    }
    handle_subagent_key(Some(KeyInput::Esc), manager, main)
        .await
        .unwrap();
    assert!(manager.active_subagent().is_none());
    assert_eq!(manager.render_target(state, parent).session().id, parent.id);
    if !approve {
        return;
    }
    assert_ne!(owner.id(), main.id());
    let allowlist = parent.permission_allowlist.clone();
    let pattern = pending_permission_pattern(&pending);
    assert!(!pattern.is_empty());
    let request_id = pending.as_ref().unwrap().request_id.clone();
    state.activate_interaction();
    state.runtime.dispatch_key(KeyInput::Right);
    assert_eq!(
        state.runtime.dispatch_key(KeyInput::Enter),
        phi_tui::InputOutcome::Action(UiAction::PermissionAllowAlways)
    );
    assert_eq!(pending.as_ref().unwrap().request_id, request_id);
    answer_permission(
        &owner,
        &mut pending,
        state,
        parent,
        PermissionDecision::AllowAlways(pattern.clone()),
    )
    .await;
    assert!(pending.is_none());
    assert!(state.interaction.is_none());
    assert_eq!(parent.permission_allowlist, allowlist);
    assert!(
        owner
            .session()
            .await
            .unwrap()
            .permission_allowlist
            .iter()
            .any(|entry| entry.pattern == pattern)
    );
    assert_eq!(
        main.session().await.unwrap().permission_allowlist,
        allowlist
    );
}

async fn cancel_run(agent_instance: &AgentAttachment, state: &mut state::UiState) {
    if let Err(error) = agent_instance.abort().await {
        state.notify(NotifyKind::Error, format!("failed to cancel run: {error}"));
    }
}

#[cfg(test)]
pub(crate) async fn verify_gateway_commands(mut attachment: AgentAttachment) -> AgentAttachment {
    let mut events = attachment.subscribe();
    let recovered = attachment.recover_events(&mut events).await.unwrap();
    let mut components = InteractiveComponents::gateway();
    let features = components.features();
    let declarations = features.filter(recovered.declarations);
    let original_generation = declarations.generation_id.clone();
    let mut capabilities = declarations.capabilities;
    let mut session = recovered.session;
    let original_id = session.id.to_string();
    let mut state = state_from_session(
        features,
        &session,
        &capabilities,
        &format::tool_display_formatter,
        declarations.commands,
        Default::default(),
    );
    release_transcript_snapshot(&mut session);
    assert!(
        !SubagentProjection::default()
            .poll(&attachment, &mut state)
            .await
            .unwrap()
    );
    assert!(!mcp::refresh_tool_display_projection(&mut state, &attachment).await);
    let mut controller = InteractiveController {
        memory: None,
        navigation: None,
        mcp_auth: None,
        mcp_read_only: true,
        authentication: components.authentication.as_mut(),
        models: &mut components.models,
        switches: &components.switches,
        sessions: &components.sessions,
        agent_instance: &mut attachment,
        session: &mut session,
        capabilities: &mut capabilities,
        extension_tasks: components.extensions.as_ref(),
        state: &mut state,
    };
    for (prompt, expected_kind) in [
        ("/switch", state::PickerKind::RuntimeSwitch),
        ("/model", state::PickerKind::Model),
        ("/resume", state::PickerKind::Resume),
    ] {
        let command = CommandRegistry::new(&[]).resolve(prompt).unwrap().unwrap();
        Box::pin(controller.execute_command(prompt, command)).await;
        let picker = controller.state.picker.as_ref().unwrap();
        assert_eq!(picker.kind, expected_kind);
        assert!(!picker.items.is_empty(), "{prompt}");
        if prompt == "/resume" {
            let current = picker
                .items
                .iter()
                .find(|item| item.value == format!("resume:{original_id}"))
                .unwrap();
            assert_eq!(current.label, "current · retained history");
            assert!(current.description.ends_with(&original_id[..8]));
            assert_eq!(picker.title, "resume session · enter to resume");
        }
        assert!(
            !picker
                .items
                .iter()
                .any(|item| item.value == "switch:configure")
        );
        controller.state.close_picker();
    }
    let prompt = format!("/resume {}", uuid::Uuid::new_v4());
    let command = CommandRegistry::new(&[]).resolve(&prompt).unwrap().unwrap();
    Box::pin(controller.execute_command(&prompt, command)).await;
    assert!(controller.state.picker.is_none());
    assert!(matches!(
        &controller.state.conversation.blocks.last().unwrap().block.body,
        phi_tui::BlockBody::Notify(notice)
            if notice.kind == NotifyKind::Info && notice.message == "no matching sessions"
    ));
    let command = CommandRegistry::new(&[])
        .resolve("/login")
        .unwrap()
        .unwrap();
    Box::pin(controller.execute_command("/login", command)).await;
    assert!(matches!(
        &controller.state.conversation.blocks.last().unwrap().block.body,
        phi_tui::BlockBody::Notify(notice)
            if notice.kind == NotifyKind::Warning
                && notice.message == "This command is unavailable in this interface."
    ));
    Box::pin(controller.select_picker_value("switch:plan".into())).await;
    assert_eq!(controller.session.current_switch, "plan");
    controller.state.editor.set_text("retained draft");
    Box::pin(controller.reload_resources()).await;
    assert_ne!(
        controller
            .agent_instance
            .recovery()
            .await
            .unwrap()
            .declarations
            .generation_id,
        original_generation
    );
    assert_eq!(controller.state.editor.text(), "retained draft");
    assert!(
        controller
            .state
            .slash_commands
            .iter()
            .any(|command| command.name == "skill:review")
    );
    assert!(
        !controller
            .state
            .slash_commands
            .iter()
            .any(|command| command.name == "login")
    );
    Box::pin(controller.select_picker_value("model:local:selected".into())).await;
    assert_eq!(controller.session.model, "selected");
    let command = CommandRegistry::new(&[]).resolve("/new").unwrap().unwrap();
    Box::pin(controller.execute_command("/new", command)).await;
    assert_ne!(controller.session.id.as_str(), original_id);
    assert_eq!(controller.session.current_switch, "plan");
    assert_eq!(controller.session.model, "selected");
    assert_eq!(controller.session.context_window, 64_000);
    assert!(controller.session.entries.is_empty());
    assert!(
        !controller
            .state
            .slash_commands
            .iter()
            .any(|command| command.name == "login")
    );
    Box::pin(controller.select_picker_value(format!("resume:{original_id}"))).await;
    assert_eq!(controller.session.id.as_str(), original_id);
    assert_eq!(controller.session.current_switch, "plan");
    assert!(
        !controller
            .state
            .slash_commands
            .iter()
            .any(|command| command.name == "login")
    );
    attachment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_branch_change_with_unchanged_entry_ids() {
        let mut session =
            crate::session::new("test", "model", 1_000, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("first");
        let first = session.transcript.leaf_id.clone().unwrap();
        session.transcript.append_user_message("second");
        let mut canonical = session.transcript.clone();
        canonical.select_branch(Some(&first)).unwrap();

        materialize_transcript_snapshot(&mut session, canonical.clone());

        assert_eq!(session.transcript.leaf_id, canonical.leaf_id);
        assert_eq!(session.transcript.entries.len(), canonical.entries.len());
        assert_eq!(
            session.total_estimated_tokens,
            canonical.estimated_context_tokens()
        );
        release_transcript_snapshot(&mut session);
        assert!(session.entries.is_empty());
        assert_eq!(
            session.total_estimated_tokens,
            canonical.estimated_context_tokens()
        );
    }
    use super::components::picker::switch::set_switch_model;
    use crate::config::SwitchSettings;
    use clap::Parser;
    use components::picker::{model, session as session_picker, switch};
    use phi_sdk::PhiRuntimeServices;
    use phi_sdk::extension::ExtensionRuntime;
    use phi_sdk::lifecycle::SessionStartReason;
    use phi_sdk::models::{ModelCatalog, ModelCatalogOptions};
    use phi_sdk::sandbox::Sandbox;
    use phi_tui::{BlockBody, BlockRole, HistoryBudget, RenderArea, Theme};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::render::{BuiltUiFrame, RenderTarget, build_ui_frame};
    use super::state::UiState;
    use crate::cli::Cli;
    use crate::runtime::attachment::AgentEvent;
    use crate::runtime::{
        AgentInstanceBuildOptions, AgentRuntimeFactory, PhiBuildOptions, RuntimeRebuildOptions,
        RuntimeResources,
    };

    #[tokio::test]
    async fn shortcuts_use_canonical_history_and_preserve_editor_drafts() {
        let (root, _, test_runtime) = crate::runtime::preparation::tests::session_test_runtime();
        let catalog = Arc::new(
            ModelCatalog::load(ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let config = Settings {
            default_provider: Some("local".into()),
            default_model: Some("test".into()),
            switches: Some(vec![crate::config::SwitchConfig {
                name: "host-only".into(),
                prompt: Some("private-host-prompt".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let policy = crate::runtime::TestConfiguration::new(
            Arc::new(Cli::parse_from(["phi"])),
            config,
            crate::AgentBuildOptions {
                no_tools: true,
                ..Default::default()
            },
            Vec::new(),
            catalog,
        );
        policy.install(&test_runtime.preparation);
        let settings = root.join(".phi/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let content = r#"{"switches":[{"name":"host-only","provider":"local","model":"test"}],"no_tools":true}"#;
        std::fs::write(&settings, content).unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        session.current_switch = "plan".into();
        session.transcript.append_user_message("rewind draft");
        let user_entry = session.leaf_id.clone().unwrap();
        session.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::ToolCall {
                id: "host-anchor-call".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "tape_anchor".into(),
                args: serde_json::json!({"name": "host-anchor"}),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "tool_use".into(),
        );
        session.transcript.append_tool_result(
            "host-anchor-call",
            r#"{"name":"host-anchor","type":"normal"}"#,
            false,
            None,
        );
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap();
        let agent = Box::pin(workflows.start_session(session, false))
            .await
            .unwrap();
        let models = policy.model_choices_owner();
        let picker_draft = Rc::new(RefCell::new(models.picker_seed()));
        assert_eq!(
            picker_draft.borrow().switch_names(),
            ["default", "plan", "host-only"]
        );
        let mut attachment = AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        let accepted = agent.current_runtime_view().await;
        let mut session = accepted.session;
        let mut capabilities = accepted.declarations.capabilities;
        let mut state = UiState {
            features: InteractiveFeatures::all_enabled(),
            ..UiState::default()
        };
        let tasks = extensions::ExtensionTaskQueue::default();
        let mut controller = InteractiveController {
            memory: Some(&policy.memory_administration()),
            navigation: Some(&workflows),
            mcp_auth: Some(&policy.mcp_auth_diagnostics()),
            mcp_read_only: false,
            authentication: None,
            models: &mut model::ModelPicker::catalog(models.clone(), picker_draft.clone()),
            switches: &switch::SwitchPicker::new()
                .with_configuration(switch::SwitchEditor::new(models, picker_draft)),
            sessions: &session_picker::SessionPicker::new(SessionSelection::Embedded(
                workflows.clone(),
            ))
            .with_deletion(session_picker::SessionDeletion::new(
                test_runtime.sessions.clone(),
            )),
            agent_instance: &mut attachment,
            session: &mut session,
            capabilities: &mut capabilities,
            extension_tasks: Some(&tasks),
            state: &mut state,
        };
        Box::pin(controller.cycle_switch(1)).await;
        assert_eq!(controller.session.current_switch, "host-only");
        assert_eq!(controller.session.model, "test");
        assert!(controller.session.entries.is_empty());
        assert_eq!(std::fs::read_to_string(settings).unwrap(), content);
        Box::pin(controller.execute_command(
            "/rewind",
            crate::commands::CommandInvocation::Builtin {
                command: crate::commands::BuiltinCommand::RewindPicker,
            },
        ))
        .await;
        assert!(controller.session.entries.is_empty());
        let picker = controller.state.picker.as_ref().unwrap();
        assert!(
            picker
                .items
                .iter()
                .any(|item| item.label.contains("host-anchor"))
        );
        assert!(
            picker
                .items
                .iter()
                .all(|item| !item.label.contains("client-only"))
        );

        assert!(!root.join("client-memory").exists());
        controller.state.editor.set_text("reload draft");
        Box::pin(controller.reload_resources()).await;
        assert_eq!(controller.state.editor.text(), "reload draft");
        assert!(controller.session.entries.is_empty());
        assert_eq!(controller.session.provider, "local");
        assert_eq!(controller.session.model, "test");
        assert_eq!(
            std::fs::read_to_string(root.join(".phi/settings.json")).unwrap(),
            content
        );
        let canonical = controller.agent_instance.transcript().await.unwrap();
        Box::pin(controller.rewind_session("missing-entry")).await;
        assert_eq!(controller.state.editor.text(), "reload draft");
        assert_eq!(controller.session.leaf_id, canonical.leaf_id);
        Box::pin(controller.rewind_session(&user_entry)).await;
        assert_eq!(controller.state.editor.text(), "rewind draft");
        assert!(controller.session.leaf_id.is_none());
        assert!(controller.session.entries.is_empty());
        assert_eq!(
            controller
                .agent_instance
                .transcript()
                .await
                .unwrap()
                .entries
                .len(),
            canonical.entries.len()
        );
        test_runtime
            .sessions
            .stop(
                agent.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn resource_picker_configuration_preserves_existing_save_effects() {
        match std::env::var("PHI_TEST_PICKER_CONFIGURATION")
            .ok()
            .as_deref()
        {
            Some(mode) => characterize_picker_configuration(mode == "protected").await,
            None => {
                for mode in ["local", "protected"] {
                    run_picker_configuration_child(mode);
                }
            }
        }
    }

    fn run_picker_configuration_child(mode: &str) {
        let root = std::env::temp_dir().join(format!("phi-picker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "interactive::tests::resource_picker_configuration_preserves_existing_save_effects",
                "--nocapture",
            ])
            .env("PHI_TEST_PICKER_CONFIGURATION", mode)
            .env("PHI_CONFIG_DIR", root.join("config"))
            .env("PHI_AUTH_DIR", root.join("auth"))
            .env("PHI_SESSION_DIR", root.join("sessions"))
            .env("PHI_OFFLINE", "1")
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn characterize_picker_configuration(protected: bool) {
        let arguments = if protected {
            vec!["phi", "--rpc"]
        } else {
            vec!["phi"]
        };
        let cli = Cli::parse_from(arguments);
        crate::security::initialize(&cli).unwrap();
        crate::security::initialize_file_trust(true).unwrap();
        let path = crate::config::settings::settings_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"temperature":0.25,"modes":["legacy"]}"#).unwrap();
        let project = std::env::current_dir().unwrap().join(".phi/settings.json");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        let content = r#"{"switches":[{"name":"project","provider":"local","model":"test"}]}"#;
        std::fs::write(&project, content).unwrap();
        let mut settings =
            Settings::default().merge_project(serde_json::from_str(content).unwrap());
        let session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        let mut state = UiState::default();
        let (root, _, test_runtime) = crate::runtime::preparation::tests::session_test_runtime();
        let catalog = Arc::new(
            ModelCatalog::load(ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let models = test_runtime.preparation.clone().initialize(
            &root,
            Arc::new(cli.clone()),
            settings.clone(),
            crate::config::SettingsManager::from_settings(
                &root,
                settings.clone(),
                Settings::default(),
            )
            .unwrap(),
            Vec::new(),
            catalog,
        );
        let mut picker_draft = models.picker_seed();
        characterize_model_open(
            &models,
            &path,
            &mut picker_draft,
            &mut state,
            &session,
            protected,
        )
        .await;
        characterize_switch_save(&path, &mut picker_draft, &mut state, &session, protected);
        assert!(settings.default_model.is_none());
        assert_eq!(
            settings.switch_by_name("project").unwrap().model.as_deref(),
            Some("test")
        );
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(std::fs::read_to_string(&project).unwrap(), content);
        characterize_switch_runtime(
            &path,
            &project,
            &mut settings,
            &mut picker_draft,
            &mut state,
        )
        .await;
    }

    async fn characterize_switch_runtime(
        path: &std::path::Path,
        project: &std::path::Path,
        settings: &mut Settings,
        picker_draft: &mut SwitchSettings,
        state: &mut UiState,
    ) {
        std::fs::write(path, "{}").unwrap();
        let (root, policy, test_runtime) =
            crate::runtime::preparation::tests::session_test_runtime();
        let models = test_runtime.preparation.clone().initialize(
            &std::env::current_dir().unwrap(),
            Arc::new(Cli::parse_from(["phi"])),
            settings.clone(),
            crate::config::SettingsManager::load_for(&std::env::current_dir().unwrap()).unwrap(),
            Vec::new(),
            Arc::new(
                ModelCatalog::load(ModelCatalogOptions {
                    store_path: root.join("store.json"),
                    user_paths: vec![root.join("models.json")],
                    offline: true,
                })
                .unwrap(),
            ),
        );
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = std::env::current_dir().unwrap().to_string_lossy().into();
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap();
        let agent = Box::pin(workflows.start_session(session, false))
            .await
            .unwrap();
        let mut attachment = AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        let accepted = agent.current_runtime_view().await;
        let mut session = accepted.session;
        let mut capabilities = accepted.declarations.capabilities;
        let picker_draft = Rc::new(RefCell::new(picker_draft.clone()));
        let tasks = extensions::ExtensionTaskQueue::default();
        state.features = InteractiveFeatures::all_enabled();
        state.editor.set_text("selection draft");
        let mut controller = InteractiveController {
            memory: Some(&policy.memory_administration()),
            navigation: Some(&workflows),
            mcp_auth: Some(&policy.mcp_auth_diagnostics()),
            mcp_read_only: false,
            authentication: None,
            models: &mut model::ModelPicker::catalog(models.clone(), picker_draft.clone()),
            switches: &switch::SwitchPicker::new()
                .with_configuration(switch::SwitchEditor::new(models, picker_draft.clone())),
            sessions: &session_picker::SessionPicker::new(SessionSelection::Embedded(
                workflows.clone(),
            ))
            .with_deletion(session_picker::SessionDeletion::new(
                test_runtime.sessions.clone(),
            )),
            agent_instance: &mut attachment,
            session: &mut session,
            capabilities: &mut capabilities,
            extension_tasks: Some(&tasks),
            state,
        };
        controller
            .select_picker_value("switch:configure".into())
            .await;
        assert!(matches!(
            controller.state.picker.as_ref().unwrap().kind,
            state::PickerKind::SwitchConfigure
        ));
        controller
            .select_picker_value("switch:model:project:model:local:edited".into())
            .await;
        assert!(
            matches!(&controller.state.picker.as_ref().unwrap().kind, state::PickerKind::SwitchEdit { switch_name } if switch_name == "project")
        );
        assert_eq!(controller.session.model, "test");
        controller
            .select_picker_value("switch:delete:project".into())
            .await;
        assert_eq!(
            controller.state.picker_confirming_delete.as_deref(),
            Some("switch:delete:project")
        );
        assert!(picker_draft.borrow().switch_by_name("project").is_some());
        controller.select_picker_value("switch:back".into()).await;
        assert!(matches!(
            controller.state.picker.as_ref().unwrap().kind,
            state::PickerKind::SwitchSelect
        ));
        Box::pin(controller.select_picker_value("switch:project".into())).await;
        assert_eq!(controller.session.current_switch, "project");
        assert_eq!(controller.session.model, "test");
        assert_eq!(
            picker_draft
                .borrow()
                .switch_by_name("project")
                .unwrap()
                .model
                .as_deref(),
            Some("edited")
        );
        assert_eq!(controller.state.editor.text(), "");
        let accepted = agent.current_runtime_view().await;
        assert_eq!(
            serde_json::to_value(&*controller.capabilities).unwrap(),
            serde_json::to_value(&accepted.declarations.capabilities).unwrap()
        );
        let generation = accepted.declarations.generation_id;
        picker_draft
            .borrow_mut()
            .switches
            .as_mut()
            .unwrap()
            .push(crate::config::SwitchConfig {
                name: "draft-only".into(),
                ..Default::default()
            });
        controller.state.editor.set_text("draft-cycle");
        Box::pin(controller.cycle_switch(1)).await;
        assert_configuration_notice(controller.state, NotifyKind::Error, "draft-only");
        assert_eq!(controller.state.editor.text(), "draft-cycle");
        assert_eq!(
            agent
                .current_runtime_view()
                .await
                .declarations
                .generation_id,
            generation
        );
        picker_draft.borrow_mut().switches.as_mut().unwrap().pop();
        let saved = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        controller.state.editor.set_text("failed save draft");
        Box::pin(controller.select_picker_value("switch:project".into())).await;
        assert_eq!(controller.state.editor.text(), "");
        assert_eq!(controller.session.current_switch, "project");
        let accepted = agent.current_runtime_view().await;
        assert_ne!(accepted.declarations.generation_id, generation);
        let generation = accepted.declarations.generation_id;
        std::fs::remove_dir(path).unwrap();
        std::fs::write(path, saved).unwrap();
        std::fs::remove_file(project).unwrap();
        controller.state.editor.set_text("preserved draft");
        Box::pin(controller.select_picker_value("switch:project".into())).await;
        assert_eq!(controller.state.editor.text(), "");
        assert_eq!(controller.session.current_switch, "project");
        assert_eq!(controller.session.model, "test");
        assert_ne!(
            agent
                .current_runtime_view()
                .await
                .declarations
                .generation_id,
            generation
        );
        test_runtime
            .sessions
            .stop(
                agent.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn characterize_model_open(
        models: &crate::runtime::preparation::ModelChoices,
        path: &std::path::Path,
        settings: &mut SwitchSettings,
        state: &mut UiState,
        session: &Session,
        protected: bool,
    ) {
        let before = std::fs::read_to_string(path).unwrap();
        let draft = Rc::new(RefCell::new(settings.clone()));
        let configuration = model::DefaultModelConfiguration::new(draft.clone());
        configuration.prepare(state);
        *settings = draft.borrow().clone();
        let choices = models.model_choices(
            session.provider.as_str(),
            session.model.as_str(),
            settings.default_model(),
        );
        model::open_model_choices(choices, state, session);
        assert!(matches!(
            state.picker.as_ref().unwrap().kind,
            state::PickerKind::Model
        ));
        assert_eq!(settings.default_model(), Some("deepseek/deepseek-v4-flash"));
        if protected {
            assert_configuration_notice(state, NotifyKind::Warning, "read-only");
        } else {
            assert_configuration_notice(state, NotifyKind::Info, "generated defaultModel setting");
        }
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["temperature"], 0.25);
        if protected {
            assert_eq!(std::fs::read_to_string(path).unwrap(), before);
        } else {
            assert_eq!(saved["defaultModel"], settings.default_model().unwrap());
        }
        std::fs::write(path, r#"{"defaultModel":"external","temperature":0.5}"#).unwrap();
        configuration.prepare(state);
        let choices = models.model_choices(
            session.provider.as_str(),
            session.model.as_str(),
            settings.default_model(),
        );
        model::open_model_choices(choices, state, session);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            r#"{"defaultModel":"external","temperature":0.5}"#
        );
    }

    fn characterize_switch_save(
        path: &std::path::Path,
        settings: &mut SwitchSettings,
        state: &mut UiState,
        session: &Session,
        protected: bool,
    ) {
        set_switch_model("project", "model:local:edited", state, session, settings);
        set_switch_alias("project", "alias", state, session, settings);
        assert_eq!(
            settings.switch_by_name("project").unwrap().model.as_deref(),
            Some("edited")
        );
        std::fs::write(
            path,
            r#"{"defaultModel":"external","temperature":0.75,"modes":["legacy"]}"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(path).unwrap();
        save_switches(settings, state);
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["temperature"], 0.75);
        assert_eq!(saved["defaultModel"], "external");
        if protected {
            assert_eq!(std::fs::read_to_string(path).unwrap(), before);
        } else {
            assert_eq!(saved["switches"][0]["name"], "project");
            assert_eq!(saved["switches"][0]["model"], "edited");
            assert_eq!(saved["switches"][0]["alias"], "alias");
            assert!(saved.get("modes").is_none());
        }
        characterize_configuration_save_failures(path, settings, state, protected);
    }

    fn characterize_configuration_save_failures(
        path: &std::path::Path,
        settings: &SwitchSettings,
        state: &mut UiState,
        protected: bool,
    ) {
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        save_switches(settings, state);
        if protected {
            assert_configuration_notice(state, NotifyKind::Warning, "read-only");
        } else {
            assert_configuration_notice(state, NotifyKind::Error, "failed to save switches");
        }
        assert!(path.is_dir());
        assert_eq!(
            settings.switch_by_name("project").unwrap().model.as_deref(),
            Some("edited")
        );
        std::fs::remove_dir(path).unwrap();
        std::fs::write(path, "malformed").unwrap();
        save_switches(settings, state);
        let content = std::fs::read_to_string(path).unwrap();
        assert_eq!(content, "malformed");
        if protected {
            assert_configuration_notice(state, NotifyKind::Warning, "read-only");
        } else {
            assert_configuration_notice(state, NotifyKind::Error, "failed to save switches");
        }
    }

    fn assert_configuration_notice(state: &UiState, kind: NotifyKind, message: &str) {
        let BlockBody::Notify(notice) = &state.conversation.blocks.last().unwrap().block.body
        else {
            panic!("expected a configuration notification");
        };
        assert_eq!(notice.kind, kind);
        assert!(notice.message.contains(message), "{}", notice.message);
    }

    struct CancellationUi {
        sessions: Arc<crate::runtime::SessionAssociations>,
        attachment: AgentAttachment,
        events: tokio::sync::broadcast::Receiver<AgentEvent>,
        state: UiState,
        session: Session,
    }

    impl CancellationUi {
        async fn submit(&mut self, text: &str) {
            self.state.editor.clear();
            submit_prompt(
                phi_sdk::turn::TurnPrompt {
                    prompt: text.into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                },
                text.into(),
                text.into(),
                &mut self.state,
                &mut self.session,
                &self.attachment,
                phi_sdk::turn::SubmissionDispatch::FollowUp,
                None,
            )
            .await;
        }

        async fn drain_until(&mut self, ready: impl Fn(&Self) -> bool) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    drain_agent_events(
                        &self.attachment,
                        &mut self.events,
                        &mut self.state,
                        &mut self.session,
                    )
                    .await;
                    if ready(self) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }

        fn paint(&mut self) -> BuiltUiFrame {
            let frame = build_ui_frame(
                &mut RenderTarget::Main {
                    state: &mut self.state,
                    session: &self.session,
                },
                RenderArea {
                    width: 80,
                    height: Some(24),
                },
                &Theme::default(),
            );
            self.state.conversation.acknowledge_history_render(
                frame.history_revision,
                frame.first_block,
                frame.retire_before,
            );
            self.state.painted_conversation = Some(self.state.conversation.source().clone());
            frame
        }
    }

    async fn read_provider_request(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= start + 4 + length {
                    return stream;
                }
            }
        }
    }

    async fn cancellation_provider(listener: TcpListener) {
        for text in ["partial output\n", "next answer\n"] {
            let mut stream = read_provider_request(&listener).await;
            let chunk = serde_json::json!({"id":"completion","model":"test","choices":[{
                "delta":{"content":text},"finish_reason":null
            }]});
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {chunk}\n\n"
            ).as_bytes()).await.unwrap();
            if text == "partial output\n" {
                let _ = stream.read(&mut [0; 1]).await;
            } else {
                let end = serde_json::json!({"id":"completion","model":"test","choices":[{
                    "delta":{},"finish_reason":"stop"
                }]});
                stream
                    .write_all(format!("data: {end}\n\ndata: [DONE]\n\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    }

    async fn create_cancellation_ui(
        root: &std::path::Path,
        address: std::net::SocketAddr,
        history: usize,
    ) -> CancellationUi {
        create_test_ui(
            root,
            address,
            history,
            crate::AgentBuildOptions {
                no_tools: true,
                ..Default::default()
            },
        )
        .await
    }

    async fn create_test_ui(
        root: &std::path::Path,
        address: std::net::SocketAddr,
        history: usize,
        agent_options: crate::AgentBuildOptions,
    ) -> CancellationUi {
        let models = root.join("models.json");
        std::fs::write(
            &models,
            serde_json::json!({"providers":{"local":{
                "api":"openai-completions","baseUrl":format!("http://{address}/v1"),
                "models":{"test":{"contextWindow":128000}}
            }}})
            .to_string(),
        )
        .unwrap();
        let catalog = Arc::new(
            ModelCatalog::load(ModelCatalogOptions {
                store_path: root.join("catalog.json"),
                user_paths: vec![models],
                offline: true,
            })
            .unwrap(),
        );
        let resources = RuntimeResources::new(
            PhiRuntimeServices::new().model_catalog(catalog.clone()),
            Arc::new(ExtensionRuntime::load()),
            catalog.clone(),
            Sandbox::new(false, root.to_path_buf()).unwrap(),
        );
        let sessions = crate::runtime::SessionAssociations::new(
            phi_sdk::agent_manager::AgentManager::new(),
            Arc::new(phi_sdk::workflow::WorkflowRuntime::default()),
        );
        let preparation =
            crate::runtime::RuntimePreparation::new(AgentRuntimeFactory::new(resources));
        let cli = Cli::parse_from(["phi"]);
        let mut config = Settings::default();
        config.memory = Some(phi_sdk::extension::memory::MemoryConfig {
            enabled: false,
            ..Default::default()
        });
        config.compact_enabled = Some(false);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let mut state = UiState::default();
        state.features.activities = true;
        state.conversation.blocks_expanded = Some(false);
        state
            .conversation
            .transcript_tail
            .set_history_budget(HistoryBudget {
                terminal_bytes: 400_000,
                ..Default::default()
            });
        for index in 0..history {
            let prompt = format!("previous prompt {index}\n{}", "history line\n".repeat(10));
            session.transcript.append_user_message(&prompt);
            state.conversation.push_text_block(BlockRole::User, prompt);
        }
        let agent_session = sessions
            .create(
                &preparation,
                session.clone(),
                RuntimeRebuildOptions {
                    phi: PhiBuildOptions {
                        cli: &cli,
                        provider: "local",
                        model: "test",
                        runtime_cfg: config.runtime_config(&catalog),
                        context: phi_sdk::context::load(true),
                        agent_options,
                        hooks: Vec::new(),
                    },
                    instance: AgentInstanceBuildOptions::from_session(&session),
                },
                SessionStartReason::New,
                false,
            )
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &agent_session,
            sessions.session_access(agent_session.instance().id()),
            preparation,
        );
        let events = attachment.subscribe();
        CancellationUi {
            sessions,
            attachment,
            events,
            state,
            session,
        }
    }

    #[tokio::test]
    async fn terminal_submission_projects_once_after_commit() {
        let root = std::env::temp_dir().join(format!("phi-input-view-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut ui = create_cancellation_ui(&root, listener.local_addr().unwrap(), 0).await;
        ui.submit("terminal input").await;
        ui.drain_until(|ui| {
            ui.state
                .conversation
                .blocks
                .iter()
                .any(|block| matches!(&block.block.body, BlockBody::Text(text) if text == "terminal input"))
        })
        .await;
        let input_count = ui
            .state
            .conversation
            .blocks
            .iter()
            .filter(|block| matches!(&block.block.body, BlockBody::Text(text) if text == "terminal input"))
            .count();
        assert_eq!(input_count, 1);
        ui.sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn parallel_bash_panel_handoff() {
        use phi_sdk::agent_instance::{BashMode, BashStatus};

        const GATED_OUTPUT: &str = "sleep 0.01; printf ready; while [ ! -f release ]; do sleep 0.01; done; printf next; sleep 60";

        let root = std::env::temp_dir().join(format!("phi-bash-shortcut-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut ui = create_test_ui(
            &root,
            listener.local_addr().unwrap(),
            0,
            crate::AgentBuildOptions::default(),
        )
        .await;
        let provider = tokio::spawn(async move {
            let mut stream = read_provider_request(&listener).await;
            let reply = serde_json::json!({"id":"completion","model":"test","choices":[{
                "delta":{"tool_calls":([0, 1].map(|index| serde_json::json!({
                    "index":index,"id":format!("bash-{index}"),"type":"function",
                    "function":{"name":"Bash","arguments":serde_json::json!({"command": if index == 0 { GATED_OUTPUT } else { "sleep 60" }}).to_string()}
                }))) },
                "finish_reason":"tool_calls"
            }]});
            let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        ui.submit("run both commands").await;
        ui.drain_until(|ui| {
            ui.attachment
                .snapshot()
                .bash_executions
                .iter()
                .filter(|execution| execution.status == BashStatus::Running)
                .count()
                == 2
        })
        .await;
        let before = ui.attachment.snapshot().bash_executions;
        assert_eq!(before.len(), 2);
        assert!(
            before
                .iter()
                .all(|execution| execution.mode == BashMode::Foreground)
        );
        assert_eq!(
            ui.state.runtime.dispatch_key(KeyInput::Ctrl('b')),
            phi_tui::InputOutcome::Action(UiAction::BackgroundRunningBash)
        );
        ui.state.editor.replace_text("retained Bash draft");
        activities::open_current_bash(&ui.attachment, ui.attachment.id().as_str(), &mut ui.state)
            .await;
        assert_eq!(
            ui.state.runtime.dispatch_key(KeyInput::Esc),
            phi_tui::InputOutcome::Action(UiAction::CloseBashSelector)
        );
        ui.state.close_bash_selector();
        assert_eq!(ui.state.editor.text(), "retained Bash draft");
        assert_eq!(ui.attachment.snapshot().bash_executions, before);
        activities::open_current_bash(&ui.attachment, ui.attachment.id().as_str(), &mut ui.state)
            .await;
        assert_eq!(ui.attachment.snapshot().bash_executions, before);
        assert!(ui.state.active_overlay.is_none());
        assert_eq!(
            ui.attachment
                .activities()
                .await
                .unwrap()
                .bash_executions
                .len(),
            2
        );
        assert_eq!(ui.state.bash_views.len(), 2);
        let theme = phi_tui::Theme::default();
        let lines =
            components::status::render(&ui.state, &phi_tui::RenderContext::width(120, &theme));
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("Background Bash"))
        );
        assert!(lines.iter().any(|line| line.text.contains("sleep 60")));
        ui.state.runtime.dispatch_key(KeyInput::Down);
        let phi_tui::InputOutcome::Action(UiAction::BackgroundBash { owner, id }) =
            ui.state.runtime.dispatch_key(KeyInput::Enter)
        else {
            panic!("expected selected Bash handoff");
        };
        assert_eq!(id, before[1].id);
        activities::background_bash(&ui.attachment, &owner, &id, &mut ui.state).await;
        assert_eq!(ui.state.bash_views.len(), 2);
        assert!(ui.state.active_overlay.is_none());
        assert!(
            !components::status::render(&ui.state, &phi_tui::RenderContext::width(120, &theme))
                .iter()
                .any(|line| line.text.contains("Background Bash"))
        );
        let after = ui.attachment.snapshot().bash_executions;
        assert_eq!(after.len(), 2);
        for execution in &after {
            assert_eq!(execution.status, BashStatus::Running);
            assert_eq!(
                execution.mode,
                if execution.id == id {
                    BashMode::Background
                } else {
                    BashMode::Foreground
                }
            );
        }
        ui.state.activity_selection = Some(activities::ActivitySelection::Bash {
            owner: owner.clone(),
            id: id.clone(),
        });
        overlays::open_tasks(&mut ui.state);
        let overlays::ActiveOverlay::Tasks(panel) = ui
            .state
            .runtime
            .widget(ui.state.active_overlay.unwrap())
            .unwrap()
        else {
            panic!("expected managed commands overlay");
        };
        assert_eq!(
            panel
                .render_lines(120, 24)
                .iter()
                .filter(|line| line.text.contains("[bash]"))
                .count(),
            2
        );
        let mut refresh = activities::TaskRefresh::default();
        refresh
            .poll(
                &ui.attachment,
                &mut ui.state,
                activities::TaskVisibility::Hidden,
            )
            .await;
        let gated = ui
            .state
            .bash_views
            .iter()
            .find(|view| view.execution.command == GATED_OUTPUT)
            .unwrap()
            .clone();
        activities::open_bash_output(
            &ui.attachment,
            gated.owner,
            &gated.execution.id,
            &mut ui.state,
        )
        .await;
        std::fs::write(root.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                refresh.poll(&ui.attachment, &mut ui.state, activities::TaskVisibility::Hidden).await;
                if ui.state.active_overlay.is_some_and(|handle| {
                    matches!(ui.state.runtime.widget(handle), Some(overlays::ActiveOverlay::BashDetails(panel)) if panel.output == "readynext")
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("Bash output notifications must refresh the open detail view");
        activities::stop_bash(&ui.attachment, &owner, &id, &mut ui.state).await;
        assert!(
            ui.attachment
                .snapshot()
                .bash_executions
                .iter()
                .any(|execution| { execution.id == id && execution.status == BashStatus::Stopped })
        );
        let remaining = before.iter().find(|execution| execution.id != id).unwrap();
        activities::open_current_bash(&ui.attachment, ui.attachment.id().as_str(), &mut ui.state)
            .await;
        assert!(ui.state.bash_selector.is_some());
        activities::stop_bash(&ui.attachment, &owner, &remaining.id, &mut ui.state).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                refresh
                    .poll(
                        &ui.attachment,
                        &mut ui.state,
                        activities::TaskVisibility::Hidden,
                    )
                    .await;
                if ui.state.bash_selector.is_none() && ui.state.bash_views.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finished foreground Bash must leave the list and close its selector");
        assert_eq!(ui.state.bash_views[0].execution.id, id);
        assert_eq!(ui.state.bash_views[0].execution.status, BashStatus::Stopped);
        assert_eq!(ui.state.editor.text(), "retained Bash draft");
        ui.sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        provider.abort();
        let _ = provider.await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn idle_refresh_skips_render() {
        const REFRESH_WITH_MARGIN: Duration = Duration::from_millis(1100);

        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-idle-activities-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut ui = create_cancellation_ui(&root, listener.local_addr().unwrap(), 0).await;
        let mut refresh = activities::TaskRefresh::default();
        refresh
            .poll(
                &ui.attachment,
                &mut ui.state,
                activities::TaskVisibility::Hidden,
            )
            .await;
        tokio::time::sleep(REFRESH_WITH_MARGIN).await;
        let changed = refresh
            .poll(
                &ui.attachment,
                &mut ui.state,
                activities::TaskVisibility::Hidden,
            )
            .await;
        ui.sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(!changed);
    }

    #[tokio::test]
    async fn task_refresh_is_passive_and_rejects_outgoing_session_reads() {
        async fn wait_for_tasks(
            refresh: &mut activities::TaskRefresh,
            attachment: &AgentAttachment,
            state: &mut UiState,
            expected: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    refresh
                        .poll(attachment, state, activities::TaskVisibility::Visible)
                        .await;
                    state.tasks.expanded = true;
                    if state
                        .tasks
                        .progress_lines(160)
                        .iter()
                        .any(|line| line.text.contains(expected))
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let root = std::path::PathBuf::from(format!("/tmp/phi-task-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("incoming")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut outgoing = create_cancellation_ui(&root, listener.local_addr().unwrap(), 0).await;
        let incoming =
            create_cancellation_ui(&root.join("incoming"), listener.local_addr().unwrap(), 0).await;
        let mut paths = Vec::new();
        for (ui, subject) in [
            (&outgoing, "Outgoing review"),
            (&incoming, "Incoming review"),
        ] {
            let session = ui.attachment.session().await.unwrap();
            let directory = crate::config::settings::phi_path()
                .join("tasks")
                .join(session.task_list_id.as_deref().unwrap());
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("1.json"), serde_json::to_vec(&serde_json::json!({
                "id": "1", "subject": subject, "description": "Inspect work", "status": "in_progress",
                "owner": {"instance_id": ui.attachment.id()}, "blocked_by": [], "metadata": {}
            })).unwrap()).unwrap();
            paths.push(directory);
        }
        outgoing.state.editor.replace_text("retained task draft");
        let mut refresh = activities::TaskRefresh::default();
        wait_for_tasks(
            &mut refresh,
            &outgoing.attachment,
            &mut outgoing.state,
            "Outgoing review",
        )
        .await;
        overlays::open_tasks(&mut outgoing.state);
        let mut subagents = SubagentProjection::default();
        activities::open_owner(
            "unrelated-instance".into(),
            &mut subagents,
            &outgoing.attachment,
            &mut outgoing.state,
        )
        .await;
        assert!(activities::tasks_open(&outgoing.state));
        assert!(subagents.active_subagent().is_none());
        activities::open_owner(
            outgoing.attachment.id().as_str().into(),
            &mut subagents,
            &outgoing.attachment,
            &mut outgoing.state,
        )
        .await;
        assert!(!activities::tasks_open(&outgoing.state));
        refresh
            .poll(
                &outgoing.attachment,
                &mut outgoing.state,
                activities::TaskVisibility::Visible,
            )
            .await;
        std::fs::write(paths[0].join("1.json"), b"invalid").unwrap();
        overlays::open_tasks(&mut outgoing.state);
        wait_for_tasks(
            &mut refresh,
            &outgoing.attachment,
            &mut outgoing.state,
            "Tasks not refreshed",
        )
        .await;
        assert!(
            outgoing
                .state
                .tasks
                .progress_lines(160)
                .iter()
                .any(|line| line.text.contains("Outgoing review"))
        );
        activities::close_tasks(&mut outgoing.state);
        refresh
            .poll(
                &outgoing.attachment,
                &mut outgoing.state,
                activities::TaskVisibility::Visible,
            )
            .await;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths[0].join(".lock"))
            .unwrap();
        lock.lock().unwrap();
        overlays::open_tasks(&mut outgoing.state);
        tokio::time::timeout(
            Duration::from_millis(100),
            refresh.poll(
                &outgoing.attachment,
                &mut outgoing.state,
                activities::TaskVisibility::Visible,
            ),
        )
        .await
        .unwrap();
        tokio::task::yield_now().await;
        refresh
            .poll(
                &incoming.attachment,
                &mut outgoing.state,
                activities::TaskVisibility::Visible,
            )
            .await;
        assert!(!activities::tasks_open(&outgoing.state));
        assert!(outgoing.state.tasks.progress_lines(160).is_empty());
        drop(lock);
        wait_for_tasks(
            &mut refresh,
            &incoming.attachment,
            &mut outgoing.state,
            "Incoming review",
        )
        .await;
        assert!(
            outgoing
                .state
                .tasks
                .progress_lines(160)
                .iter()
                .all(|line| !line.text.contains("Outgoing"))
        );
        assert_eq!(outgoing.state.editor.text(), "retained task draft");
        assert!(outgoing.state.conversation.blocks.is_empty());
        for ui in [&outgoing, &incoming] {
            assert!(ui.attachment.transcript().await.unwrap().entries.is_empty());
            assert_eq!(
                ui.attachment.snapshot().status,
                phi_sdk::agent_instance::AgentInstanceStatus::Idle
            );
            ui.sessions
                .stop(
                    ui.attachment.id(),
                    phi_sdk::lifecycle::SessionEndReason::Quit,
                )
                .await
                .unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn esc_then_continue_preserves_content_identity_and_retained_history() {
        for history in [0, 250] {
            let root = std::path::PathBuf::from(format!(
                "/tmp/phi-cancellation-ui-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut ui =
                create_cancellation_ui(&root, listener.local_addr().unwrap(), history).await;
            let provider = tokio::spawn(cancellation_provider(listener));
            ui.paint();
            // Reserve headroom after retirement so new notices do not require budget eviction.
            ui.state
                .conversation
                .transcript_tail
                .set_history_budget(HistoryBudget::default());
            ui.state.notify(NotifyKind::Info, "previous notice");
            ui.submit("first prompt").await;
            ui.drain_until(|ui| ui.state.conversation.live_assistant_block().is_some())
                .await;
            ui.submit("queued follow-up").await;
            ui.state.editor.replace_text("retained draft");
            let before = ui.paint();
            let ids = ui
                .state
                .conversation
                .blocks
                .iter()
                .map(|block| block.id)
                .collect::<Vec<_>>();
            let assistant = ui.state.conversation.live_assistant_block().unwrap();
            if history > 0 {
                assert!(ids.len() < history);
            }

            cancel_run(&ui.attachment, &mut ui.state).await;
            assert_eq!(
                ui.state.conversation.live_assistant_block(),
                Some(assistant)
            );
            ui.drain_until(|ui| !ui.state.conversation.run_active_projection())
                .await;
            let cancelled = ui.paint();
            assert_eq!(cancelled.first_block, before.first_block);
            assert!(!cancelled.replace_history);
            assert_eq!(
                ui.state.conversation.blocks[..ids.len()]
                    .iter()
                    .map(|block| block.id)
                    .collect::<Vec<_>>(),
                ids
            );
            assert!(
                ui.state
                    .conversation
                    .blocks
                    .iter()
                    .any(|block| block.id == assistant
                        && matches!(
                            &block.block.body, BlockBody::Text(text) if text == "partial output\n"
                        ))
            );
            assert!(ui.state.conversation.live_run.is_none());
            assert!(!ui.state.agent_recovery_pending);
            assert!(ui.state.queued_submissions.is_empty());
            assert_eq!(ui.state.editor.text(), "queued follow-up\n\nretained draft");

            ui.submit("continue").await;
            ui.drain_until(|ui| {
                !ui.state.conversation.run_active_projection()
                    && ui.state.conversation.blocks.iter().any(|block| {
                        matches!(
                            &block.block.body, BlockBody::Text(text) if text == "next answer\n"
                        )
                    })
            })
            .await;
            let continued = ui.paint();
            assert_eq!(continued.first_block, before.first_block);
            assert!(!continued.replace_history);
            assert_eq!(ui.state.conversation.blocks_expanded, Some(false));
            assert!(ui.state.conversation.blocks.iter().any(|block| matches!(
                &block.block.body, BlockBody::Notify(notice) if notice.message == "previous notice"
            )));
            assert_eq!(ui.state.conversation.blocks.iter().filter(|block| matches!(
                &block.block.body, BlockBody::Styled(lines) if lines[0].text == "Operation canceled"
            )).count(), 1);
            let transcript = ui.attachment.transcript().await.unwrap();
            assert!(transcript.entries.iter().any(|entry| matches!(
                &entry.kind, crate::session::TranscriptEntryKind::Message {
                    message: crate::session::AgentMessage::Assistant { stop_reason, .. }
                } if stop_reason == "aborted"
            )));
            ui.state.agent_recovery_pending = true;
            ui.drain_until(|ui| !ui.state.agent_recovery_pending).await;
            assert!(ui.state.conversation.blocks.iter().any(|block| matches!(
                &block.block.body, BlockBody::Text(text) if text == "partial output\n"
            )));
            assert!(ui.state.conversation.blocks.iter().any(|block| matches!(
                &block.block.body, BlockBody::Notify(notice) if notice.message == "previous notice"
            )));
            assert!(ui.state.conversation.live_run.is_none());
            let recovered = ui.paint();
            let recovered_ids = ui
                .state
                .conversation
                .blocks
                .iter()
                .map(|block| block.id)
                .collect::<Vec<_>>();
            ui.state
                .conversation
                .push_text_block(BlockRole::User, "continue after recovery");
            let appended = ui.paint();
            assert_eq!(appended.first_block, recovered.first_block);
            assert!(!appended.replace_history);
            assert_eq!(
                ui.state.conversation.blocks[..recovered_ids.len()]
                    .iter()
                    .map(|block| block.id)
                    .collect::<Vec<_>>(),
                recovered_ids
            );
            ui.sessions
                .stop(
                    ui.attachment.id(),
                    phi_sdk::lifecycle::SessionEndReason::Quit,
                )
                .await
                .unwrap();
            provider.await.unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn failed_abort_preserves_live_routes_queue_and_editor() {
        let root = std::path::PathBuf::from(format!(
            "/tmp/phi-cancellation-failure-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut ui = create_cancellation_ui(&root, listener.local_addr().unwrap(), 0).await;
        ui.sessions
            .stop(
                ui.attachment.id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        ui.state.conversation.project_main_run_started();
        let block = ui
            .state
            .conversation
            .append_live_assistant("retained output");
        ui.state.editor.replace_text("retained draft");
        ui.state
            .queued_submissions
            .push_back(state::QueuedSubmissionView {
                submission_id: "pending".into(),
                prompt: "accepted text".into(),
                display: "display text".into(),
                editor_prompt: "original input".into(),
                dispatch: phi_sdk::turn::SubmissionDispatch::FollowUp,
            });

        cancel_run(&ui.attachment, &mut ui.state).await;

        assert!(ui.state.conversation.run_active_projection());
        assert_eq!(ui.state.conversation.live_assistant_block(), block);
        assert_eq!(ui.state.queued_submissions.len(), 1);
        assert_eq!(ui.state.editor.text(), "retained draft");
        assert!(
            matches!(&ui.state.conversation.blocks.last().unwrap().block.body,
                BlockBody::Notify(notice) if notice.kind == NotifyKind::Error && notice.message.starts_with("failed to cancel run:")
            )
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
