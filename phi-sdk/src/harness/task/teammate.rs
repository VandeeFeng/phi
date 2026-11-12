use super::list::{TaskList, TaskListError};
use super::model::{Task, TaskId};
use super::operation::{TaskAccessError, TaskAssignment, TaskChanges, TaskListAccess, TaskRequest};
use crate::core::lifecycle::{TeammateIdleEvent, TeammateIdleOutcome};
use crate::harness::manager::AgentAccess;
use crate::harness::manager::AgentKind;
use crate::hooks::HookRegistry;

impl AgentAccess {
    pub(crate) fn teammate_tasks(
        &self,
        hooks: HookRegistry,
    ) -> Result<Option<TeammateTasks>, TaskAccessError> {
        {
            let registry = self.manager.lock_live()?;
            let member = registry.member(&self.caller)?;
            if !matches!(member.snapshot().kind, AgentKind::Teammate { .. }) {
                return Ok(None);
            }
        }
        let access = self.task_list()?;
        access.teammate_context()?;
        Ok(Some(TeammateTasks {
            list: TaskList::new(access.clone(), hooks.clone()),
            access,
            hooks,
        }))
    }
}

/// Caller-bound lifecycle cooperation without exposing the file store.
#[derive(Clone)]
pub(crate) struct TeammateTasks {
    access: TaskListAccess,
    list: TaskList,
    hooks: HookRegistry,
}

pub(crate) struct IdleTask {
    task: Task,
    caller: crate::harness::instance::AgentInstanceId,
    context: crate::core::lifecycle::TaskLifecycleContext,
}

impl IdleTask {
    pub(crate) fn request(&self) -> TaskRequest {
        TaskRequest::Update {
            task_id: self.task.id.clone(),
            changes: TaskChanges {
                owner: TaskAssignment::Assign(self.caller.clone()),
                ..Default::default()
            },
        }
    }
}

impl TeammateTasks {
    pub(crate) fn validate(&self) -> Result<(), TaskAccessError> {
        self.access.teammate_context()?;
        Ok(())
    }

    pub(crate) async fn idle_candidate(&self) -> Result<Option<IdleTask>, TaskListError> {
        let Some(task) = self.list.idle_candidate().await? else {
            return Ok(None);
        };
        Ok(Some(IdleTask {
            task,
            caller: self.access.caller_id().clone(),
            context: self.access.teammate_context()?,
        }))
    }

    pub(crate) async fn take_idle_work(
        &self,
        candidate: IdleTask,
    ) -> Result<Option<TaskId>, TaskListError> {
        if self.access.teammate_context()? != candidate.context {
            return Err(TaskListError::Conflict);
        }
        // Dropping the actor worker also revokes file work waiting on the blocking pool.
        let (cancellation, active) = tokio::sync::oneshot::channel();
        let selected = self
            .list
            .take_idle_candidate(candidate.task, cancellation)
            .await?;
        drop(active);
        Ok(selected.map(|task| task.id))
    }

    pub(crate) async fn check_boundary(&self) -> anyhow::Result<TeammateIdleOutcome> {
        match self.list.check_end_turn().await {
            Ok(()) => {}
            Err(TaskListError::Rejected(feedback)) => {
                return Ok(TeammateIdleOutcome::Continue { feedback });
            }
            Err(error) => return Err(error.into()),
        }
        let context = self.access.teammate_context()?;
        let outcome = self
            .hooks
            .teammate_idle(TeammateIdleEvent {
                context: context.clone(),
            })
            .await;
        if self.access.teammate_context()? != context {
            anyhow::bail!("teammate context changed during the idle gate");
        }
        Ok(outcome?)
    }
}
