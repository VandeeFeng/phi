use crate::core::event::PhiFailure;
use crate::harness::manager::preparation::{AgentCreation, SubagentCompletion};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentAdmission {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SubagentStatus {
    pub creation: AgentCreation,
    pub parent_tool_call_id: Option<String>,
    pub admission: SubagentAdmission,
    pub outcome: Option<Result<String, PhiFailure>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentSnapshot {
    pub status: SubagentStatus,
    pub initial_turn_id: Option<String>,
    #[serde(skip)]
    pub executor_registration_id: Option<crate::harness::manager::AgentRegistrationId>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubagentActivity {
    Started {
        instance_id: String,
        name: String,
        prompt: String,
    },
    Finished {
        instance_id: String,
        name: String,
        succeeded: bool,
    },
}

pub(crate) struct ChildResult {
    pub(crate) creation: AgentCreation,
    pub(crate) outcome: Result<String, PhiFailure>,
}

pub(crate) type InitialCompletion = std::sync::Arc<CompletionState>;

#[derive(Default)]
pub(crate) struct CompletionState {
    admission_accepted: OnceLock<()>,
    executor_epoch: OnceLock<uuid::Uuid>,
    completion: OnceLock<SubagentCompletion>,
    completion_updates: OnceLock<tokio::sync::watch::Sender<bool>>,
    initial_turn_id: OnceLock<String>,
    cancellation: OnceLock<tokio::sync::watch::Sender<bool>>,
    cancel_reason: OnceLock<crate::CancellationReason>,
    admission_finished: OnceLock<tokio::sync::watch::Sender<bool>>,
    admission_failure: OnceLock<String>,
}

impl CompletionState {
    pub(crate) fn bind_turn(&self, turn_id: String) {
        let _ = self.initial_turn_id.set(turn_id);
    }

    pub(crate) fn initial_turn_id(&self) -> Option<String> {
        self.initial_turn_id.get().cloned()
    }

    pub(crate) fn bind_executor(&self, epoch: uuid::Uuid) {
        let _ = self.executor_epoch.set(epoch);
    }

    pub(crate) fn executor_epoch(&self) -> Option<uuid::Uuid> {
        self.executor_epoch.get().copied()
    }

    pub(crate) fn executor_registration_id(
        &self,
    ) -> Option<crate::harness::manager::AgentRegistrationId> {
        self.executor_epoch()
            .map(crate::harness::manager::AgentRegistrationId::new)
    }

    pub(crate) fn accept(&self) {
        let _ = self.admission_accepted.set(());
    }

    pub(crate) fn admission(&self) -> crate::harness::instance::child::SubagentAdmission {
        if self.admission_accepted.get().is_some() {
            crate::harness::instance::child::SubagentAdmission::Accepted
        } else if self.is_admission_finished() {
            crate::harness::instance::child::SubagentAdmission::Rejected
        } else {
            crate::harness::instance::child::SubagentAdmission::Pending
        }
    }

    pub(crate) fn cancel_pending(
        &self,
        cancellation: &tokio::sync::watch::Sender<bool>,
    ) -> crate::harness::instance::child::SubagentAdmission {
        let mut admission = self.admission();
        cancellation.send_if_modified(|cancelled| {
            admission = self.admission();
            if admission == crate::harness::instance::child::SubagentAdmission::Pending
                && !*cancelled
            {
                let _ = self
                    .cancel_reason
                    .set(crate::CancellationReason::Interrupted);
                *cancelled = true;
                return true;
            }
            false
        });
        admission
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.completion.get().is_some()
    }

    pub(crate) fn get(&self) -> Option<SubagentCompletion> {
        self.completion.get().cloned()
    }

    pub(crate) async fn wait_complete(&self) {
        let mut completed = self
            .completion_updates
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .subscribe();
        completed
            .wait_for(|completed| *completed)
            .await
            .expect("completion retains its notification sender");
    }

    pub(crate) fn cancellation(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancellation
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .subscribe()
    }

    pub(crate) fn mark_admission_finished(&self, failure: Option<String>) {
        if let Some(failure) = failure {
            let _ = self.admission_failure.set(failure);
        }
        self.admission_finished
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .send_replace(true);
    }

    pub(crate) fn is_admission_finished(&self) -> bool {
        self.admission_finished
            .get()
            .is_some_and(|sender| *sender.borrow())
    }

    pub(crate) async fn wait_admission_finished(&self) -> Option<String> {
        let mut finished = self
            .admission_finished
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .subscribe();
        crate::harness::manager::preparation::wait_for_cancellation(&mut finished).await;
        self.admission_failure.get().cloned()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation
            .get()
            .is_some_and(|sender| *sender.borrow())
    }

    pub(crate) fn cancellation_reason(&self) -> crate::CancellationReason {
        self.cancel_reason
            .get()
            .copied()
            .unwrap_or(crate::CancellationReason::Interrupted)
    }

    pub(crate) fn cancel(&self) -> bool {
        self.cancel_with_reason(crate::CancellationReason::Interrupted)
    }

    pub(crate) fn cancel_with_reason(&self, reason: crate::CancellationReason) -> bool {
        self.cancellation
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .send_if_modified(|cancelled| {
                if *cancelled || (self.admission_accepted.get().is_some() && self.is_complete()) {
                    return false;
                }
                let _ = self.cancel_reason.set(reason);
                *cancelled = true;
                true
            })
    }

    pub(crate) fn publish(
        &self,
        outcome: Result<String, crate::PhiFailure>,
    ) -> Result<(), Result<String, crate::PhiFailure>> {
        let sender = self
            .cancellation
            .get_or_init(|| tokio::sync::watch::channel(false).0);
        let cancellation = sender.borrow();
        if *cancellation && outcome.is_ok() {
            return Err(outcome);
        }
        self.completion
            .set(SubagentCompletion { outcome })
            .map_err(|completion| completion.outcome)?;
        drop(cancellation);
        self.completion_updates
            .get_or_init(|| tokio::sync::watch::channel(false).0)
            .send_replace(true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initial_completion_is_immutable_and_not_consumed_by_waiters() {
        let completion = CompletionState::default();
        completion.accept();
        let barrier = std::sync::Barrier::new(2);
        let published = std::thread::scope(|scope| {
            let workers = ["first", "second"].map(|value| {
                let completion = &completion;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    completion.publish(Ok(value.into())).is_ok()
                })
            });
            workers.map(|worker| worker.join().unwrap())
        });
        assert_eq!(
            published.into_iter().filter(|published| *published).count(),
            1
        );
        let original = completion.get().unwrap();
        tokio::join!(completion.wait_complete(), completion.wait_complete());
        assert_eq!(completion.get(), Some(original));
        assert!(!completion.cancel());
        assert!(completion.publish(Ok("replacement".into())).is_err());

        let cancelled = CompletionState::default();
        assert!(cancelled.cancel());
        assert!(cancelled.publish(Ok("late success".into())).is_err());
        cancelled
            .publish(Err(crate::PhiFailure::new(
                crate::PhiFailureKind::Cancelled,
                "cancelled",
                None,
            )))
            .unwrap();
        cancelled.wait_complete().await;
        assert_eq!(
            cancelled.get().unwrap().outcome.unwrap_err().kind,
            crate::PhiFailureKind::Cancelled
        );
    }

    #[test]
    fn first_cancellation_source_survives_later_requests() {
        let reasons = [
            crate::CancellationReason::UserRequested,
            crate::CancellationReason::Interrupted,
            crate::CancellationReason::AgentStopped,
        ];
        for first in reasons {
            for later in reasons {
                let completion = CompletionState::default();
                assert!(completion.cancel_with_reason(first));
                assert!(!completion.cancel_with_reason(later));
                assert_eq!(completion.cancellation_reason(), first);
            }
        }
        let pending = CompletionState::default();
        let cancellation = tokio::sync::watch::channel(false).0;
        pending.cancel_pending(&cancellation);
        pending.cancel_with_reason(crate::CancellationReason::UserRequested);
        assert_eq!(
            pending.cancellation_reason(),
            crate::CancellationReason::Interrupted
        );
    }

    #[test]
    fn pending_cancellation_cannot_revoke_accepted_admission() {
        const RACE_ROUNDS: usize = 32;
        for _ in 0..RACE_ROUNDS {
            let completion = CompletionState::default();
            let cancellation = tokio::sync::watch::channel(false).0;
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    let cancelled = cancellation.borrow();
                    if !*cancelled {
                        completion.accept();
                    }
                });
                scope.spawn(|| {
                    barrier.wait();
                    completion.cancel_pending(&cancellation);
                });
            });
            let accepted = completion.admission()
                == crate::harness::instance::child::SubagentAdmission::Accepted;
            assert_eq!(*cancellation.borrow(), !accepted);
            completion.mark_admission_finished(None);
            assert_eq!(
                completion.admission(),
                if accepted {
                    crate::harness::instance::child::SubagentAdmission::Accepted
                } else {
                    crate::harness::instance::child::SubagentAdmission::Rejected
                }
            );
        }
    }
}
