//! Session checkpoint policy and typed save observations.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};

use phi_sdk::PhiEvent;
use phi_sdk::agent_instance::{AgentInstanceEvent, AgentInstanceId};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::associations::SessionAssociations;
use super::session::AgentSession;

pub(super) type AgentSessions = RwLock<HashMap<AgentInstanceId, Arc<AgentSession>>>;

pub(super) struct SessionCheckpoints;

impl SessionCheckpoints {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self)
    }

    pub(super) async fn observe(
        self: &Arc<Self>,
        agent_session: &Arc<AgentSession>,
        associations: Weak<SessionAssociations>,
    ) {
        let events = agent_session.instance().subscribe();
        let task = tokio::spawn(persist_turns(
            events,
            Arc::downgrade(agent_session),
            Arc::downgrade(self),
            agent_session.checkpoint_wakeup(),
            agent_session
                .instance()
                .subscribe_finalization()
                .expect("Session association requires SDK registration"),
            associations,
        ));
        agent_session.install_checkpoint_observer(task).await;
    }

    pub(super) async fn checkpoint(&self, agent_session: &AgentSession) -> anyhow::Result<()> {
        let _checkpoint = agent_session.checkpoint_guard().await;
        self.save_with_status(agent_session, false).await
    }

    async fn automatic_checkpoint(&self, agent_session: &AgentSession) -> anyhow::Result<()> {
        let _checkpoint = agent_session.checkpoint_guard().await;
        self.save_with_status(agent_session, true).await
    }

    async fn save_with_status(
        &self,
        agent_session: &AgentSession,
        retry: bool,
    ) -> anyhow::Result<()> {
        let attempt = agent_session.begin_save(SaveOperation::Checkpoint).await;
        let mut result = self.save_history(agent_session).await;
        let mut attempts = 1;
        if retry && result.is_err() {
            tokio::task::yield_now().await;
            attempts += 1;
            result = self.save_history(agent_session).await;
        }
        agent_session
            .complete_save(attempt, SaveOperation::Checkpoint, attempts, &result)
            .await;
        result.map(|_| ())
    }

    async fn save_history(
        &self,
        agent_session: &AgentSession,
    ) -> anyhow::Result<Option<SavedBoundary>> {
        Ok(agent_session
            .checkpoint_history()
            .await?
            .as_ref()
            .map(SavedBoundary::from_session))
    }
}

async fn persist_turns(
    mut events: tokio::sync::broadcast::Receiver<AgentInstanceEvent>,
    agent_session: Weak<AgentSession>,
    checkpoints: Weak<SessionCheckpoints>,
    checkpoint_requested: Arc<Notify>,
    mut finalization: tokio::sync::watch::Receiver<phi_sdk::agent_manager::AgentFinalizationStatus>,
    associations: Weak<SessionAssociations>,
) {
    checkpoint_requested.notify_one();
    loop {
        let checkpoint = tokio::select! {
            changed = finalization.changed() => {
                if changed.is_err() { break; }
                finalization.borrow_and_update();
                true
            },
            _ = checkpoint_requested.notified() => true,
            event = events.recv() => {
                match event {
                    Ok(AgentInstanceEvent::Phi(envelope)) => matches!(
                        envelope.event,
                        PhiEvent::TurnFinished { .. } | PhiEvent::TurnFailed { .. }
                    ),
                    Ok(AgentInstanceEvent::HistoryChanged | AgentInstanceEvent::RunCancelled { .. } | AgentInstanceEvent::Subagent(_))
                    | Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
                    | Ok(AgentInstanceEvent::Stopped) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => false,
                }
            },
        };
        if checkpoint {
            let (Some(agent_session), Some(checkpoints)) =
                (agent_session.upgrade(), checkpoints.upgrade())
            else {
                break;
            };
            if agent_session.instance().finalization().is_some() {
                let Some(associations) = associations.upgrade() else {
                    break;
                };
                if associations
                    .finalize_agent_session(&agent_session)
                    .await
                    .is_ok()
                {
                    break;
                }
            } else {
                let _ = checkpoints.automatic_checkpoint(&agent_session).await;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SaveOperation {
    Checkpoint,
    Finalization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedBoundary {
    pub(crate) leaf_id: Option<String>,
    pub(crate) entry_count: usize,
}

impl SavedBoundary {
    pub(super) fn from_session(session: &crate::session::Session) -> Self {
        Self {
            leaf_id: session
                .transcript
                .entries
                .last()
                .map(|entry| entry.id.to_string()),
            entry_count: session.transcript.entries.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveCompletion {
    pub(crate) attempt: u64,
    pub(crate) operation: SaveOperation,
    pub(crate) attempts: u8,
    pub(crate) saved_boundary: Option<SavedBoundary>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveObservation {
    pub(crate) association_id: String,
    pub(crate) persistent: bool,
    pub(crate) attempt: u64,
    pub(crate) pending: Option<SaveOperation>,
    pub(crate) completed: Option<SaveCompletion>,
    pub(crate) finalization: Option<SaveCompletion>,
}

impl SaveObservation {
    pub(super) fn new(persistent: bool) -> Self {
        Self {
            association_id: uuid::Uuid::new_v4().to_string(),
            persistent,
            attempt: 0,
            pending: None,
            completed: None,
            finalization: None,
        }
    }

    pub(super) fn begin(&mut self, operation: SaveOperation) -> u64 {
        self.attempt += 1;
        self.pending = Some(operation);
        self.attempt
    }

    pub(super) fn complete(&mut self, completion: SaveCompletion) -> bool {
        if completion.attempt != self.attempt || self.pending != Some(completion.operation) {
            return false;
        }
        self.pending = None;
        match completion.operation {
            SaveOperation::Checkpoint => self.completed = Some(completion),
            SaveOperation::Finalization => self.finalization = Some(completion),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_retains_failure_and_stale_completion_cannot_clear_it() {
        let mut observation = SaveObservation::new(true);
        let first = observation.begin(SaveOperation::Checkpoint);
        let failed = SaveCompletion {
            attempt: first,
            operation: SaveOperation::Checkpoint,
            attempts: 2,
            saved_boundary: None,
            error: Some("save failed".into()),
        };
        observation.complete(failed.clone());
        let retry = observation.begin(SaveOperation::Checkpoint);
        observation.complete(SaveCompletion {
            error: None,
            ..failed.clone()
        });
        assert_eq!(observation.completed, Some(failed));
        assert_eq!(observation.pending, Some(SaveOperation::Checkpoint));
        observation.complete(SaveCompletion {
            attempt: retry,
            operation: SaveOperation::Checkpoint,
            attempts: 1,
            saved_boundary: Some(SavedBoundary {
                leaf_id: None,
                entry_count: 0,
            }),
            error: None,
        });
        assert!(observation.completed.as_ref().unwrap().error.is_none());
        assert!(observation.pending.is_none());
        assert_ne!(
            observation.association_id,
            SaveObservation::new(true).association_id
        );
    }
}
