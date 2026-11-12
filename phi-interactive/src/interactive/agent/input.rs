use std::time::Duration;

use phi_tui::{KeyInput, RuntimeEvent};

use super::SubagentProjection;
use crate::interactive::components::input::{InputSurfaceKind, UiAction};
use crate::interactive::components::panel::activities;
use crate::interactive::state::UiState;
use crate::runtime::attachment::AgentAttachment;

pub(in crate::interactive) enum ActiveSubagentAction {
    None,
    Render,
    Quit,
}

pub(in crate::interactive) async fn handle_active_subagent_input(
    subagents: &mut SubagentProjection,
    agent_instance: &AgentAttachment,
    state: &mut UiState,
    timeout: Duration,
) -> anyhow::Result<Option<ActiveSubagentAction>> {
    if state.interaction.is_some()
        || state.bash_selector.is_some()
        || state.active_overlay.is_some()
    {
        return Ok(None);
    }
    if subagents.active_subagent().is_none() {
        return Ok(None);
    }
    state.input_surface.activate(
        &mut state.runtime,
        InputSurfaceKind::Subagent,
        phi_tui::InputContext::Transcript,
    );
    let event = state.runtime.poll_event_with_timeout(timeout)?;
    let key = match event {
        RuntimeEvent::Key(key) => match state.runtime.dispatch_key(key) {
            phi_tui::InputOutcome::Action(UiAction::SurfaceKey {
                surface: InputSurfaceKind::Subagent,
                key,
            }) => Some(key),
            _ => None,
        },
        RuntimeEvent::Resize => return Ok(Some(ActiveSubagentAction::Render)),
        RuntimeEvent::Idle | RuntimeEvent::TerminalFocus(_) => None,
    };
    match key {
        Some(KeyInput::Ctrl('t')) => {
            activities::toggle_tasks(state);
            Ok(Some(ActiveSubagentAction::Render))
        }
        Some(KeyInput::Ctrl('b')) => {
            let owner = subagents
                .active_subagent()
                .expect("active subagent")
                .to_owned();
            activities::open_current_bash(agent_instance, &owner, state).await;
            Ok(Some(ActiveSubagentAction::Render))
        }
        _ => handle_subagent_key(key, subagents, agent_instance).await,
    }
}

pub(in crate::interactive) async fn handle_subagent_key(
    key: Option<KeyInput>,
    subagents: &mut SubagentProjection,
    agent_instance: &AgentAttachment,
) -> anyhow::Result<Option<ActiveSubagentAction>> {
    let Some(id) = subagents.active_subagent().map(str::to_string) else {
        return Ok(None);
    };
    match key {
        Some(KeyInput::Esc) => {
            subagents.switch_to_main();
            Ok(Some(ActiveSubagentAction::Render))
        }
        Some(KeyInput::Ctrl('c')) => {
            let is_working = subagents.active_mut().is_some_and(|run| run.is_working());
            if is_working {
                subagents.cancel(&id, agent_instance).await?;
                Ok(Some(ActiveSubagentAction::Render))
            } else {
                Ok(Some(ActiveSubagentAction::Quit))
            }
        }
        Some(KeyInput::Ctrl('o')) => {
            if let Some(run) = subagents.active_mut() {
                return Ok(Some(if run.toggle_all_collapsible_blocks() {
                    ActiveSubagentAction::Render
                } else {
                    ActiveSubagentAction::None
                }));
            }
            Ok(Some(ActiveSubagentAction::None))
        }
        Some(_) | None => Ok(Some(ActiveSubagentAction::None)),
    }
}
