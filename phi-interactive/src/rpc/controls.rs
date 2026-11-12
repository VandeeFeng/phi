use std::sync::Arc;

use phi_sdk::agent_instance::AgentInstanceId;
use serde_json::{Value, json};

use crate::runtime::preparation::{
    CreateSessionRequest, ModelChoices, RuntimeSelection, SessionWorkflows,
};
use crate::runtime::session::AgentSession;
use crate::runtime::{RuntimePreparation, RuntimeView, SessionAssociations};

use super::messages::{Code, Control, Fault, Selection, Startup};

pub(super) async fn initialize(
    sessions: Arc<SessionAssociations>,
    preparation: Arc<RuntimePreparation>,
    startup: Startup,
) -> Result<Arc<AgentSession>, Fault> {
    let workflows = SessionWorkflows::new(sessions, preparation).map_err(Fault::operation)?;
    match startup {
        Startup::New {
            persistence,
            provider,
            model,
            switch,
        } => {
            workflows
                .create_session(CreateSessionRequest {
                    provider,
                    model,
                    switch,
                    persist: persistence.saved(),
                })
                .await
        }
        Startup::Resume {
            persistence,
            session_id,
        } => {
            workflows
                .start_saved_session(&session_id, persistence.saved())
                .await
        }
    }
    .map_err(|error| Fault::new(Code::StartupFailed, error))
}

pub(super) fn identity(view: &RuntimeView) -> Value {
    json!({"generation_id":view.declarations.generation_id,"provider":view.session.provider,"model":view.session.model})
}

fn accepted(view: RuntimeView) -> Value {
    json!({"runtime":identity(&view), "agent_diagnostics":view.declarations.capabilities.agent_diagnostics,
        "prompt_diagnostics":view.declarations.capabilities.prompt_diagnostics,
        "persistence_warning":view.snapshot.persistence_error})
}

pub(super) async fn execute(
    root: Arc<AgentSession>,
    sessions: Arc<SessionAssociations>,
    preparation: Arc<RuntimePreparation>,
    models: ModelChoices,
    control: Control,
) -> Result<Value, Fault> {
    let access = sessions.session_access(root.instance().id());
    match control {
        Control::Interrupt => root
            .instance()
            .abort()
            .await
            .map(|_| json!({}))
            .map_err(Fault::operation),
        Control::RuntimeInfo => {
            let view = root.current_runtime_view().await;
            let choices = models.model_choices(&view.session.provider, &view.session.model, None);
            let agents = root
                .instance()
                .supported_agents()
                .await
                .map_err(Fault::operation)?;
            let agents = agents
                .all()
                .iter()
                .map(|agent| agent.name().to_string())
                .collect::<Vec<_>>();
            let prompts = view
                .declarations
                .commands
                .iter()
                .filter(|command| {
                    command.source == crate::commands::SlashCommandSource::User
                        || command.name.starts_with("skill:")
                })
                .map(|command| &command.name)
                .collect::<Vec<_>>();
            Ok(
                json!({"runtime":identity(&view),"models":choices.into_iter().map(|choice| json!({"provider":choice.provider,"model":choice.model})).collect::<Vec<_>>(),
                "switches":models.picker_seed().switch_names(),"prompts":prompts,
                "agents":agents,"agent_diagnostics":view.declarations.capabilities.agent_diagnostics,
                "prompt_diagnostics":view.declarations.capabilities.prompt_diagnostics}),
            )
        }
        Control::Configure { selection } => preparation
            .configure_runtime(
                &access,
                match selection {
                    Selection::Model { selection } => RuntimeSelection::Model { selection },
                    Selection::Switch { name } => RuntimeSelection::Switch { name },
                },
            )
            .await
            .map(accepted)
            .map_err(Fault::operation),
        Control::Reload => preparation
            .reload_resources(&access)
            .await
            .map(accepted)
            .map_err(Fault::operation),
        Control::ExpandPrompt { text } => {
            let expanded = root.expand_prompt(&text).map_err(Fault::operation)?;
            Ok(
                json!({"text":expanded.expanded_prompt.unwrap_or(text),"tool_policy":expanded.tool_policy}),
            )
        }
        Control::WorkflowStop { run_id } => {
            let snapshot = sessions
                .stop_scoped_workflow(&root, run_id)
                .await
                .map_err(Fault::operation)?
                .ok_or_else(|| {
                    Fault::new(
                        Code::InvalidTarget,
                        "Workflow is outside the controlled root",
                    )
                })?;
            Ok(json!({"run_id": snapshot.id(), "status": snapshot.status()}))
        }
        Control::TaskStop { task_id } => {
            let (parent, instance_id) = find_task(&root, &sessions, &task_id).await?;
            let cancelled = parent
                .instance()
                .cancel_subagent(instance_id)
                .await
                .map_err(Fault::operation)?;
            if !cancelled {
                return Err(Fault::new(
                    Code::InvalidTarget,
                    "task binding is no longer live",
                ));
            }
            Ok(json!({}))
        }
        Control::Initialize { .. } | Control::Close { .. } => Err(Fault::new(
            Code::InvalidMessage,
            "control is not an execution operation",
        )),
    }
}

async fn find_task(
    root: &Arc<AgentSession>,
    sessions: &Arc<SessionAssociations>,
    task_id: &str,
) -> Result<(Arc<AgentSession>, AgentInstanceId), Fault> {
    let mut parents = vec![root.clone()];
    let mut visited = std::collections::HashSet::new();
    while let Some(parent) = parents.pop() {
        if !visited.insert(parent.instance().id().clone()) {
            continue;
        }
        let snapshots = parent
            .instance()
            .subagent_snapshots()
            .await
            .map_err(Fault::operation)?;
        for snapshot in snapshots {
            if snapshot.status.admission != phi_sdk::agents::SubagentAdmission::Accepted {
                continue;
            }
            let creation = snapshot.status.creation;
            let child = sessions
                .get(&AgentInstanceId::new(creation.instance_id().as_str()))
                .ok()
                .filter(|child| {
                    snapshot.executor_registration_id.is_some()
                        && snapshot.executor_registration_id == child.instance().registration_id()
                        && child.creator_id() == Some(parent.instance().id())
                });
            if snapshot.initial_turn_id.as_deref() == Some(task_id) {
                return child
                    .filter(|child| {
                        snapshot.status.outcome.is_none()
                            && child
                                .instance()
                                .subscribe_finalization()
                                .is_some_and(|status| {
                                    matches!(
                                        *status.borrow(),
                                        phi_sdk::agent_manager::AgentFinalizationStatus::Open
                                    )
                                })
                    })
                    .map(|_| (parent, creation.instance_id().clone()))
                    .ok_or_else(|| {
                        Fault::new(Code::InvalidTarget, "task binding is no longer live")
                    });
            }
            if let Some(child) = child {
                parents.push(child);
            }
        }
    }
    Err(Fault::new(
        Code::InvalidTarget,
        "task is outside the controlled root",
    ))
}
