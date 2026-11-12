use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use phi_sdk::agent_instance::{AgentInstanceEvent, AgentInstanceId, AgentInstanceStatus};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::runtime::SessionAssociations;
use crate::runtime::session::AgentSession;

use super::messages::{Code, Fault};

pub(super) enum Observation {
    Event(Arc<AgentSession>, AgentInstanceEvent),
    Children(Vec<(Arc<AgentSession>, Option<String>, Visibility)>),
    Workflow(Arc<AgentSession>, Value),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Visibility {
    Execution,
    Permissions,
}

#[derive(Default)]
struct WorkflowObservations {
    payloads: HashMap<phi_sdk::workflow::WorkflowRunId, Value>,
    workers: HashSet<phi_sdk::agent_manager::AgentRegistrationId>,
}

pub(super) fn subscribe(
    agent: Arc<AgentSession>,
    sessions: Arc<SessionAssociations>,
    sender: mpsc::Sender<Observation>,
    stop: oneshot::Receiver<()>,
    visibility: Visibility,
) -> impl std::future::Future<Output = Result<(), Fault>> + Send + 'static {
    // Subscribe before spawning so readiness cannot overtake the observation boundary.
    let events = agent.instance().subscribe();
    let workflow_changed = agent.workflow_changed();
    async move {
        observe(
            agent,
            sessions,
            sender,
            events,
            stop,
            visibility,
            workflow_changed,
        )
        .await
    }
}

fn publish(sender: &mpsc::Sender<Observation>, observation: Observation) -> Result<(), Fault> {
    sender
        .try_send(observation)
        .map_err(|error| Fault::new(Code::ObservationLost, error))
}

async fn observe(
    agent: Arc<AgentSession>,
    sessions: Arc<SessionAssociations>,
    sender: mpsc::Sender<Observation>,
    mut events: broadcast::Receiver<AgentInstanceEvent>,
    mut stop: oneshot::Receiver<()>,
    visibility: Visibility,
    workflow_changed: tokio::sync::futures::OwnedNotified,
) -> Result<(), Fault> {
    let mut workflow_changed = Box::pin(workflow_changed);
    let boundary = uuid::Uuid::new_v4().to_string();
    let view = agent
        .instance()
        .view_with_event_boundary(boundary.clone())
        .await;
    let view = match view {
        Ok(view) => view,
        Err(phi_sdk::agent_instance::AgentInstanceError::Stopped)
            if visibility == Visibility::Permissions =>
        {
            return publish(
                &sender,
                Observation::Event(agent, AgentInstanceEvent::Stopped),
            );
        }
        Err(error) => return Err(Fault::new(Code::ObservationLost, error)),
    };
    let mut initial_progress = view.snapshot.progress;
    for pending in view.snapshot.pending_interactions {
        publish(
            &sender,
            Observation::Event(
                agent.clone(),
                AgentInstanceEvent::InteractionRequested(pending),
            ),
        )?;
    }
    discover(&agent, &sessions, &sender, visibility).await?;
    let mut runs = WorkflowObservations::default();
    workflows(&agent, &sessions, &sender, &mut runs, visibility)?;
    loop {
        tokio::select! {
            _ = &mut stop => return Ok(()),
            _ = &mut workflow_changed => {
                // Arm before reading so a concurrent state change cannot lose its wakeup.
                workflow_changed.set(agent.workflow_changed());
                workflows(&agent, &sessions, &sender, &mut runs, visibility)?;
            }
            event = events.recv() => {
                let event = event.map_err(|error| Fault::new(Code::ObservationLost, error))?;
                if matches!(&event, AgentInstanceEvent::ViewBoundary { id } if *id == boundary)
                    && visibility == Visibility::Execution
                    && let Some(progress) = initial_progress.take()
                {
                    publish(&sender, Observation::Event(agent.clone(), AgentInstanceEvent::Progress(progress)))?;
                }
                let discover_children = matches!(event, AgentInstanceEvent::Subagent(_));
                let stopped = matches!(event, AgentInstanceEvent::Stopped);
                if stopped || matches!(event, AgentInstanceEvent::ViewBoundary { .. }) {
                    workflows(&agent, &sessions, &sender, &mut runs, visibility)?;
                }
                if visibility == Visibility::Execution || matches!(event,
                    AgentInstanceEvent::InteractionRequested(_)
                    | AgentInstanceEvent::InteractionClosed { .. }
                    | AgentInstanceEvent::ViewBoundary { .. }
                    | AgentInstanceEvent::PumpFailed { .. }
                    | AgentInstanceEvent::Stopped
                ) {
                    publish(&sender, Observation::Event(agent.clone(), event))?;
                }
                if stopped { return Ok(()); }
                if discover_children { discover(&agent, &sessions, &sender, visibility).await?; }
            }
        }
    }
}

fn workflows(
    agent: &Arc<AgentSession>,
    sessions: &Arc<SessionAssociations>,
    sender: &mpsc::Sender<Observation>,
    previous: &mut WorkflowObservations,
    visibility: Visibility,
) -> Result<(), Fault> {
    let Some(registration) = agent.instance().registration_id() else {
        return Ok(());
    };
    let mut children = Vec::new();
    for view in agent
        .workflow_views(registration)
        .map_err(Fault::operation)?
    {
        let snapshot = &view.snapshot;
        if visibility == Visibility::Execution {
            let payload = json!({
                "kind": "workflow",
                "run_id": snapshot.id(),
                "name": view.name,
                "status": snapshot.status(),
                "phase": snapshot.phase(),
                "active": snapshot.active(),
                "pending": snapshot.pending(),
                "completed": snapshot.completed(),
                "value": snapshot.value(),
                "error": snapshot.error().map(ToString::to_string),
                "cleanup_errors": snapshot.cleanup_errors(),
            });
            if previous.payloads.get(&snapshot.id()) != Some(&payload) {
                publish(
                    sender,
                    Observation::Workflow(agent.clone(), payload.clone()),
                )?;
                previous.payloads.insert(snapshot.id(), payload);
            }
        }
        for worker in snapshot
            .invocations()
            .iter()
            .filter_map(|item| item.worker())
        {
            let Some(registration) = worker.registration() else {
                continue;
            };
            if previous.workers.contains(&registration) {
                continue;
            }
            let Ok(child) = sessions.get(worker.instance_id()) else {
                continue;
            };
            if child.instance().registration_id() == Some(registration)
                && child.creator_id() == Some(agent.instance().id())
                && child.instance().snapshot().status != AgentInstanceStatus::Stopped
            {
                previous.workers.insert(registration);
                children.push((child, None, Visibility::Permissions));
            }
        }
    }
    if !children.is_empty() {
        publish(sender, Observation::Children(children))?;
    }
    Ok(())
}

async fn discover(
    agent: &Arc<AgentSession>,
    sessions: &Arc<SessionAssociations>,
    sender: &mpsc::Sender<Observation>,
    visibility: Visibility,
) -> Result<(), Fault> {
    let snapshots = match agent.instance().subagent_snapshots().await {
        Ok(snapshots) => snapshots,
        Err(phi_sdk::agent_instance::AgentInstanceError::Stopped) => return Ok(()),
        Err(error) => return Err(Fault::new(Code::ObservationLost, error)),
    };
    let mut children = Vec::new();
    for snapshot in snapshots {
        if snapshot.status.admission != phi_sdk::agents::SubagentAdmission::Accepted {
            continue;
        }
        let id = AgentInstanceId::new(snapshot.status.creation.instance_id().as_str());
        let Ok(child) = sessions.get(&id) else {
            if snapshot.status.outcome.is_none() {
                return Err(Fault::new(
                    Code::ObservationLost,
                    "accepted live child has no Session association",
                ));
            }
            continue;
        };
        if child.instance().snapshot().status == AgentInstanceStatus::Stopped {
            continue;
        }
        if snapshot.executor_registration_id.is_none()
            || snapshot.executor_registration_id != child.instance().registration_id()
        {
            if snapshot.status.outcome.is_some() {
                continue;
            }
            return Err(Fault::new(
                Code::ObservationLost,
                "accepted child registration no longer matches",
            ));
        }
        if child.creator_id() != Some(agent.instance().id()) {
            return Err(Fault::new(
                Code::ObservationLost,
                "child lineage does not match its Session association",
            ));
        }
        let turn_id = snapshot.initial_turn_id.ok_or_else(|| {
            Fault::new(
                Code::ObservationLost,
                "accepted child has no initial turn identity",
            )
        })?;
        children.push((child, Some(turn_id), visibility));
    }
    if !children.is_empty() {
        publish(sender, Observation::Children(children))?;
    }
    Ok(())
}
