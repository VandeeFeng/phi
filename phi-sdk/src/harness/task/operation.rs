use super::list::{OwnerUpdate, TaskCreate, TaskList, TaskListError, TaskPatch};
use super::model::{Task, TaskId, TaskListBinding, TaskOwner, TaskStatus};
use crate::harness::instance::AgentInstanceId;
use crate::harness::manager::registry::Registry;
use crate::harness::manager::{
    AgentAccess, AgentFinalizationStatus, AgentManagerError, AgentOperation,
};
use crate::hooks::HookRegistry;
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskAccessError {
    #[error(transparent)]
    Manager(#[from] AgentManagerError),
    #[error("TaskList access requires an active caller registration")]
    InactiveCaller,
    #[error("caller has no TaskList binding")]
    NotBound,
    #[error("Task lifecycle cooperation requires a live teammate")]
    NotTeammate,
    #[error("TaskList binding does not match the captured scope")]
    BindingChanged,
    #[error("{0}")]
    InvalidToolScope(String),
}

/// Live caller capability, not a store or authority derived from a saved binding.
#[derive(Clone)]
pub(crate) struct TaskListAccess {
    caller: AgentAccess,
    binding: TaskListBinding,
    invocation: Option<std::sync::Arc<tokio::sync::oneshot::Sender<()>>>,
    owner_registration: Option<AgentAccess>,
}

impl AgentAccess {
    pub(crate) fn task_list(&self) -> Result<TaskListAccess, TaskAccessError> {
        let registry = self.manager.lock_live()?;
        let binding = live_binding(self, &registry)?.clone();
        Ok(TaskListAccess {
            caller: self.clone(),
            binding,
            invocation: None,
            owner_registration: None,
        })
    }
}

impl TaskListAccess {
    pub(crate) fn with_invocation(
        mut self,
        lifetime: std::sync::Arc<tokio::sync::oneshot::Sender<()>>,
    ) -> Self {
        self.invocation = Some(lifetime);
        self
    }

    pub(crate) async fn observe(self) -> Result<TaskListView, TaskListError> {
        let (lifetime, _receiver) = tokio::sync::oneshot::channel();
        TaskList::new(
            self.with_invocation(std::sync::Arc::new(lifetime)),
            HookRegistry::new(),
        )
        .observe()
        .await
    }

    pub(super) fn resolve_owner(
        &mut self,
        selector: &crate::harness::instance::AgentInstanceId,
    ) -> Result<super::model::TaskOwner, TaskAccessError> {
        let registry = self.caller.manager.lock_live()?;
        self.validate_in(&registry)?;
        let caller = registry.member(&self.caller.caller)?;
        let target = registry.resolve_target(caller, selector)?;
        let owner = AgentAccess::new(
            self.caller.manager.clone(),
            target.instance.id().clone(),
            target.epoch,
        );
        if live_binding(&owner, &registry)? != &self.binding {
            return Err(TaskAccessError::BindingChanged);
        }
        let reference = super::model::TaskOwner {
            instance_id: owner.caller.clone(),
        };
        self.owner_registration = Some(owner);
        Ok(reference)
    }

    fn validate_in(&self, registry: &Registry) -> Result<(), TaskAccessError> {
        if self
            .invocation
            .as_ref()
            .is_some_and(|scope| scope.is_closed())
        {
            return Err(TaskAccessError::InactiveCaller);
        }
        if live_binding(&self.caller, registry)? != &self.binding
            || self.owner_registration.as_ref().is_some_and(|owner| {
                !live_binding(owner, registry).is_ok_and(|binding| binding == &self.binding)
            })
        {
            return Err(TaskAccessError::BindingChanged);
        }
        Ok(())
    }

    pub(super) fn teammate_context(
        &self,
    ) -> Result<crate::core::lifecycle::TaskLifecycleContext, TaskAccessError> {
        let registry = self.caller.manager.lock_live()?;
        let context = self.context_in(&registry)?;
        let member = registry.member(&self.caller.caller)?;
        if !matches!(
            member.snapshot().kind,
            crate::harness::manager::AgentKind::Teammate { .. }
        ) {
            return Err(TaskAccessError::NotTeammate);
        }
        Ok(context)
    }

    pub(super) fn context(
        &self,
    ) -> Result<crate::core::lifecycle::TaskLifecycleContext, TaskAccessError> {
        let registry = self.caller.manager.lock_live()?;
        self.context_in(&registry)
    }

    fn context_in(
        &self,
        registry: &Registry,
    ) -> Result<crate::core::lifecycle::TaskLifecycleContext, TaskAccessError> {
        self.validate_in(registry)?;
        let member = registry.member(&self.caller.caller)?;
        Ok(crate::core::lifecycle::TaskLifecycleContext {
            instance_id: self.caller.caller.clone(),
            list_id: self.binding.list_id().to_owned(),
            team: member.team.clone(),
            alias: member.alias.clone(),
        })
    }

    pub(super) fn caller_id(&self) -> &crate::harness::instance::AgentInstanceId {
        &self.caller.caller
    }

    /// Recheck before file effects and after unlocked lifecycle gates.
    pub(crate) fn validate(&self) -> Result<&TaskListBinding, TaskAccessError> {
        let registry = self.caller.manager.lock_live()?;
        self.validate_in(&registry)?;
        Ok(&self.binding)
    }
}

/// Cleanup-only scope; never usable for ordinary Task operations.
#[derive(Clone)]
pub(crate) struct TaskExitAccess {
    owner: std::sync::Weak<std::sync::Mutex<Registry>>,
    instance: crate::harness::instance::AgentInstanceId,
    epoch: uuid::Uuid,
    binding: TaskListBinding,
    kind: crate::harness::manager::AgentKind,
}

impl TaskExitAccess {
    pub(crate) fn capture(
        manager: &crate::harness::manager::AgentManager,
        registry: &Registry,
        instance: &crate::harness::instance::AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<Self, TaskAccessError> {
        let member = registry.member(instance)?;
        let kind = member.snapshot().kind;
        if !member.closing || member.epoch != epoch {
            return Err(TaskAccessError::InactiveCaller);
        }
        let crate::harness::manager::AgentKind::Teammate { team, .. } = &kind else {
            return Err(TaskAccessError::NotTeammate);
        };
        if member.team.as_ref() != Some(team) {
            return Err(TaskAccessError::NotTeammate);
        }
        Ok(Self {
            owner: std::sync::Arc::downgrade(&manager.registry),
            instance: instance.clone(),
            epoch,
            binding: member
                .instance
                .task_list()
                .ok_or(TaskAccessError::NotBound)?
                .clone(),
            kind,
        })
    }

    pub(super) fn caller_id(&self) -> &crate::harness::instance::AgentInstanceId {
        &self.instance
    }

    pub(super) fn validate(&self) -> Result<&TaskListBinding, TaskAccessError> {
        let manager = crate::harness::manager::AgentManager::from_scope(&self.owner)?;
        let registry = manager.lock()?;
        let member = registry.member(&self.instance)?;
        if !member.closing || member.epoch != self.epoch || member.snapshot().kind != self.kind {
            return Err(TaskAccessError::InactiveCaller);
        }
        let crate::harness::manager::AgentKind::Teammate { team, .. } = &self.kind else {
            return Err(TaskAccessError::NotTeammate);
        };
        if member.team.as_ref() != Some(team) || member.instance.task_list() != Some(&self.binding)
        {
            return Err(TaskAccessError::BindingChanged);
        }
        Ok(&self.binding)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Task exit release failed after releasing {released:?}: {source}")]
pub(crate) struct TaskExitError {
    pub(crate) released: Vec<String>,
    #[source]
    pub(super) source: super::list::TaskListError,
}

fn live_binding<'a>(
    access: &AgentAccess,
    registry: &'a Registry,
) -> Result<&'a TaskListBinding, TaskAccessError> {
    let caller = registry.member(&access.caller)?;
    if registry.shutting_down
        || caller.closing
        || caller.epoch != access.epoch
        || !access.allows(registry, caller, caller, AgentOperation::Observe)
    {
        return Err(TaskAccessError::InactiveCaller);
    }
    let finalization = caller
        .instance
        .subscribe_finalization()
        .ok_or(TaskAccessError::InactiveCaller)?;
    if !matches!(*finalization.borrow(), AgentFinalizationStatus::Open) {
        return Err(TaskAccessError::InactiveCaller);
    }
    caller.instance.task_list().ok_or(TaskAccessError::NotBound)
}

#[derive(Debug, Default)]
pub(crate) enum TaskAssignment {
    #[default]
    Unchanged,
    Assign(AgentInstanceId),
    Release,
}

#[derive(Debug, Default)]
pub(crate) struct TaskChanges {
    pub(crate) subject: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) active_form: Option<String>,
    pub(crate) status: Option<TaskStatus>,
    pub(crate) owner: TaskAssignment,
    pub(crate) add_blocks: Option<Vec<TaskId>>,
    pub(crate) add_blocked_by: Option<Vec<TaskId>>,
    pub(crate) metadata: Option<Map<String, Value>>,
}

#[derive(Debug)]
pub(crate) enum TaskRequest {
    Create {
        subject: String,
        description: String,
        active_form: Option<String>,
        metadata: Map<String, Value>,
    },
    Get(TaskId),
    List {
        after: Option<TaskId>,
        limit: usize,
    },
    Update {
        task_id: TaskId,
        changes: TaskChanges,
    },
    Delete(TaskId),
}

pub(crate) struct PreparedTaskRequest {
    access: TaskListAccess,
    request: TaskRequest,
}

#[derive(Debug)]
pub(crate) struct TaskDetails {
    pub(crate) task: Task,
    pub(crate) blocks: Vec<TaskId>,
    pub(crate) unresolved: Vec<TaskId>,
    pub(crate) eligible: bool,
}

#[derive(Debug)]
pub(crate) enum TaskResult {
    Created {
        id: TaskId,
        subject: String,
    },
    Found(Option<TaskDetails>),
    Listed {
        tasks: Vec<TaskDetails>,
        next: Option<TaskId>,
    },
    Updated(TaskDetails),
    Deleted(TaskId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatusView {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItemView {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub status: TaskStatusView,
    pub owner: Option<AgentInstanceId>,
    pub blocked_by: Vec<String>,
    pub blocks: Vec<String>,
    pub unresolved: Vec<String>,
    pub eligible: bool,
}

/// A complete bounded list read under one lock, independent of conversation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskListView {
    pub list_id: String,
    pub tasks: Vec<TaskItemView>,
}

impl TaskRequest {
    pub(crate) fn prepare(
        mut self,
        mut access: TaskListAccess,
    ) -> Result<PreparedTaskRequest, TaskAccessError> {
        access.validate()?;
        if let Self::Update { changes, .. } = &mut self
            && let TaskAssignment::Assign(selector) = &mut changes.owner
        {
            *selector = access.resolve_owner(selector)?.instance_id;
        }
        Ok(PreparedTaskRequest {
            access,
            request: self,
        })
    }
}

impl PreparedTaskRequest {
    pub(crate) fn request(&self) -> &TaskRequest {
        &self.request
    }

    pub(crate) async fn execute(self, hooks: HookRegistry) -> Result<TaskResult, TaskListError> {
        self.access.validate()?;
        let list = TaskList::new(self.access, hooks);
        Ok(match self.request {
            TaskRequest::Create {
                subject,
                description,
                active_form,
                metadata,
            } => {
                let task = list
                    .create(TaskCreate {
                        subject,
                        description,
                        active_form,
                        metadata,
                    })
                    .await?;
                TaskResult::Created {
                    id: task.id,
                    subject: task.subject,
                }
            }
            TaskRequest::Get(id) => TaskResult::Found(list.get(id).await?),
            TaskRequest::List { after, limit } => {
                let page = list.list(after, limit).await?;
                TaskResult::Listed {
                    tasks: page.tasks,
                    next: page.next,
                }
            }
            TaskRequest::Update { task_id, changes } => {
                let patch = TaskPatch {
                    subject: changes.subject,
                    description: changes.description,
                    active_form: changes.active_form,
                    status: changes.status,
                    owner: match changes.owner {
                        TaskAssignment::Unchanged => OwnerUpdate::Unchanged,
                        TaskAssignment::Assign(instance_id) => {
                            OwnerUpdate::Assign(TaskOwner { instance_id })
                        }
                        TaskAssignment::Release => OwnerUpdate::Release,
                    },
                    add_blocks: changes.add_blocks.unwrap_or_default(),
                    add_blocked_by: changes.add_blocked_by.unwrap_or_default(),
                    metadata: changes.metadata.unwrap_or_default(),
                };
                TaskResult::Updated(list.update(task_id, patch).await?)
            }
            TaskRequest::Delete(id) => {
                list.delete(id.clone()).await?;
                TaskResult::Deleted(id)
            }
        })
    }
}

impl From<TaskDetails> for TaskItemView {
    fn from(view: TaskDetails) -> Self {
        Self {
            id: view.task.id.into(),
            subject: view.task.subject,
            description: view.task.description,
            active_form: view.task.active_form,
            status: match view.task.status {
                TaskStatus::Pending => TaskStatusView::Pending,
                TaskStatus::InProgress => TaskStatusView::InProgress,
                TaskStatus::Completed => TaskStatusView::Completed,
            },
            owner: view.task.owner.map(|owner| owner.instance_id),
            blocked_by: view.task.blocked_by.into_iter().map(Into::into).collect(),
            blocks: view.blocks.into_iter().map(Into::into).collect(),
            unresolved: view.unresolved.into_iter().map(Into::into).collect(),
            eligible: view.eligible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lifecycle::TaskGateOutcome;
    use crate::factory::{Phi, PhiRuntimeServices};
    use crate::harness::instance::AgentInstance;
    use crate::harness::manager::AgentManager;
    use crate::hooks::Hook;
    use crate::models::{ModelCatalog, ModelDescriptor};
    use crate::tasks::TaskListBinding;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct Fixture {
        root: PathBuf,
        phi: Phi,
        manager: AgentManager,
        lead: AgentInstance,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        async fn new() -> Self {
            let root =
                PathBuf::from("/tmp").join(format!("phi-task-operation-{}", uuid::Uuid::new_v4()));
            let catalog = ModelCatalog::for_test_model(ModelDescriptor {
                provider: "openai-codex".into(),
                id: "test".into(),
                name: "Test".into(),
                api: crate::providers::ProviderApi::OpenAiCodexResponses,
                reasoning: false,
                thinking_level_map: None,
                input: vec![crate::models::ModelInput::Text],
                context_window: 128_000,
                max_output_tokens: 16_384,
                cost: Default::default(),
            })
            .unwrap();
            let phi = Phi::builder()
                .provider("openai-codex")
                .model("test")
                .no_tools(true)
                .agent(crate::agent::custom::AgentDefinition::new(
                    "reviewer",
                    "Review",
                    "Inspect work",
                ))
                .build()
                .unwrap()
                .with_runtime_services(PhiRuntimeServices::new().model_catalog(Arc::new(catalog)));
            let manager = AgentManager::new();
            let lead = manager
                .insert(
                    phi.agent_instance_builder()
                        .task_list(TaskListBinding::new(root.join("tasks"), "shared").unwrap())
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            Self {
                root,
                phi,
                manager,
                lead,
            }
        }

        fn access(&self, instance: &AgentInstance) -> TaskListAccess {
            self.manager
                .for_agent(instance.id())
                .unwrap()
                .task_list()
                .unwrap()
        }

        async fn call(&self, request: TaskRequest) -> TaskResult {
            request
                .prepare(self.access(&self.lead))
                .unwrap()
                .execute(HookRegistry::new())
                .await
                .unwrap()
        }

        async fn details(&self, id: &str) -> TaskDetails {
            let TaskResult::Found(Some(task)) = self.call(TaskRequest::Get(task_id(id))).await
            else {
                panic!("expected task");
            };
            task
        }
    }

    fn task_id(id: &str) -> TaskId {
        id.to_owned().try_into().unwrap()
    }

    fn create(subject: &str) -> TaskRequest {
        TaskRequest::Create {
            subject: subject.into(),
            description: "Inspect the work".into(),
            active_form: Some("Reviewing work".into()),
            metadata: Map::new(),
        }
    }

    fn update(id: &str, changes: TaskChanges) -> TaskRequest {
        TaskRequest::Update {
            task_id: task_id(id),
            changes,
        }
    }

    #[tokio::test]
    async fn task_subscription_tracks_commits_and_external_edits() {
        use futures::FutureExt;

        let fixture = Fixture::new().await;
        let mut changes = fixture.lead.subscribe_tasks().await.unwrap().unwrap();
        let before = fixture.lead.snapshot().revision;
        assert!(
            fixture
                .lead
                .observe_tasks()
                .await
                .unwrap()
                .unwrap()
                .tasks
                .is_empty()
        );
        fixture.call(create("committed task")).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        let view = fixture.lead.observe_tasks().await.unwrap().unwrap();
        assert_eq!(view.tasks[0].subject, "committed task");
        while changes.changed().now_or_never().is_some() {}
        let path = fixture.root.join("tasks/shared/1.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["subject"] = json!("external edit");
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture.lead.observe_tasks().await.unwrap().unwrap().tasks[0].subject,
            "external edit"
        );
        let directory = fixture.root.join("tasks/shared");
        std::fs::rename(&directory, fixture.root.join("tasks/moved")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while changes.changed().await.is_ok() {}
        })
        .await
        .expect("removed Task directory must invalidate its watcher");
        let mut recovered = fixture.lead.subscribe_tasks().await.unwrap().unwrap();
        assert!(
            fixture
                .lead
                .observe_tasks()
                .await
                .unwrap()
                .unwrap()
                .tasks
                .is_empty()
        );
        fixture.call(create("new directory")).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), recovered.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture.lead.observe_tasks().await.unwrap().unwrap().tasks[0].subject,
            "new directory"
        );
        assert_eq!(fixture.lead.snapshot().revision, before);
        assert!(fixture.lead.transcript().await.unwrap().entries.is_empty());
        drop(recovered);
        drop(changes);
        fixture.manager.stop(fixture.lead.id()).await.unwrap();
        assert!(fixture.lead.subscribe_tasks().await.is_err());
    }

    #[tokio::test]
    async fn observation_reads_complete_canonical_work_without_execution() {
        use crate::tasks::TaskStatusView;
        let fixture = Fixture::new().await;
        for index in 1..=105 {
            fixture.call(create(&format!("Work {index}"))).await;
        }
        fixture
            .call(update(
                "2",
                TaskChanges {
                    add_blocked_by: Some(vec![task_id("1")]),
                    ..Default::default()
                },
            ))
            .await;
        fixture
            .call(update(
                "1",
                TaskChanges {
                    owner: TaskAssignment::Assign(fixture.lead.id().clone()),
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            ))
            .await;
        let before = fixture.lead.snapshot();
        let observed = fixture.lead.observe_tasks().await.unwrap().unwrap();
        assert_eq!(observed.list_id, "shared");
        assert_eq!(observed.tasks.len(), 105);
        assert_eq!(observed.tasks.last().unwrap().id, "105");
        assert_eq!(observed.tasks[0].status, TaskStatusView::InProgress);
        assert_eq!(observed.tasks[0].owner.as_ref(), Some(fixture.lead.id()));
        assert_eq!(observed.tasks[0].blocks, ["2"]);
        assert_eq!(observed.tasks[1].unresolved, ["1"]);
        assert!(!observed.tasks[1].eligible);
        let path = fixture.root.join("tasks/shared/1.json");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["status"] = json!("completed");
        let bytes = serde_json::to_vec(&record).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let refreshed = fixture.lead.observe_tasks().await.unwrap().unwrap();
        assert_eq!(refreshed.tasks[0].status, TaskStatusView::Completed);
        assert!(refreshed.tasks[1].eligible);
        assert!(refreshed.tasks[1].unresolved.is_empty());
        assert_eq!(refreshed.tasks[1].blocked_by, ["1"]);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(fixture.lead.snapshot().revision, before.revision);
        assert!(fixture.lead.transcript().await.unwrap().entries.is_empty());
        assert_eq!(fixture.manager.list().unwrap().len(), 1);
        std::fs::write(&path, b"invalid").unwrap();
        assert!(fixture.lead.observe_tasks().await.is_err());
        std::fs::write(&path, &bytes).unwrap();
        fixture.manager.stop(fixture.lead.id()).await.unwrap();
        let mut transcript = crate::transcript::Transcript::new();
        transcript.id = fixture.lead.id().as_str().into();
        let successor = fixture
            .manager
            .insert(
                fixture
                    .phi
                    .agent_instance_builder()
                    .transcript(transcript)
                    .task_list(fixture.lead.task_list().unwrap().clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert!(fixture.lead.observe_tasks().await.is_err());
        assert_eq!(successor.observe_tasks().await.unwrap().unwrap(), refreshed);
        fixture.manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn owner_resolution_requires_shared_list_not_control_grants() {
        let fixture = Fixture::new().await;
        fixture
            .manager
            .initialize_team(
                fixture.lead.id(),
                crate::agent_manager::TeamStorageRoot::new(fixture.root.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let mut phi = fixture.phi.clone();
        phi.hooks.register_hook(Hook::agent_start(|_| async {
            std::future::pending().await
        }));
        let mut request = crate::agent_manager::AgentSpawnRequest::new("reviewer", "Review work");
        request.alias = Some(crate::agent_manager::AgentAlias::new("security").unwrap());
        let creation = fixture
            .manager
            .spawn_agent(fixture.lead.id(), &phi, request)
            .await
            .unwrap();
        let member = fixture.manager.get(creation.instance_id()).unwrap();
        fixture.manager.interrupt(member.id()).await.unwrap();
        fixture.call(create("Review")).await;
        let prepared = update(
            "1",
            TaskChanges {
                owner: TaskAssignment::Assign("security".into()),
                ..Default::default()
            },
        )
        .prepare(fixture.access(&fixture.lead))
        .unwrap();
        assert!(
            matches!(prepared.request(), TaskRequest::Update { changes, .. } if matches!(&changes.owner, TaskAssignment::Assign(id) if id == member.id()))
        );
        prepared.execute(HookRegistry::new()).await.unwrap();
        assert_eq!(fixture.details("1").await.task.status, TaskStatus::Pending);
        let peer = fixture
            .manager
            .insert(
                fixture
                    .phi
                    .agent_instance_builder()
                    .task_list(fixture.lead.task_list().unwrap().clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert!(
            fixture
                .manager
                .for_agent(peer.id())
                .unwrap()
                .interrupt(member.id())
                .await
                .is_err()
        );
        update(
            "1",
            TaskChanges {
                owner: TaskAssignment::Assign(member.id().clone()),
                ..Default::default()
            },
        )
        .prepare(fixture.access(&peer))
        .unwrap()
        .execute(HookRegistry::new())
        .await
        .unwrap();
        let outsider = fixture
            .manager
            .insert(
                fixture
                    .phi
                    .agent_instance_builder()
                    .task_list(TaskListBinding::new(fixture.root.join("tasks"), "other").unwrap())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        for selector in [outsider.id().as_str(), "unknown-owner", ""] {
            assert!(
                update(
                    "1",
                    TaskChanges {
                        owner: TaskAssignment::Assign(selector.into()),
                        ..Default::default()
                    }
                )
                .prepare(fixture.access(&peer))
                .is_err()
            );
        }
        assert_eq!(
            fixture.details("1").await.task.owner.unwrap().instance_id,
            *member.id()
        );
        fixture.manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revoked_invocations_cannot_publish_queued_file_work() {
        let fixture = Fixture::new().await;
        let (lifetime, active) = tokio::sync::oneshot::channel();
        let access = fixture
            .access(&fixture.lead)
            .with_invocation(Arc::new(lifetime));
        let prepared = create("Revoked work").prepare(access.clone()).unwrap();
        assert!(!fixture.root.join("tasks").exists());
        drop(active);
        assert!(create("Rejected work").prepare(access).is_err());
        assert!(prepared.execute(HookRegistry::new()).await.is_err());
        assert!(!fixture.root.join("tasks").exists());
        let store =
            super::super::store::TaskStore::lock(fixture.lead.task_list().unwrap()).unwrap();
        let (lifetime, active) = tokio::sync::oneshot::channel();
        let prepared = create("Queued work")
            .prepare(
                fixture
                    .access(&fixture.lead)
                    .with_invocation(Arc::new(lifetime)),
            )
            .unwrap();
        let work = prepared.execute(HookRegistry::new());
        tokio::pin!(work);
        tokio::select! {
            biased;
            result = &mut work => panic!("file worker bypassed the list lock: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        drop(active);
        drop(store);
        assert!(work.await.is_err());
        assert!(!fixture.root.join("tasks/shared/1.json").exists());
        fixture.manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unlocked_gate_revalidates_invocation_and_owner_registration() {
        for invalidation in ["invocation", "owner"] {
            let fixture = Fixture::new().await;
            fixture.call(create("Review")).await;
            let owner = fixture
                .manager
                .insert(
                    fixture
                        .phi
                        .agent_instance_builder()
                        .task_list(fixture.lead.task_list().unwrap().clone())
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let mut hooks = HookRegistry::new();
            hooks.register_hook(Hook::task_completed({
                let entered = entered.clone();
                let release = release.clone();
                move |_| {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(TaskGateOutcome::Allow)
                    }
                }
            }));
            let (lifetime, active) = tokio::sync::oneshot::channel();
            let prepared = update(
                "1",
                TaskChanges {
                    status: Some(TaskStatus::Completed),
                    owner: TaskAssignment::Assign(owner.id().clone()),
                    ..Default::default()
                },
            )
            .prepare(
                fixture
                    .access(&fixture.lead)
                    .with_invocation(Arc::new(lifetime)),
            )
            .unwrap();
            let work = tokio::spawn(prepared.execute(hooks));
            entered.notified().await;
            let mut active = Some(active);
            if invalidation == "invocation" {
                active.take();
            } else {
                fixture.manager.stop(owner.id()).await.unwrap();
                let mut transcript = crate::transcript::Transcript::new();
                transcript.id = owner.id().as_str().into();
                fixture
                    .manager
                    .insert(
                        fixture
                            .phi
                            .agent_instance_builder()
                            .transcript(transcript)
                            .task_list(fixture.lead.task_list().unwrap().clone())
                            .build()
                            .await
                            .unwrap(),
                    )
                    .unwrap();
            }
            release.notify_one();
            assert!(work.await.unwrap().is_err(), "{invalidation}");
            let record = fixture.details("1").await;
            assert_eq!(record.task.status, TaskStatus::Pending);
            assert!(record.task.owner.is_none());
            drop(active);
            fixture.manager.shutdown().await.unwrap();
        }
    }
}
