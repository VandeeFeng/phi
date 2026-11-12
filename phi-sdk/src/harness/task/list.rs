use super::model::{Task, TaskId, TaskOwner, TaskStatus, TaskValidationError};
use super::operation::{
    TaskAccessError, TaskDetails, TaskExitAccess, TaskExitError, TaskListAccess,
};
use super::store::{TaskStore, TaskStoreError};
use crate::core::lifecycle::{
    TaskCompletedEvent, TaskCompletionTrigger, TaskCreatedEvent, TaskGateOutcome,
    TaskLifecycleStatus, TaskLifecycleTask,
};
use crate::hooks::{HookError, HookRegistry};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};

const MAX_PAGE_SIZE: usize = 100;
const MAX_PATCH_BYTES: usize = 128 * 1024;
type Records = HashMap<TaskId, Task>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskListError {
    #[error(transparent)]
    Access(#[from] TaskAccessError),
    #[error(transparent)]
    Store(#[from] TaskStoreError),
    #[error(transparent)]
    Validation(#[from] TaskValidationError),
    #[error("task {0:?} was not found in this list")]
    NotFound(TaskId),
    #[error("invalid task dependency graph: {0}")]
    Dependency(String),
    #[error("task cannot be claimed: {0}")]
    Claim(&'static str),
    #[error("task list page size must be between 1 and {MAX_PAGE_SIZE}")]
    PageSize,
    #[error("task mutation stopped after writing {written:?}: {source}")]
    PartialMutation {
        written: Vec<TaskId>,
        #[source]
        source: Box<TaskListError>,
    },
    #[error("task lifecycle gate rejected the operation: {0}")]
    Rejected(String),
    #[error("task lifecycle gate failed: {0}")]
    Hook(#[from] HookError),
    #[error("task list or caller context changed during the lifecycle gate; retry the operation")]
    Conflict,
    #[error("task file worker failed: {0}")]
    Worker(String),
}

#[derive(Debug, Clone)]
pub(super) struct TaskCreate {
    pub(super) subject: String,
    pub(super) description: String,
    pub(super) active_form: Option<String>,
    pub(super) metadata: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(super) enum OwnerUpdate {
    #[default]
    Unchanged,
    Assign(TaskOwner),
    Release,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct TaskPatch {
    pub(super) subject: Option<String>,
    pub(super) description: Option<String>,
    pub(super) active_form: Option<String>,
    pub(super) status: Option<TaskStatus>,
    pub(super) owner: OwnerUpdate,
    pub(super) add_blocks: Vec<TaskId>,
    pub(super) add_blocked_by: Vec<TaskId>,
    pub(super) metadata: Map<String, Value>,
}

#[derive(Debug)]
pub(super) struct TaskPage {
    pub(super) tasks: Vec<TaskDetails>,
    pub(super) next: Option<TaskId>,
}

#[derive(Clone)]
pub(super) struct TaskList {
    access: TaskListAccess,
    hooks: HookRegistry,
}

impl TaskList {
    pub(super) fn new(access: TaskListAccess, hooks: HookRegistry) -> Self {
        Self { access, hooks }
    }

    pub(super) async fn create(&self, input: TaskCreate) -> Result<Task, TaskListError> {
        let gated = self.hooks.has_task_created();
        let (task, pending) = self
            .run(move |store, records, access| {
                let task = Task {
                    id: store.next_id(records.values())?,
                    subject: input.subject,
                    description: input.description,
                    active_form: input.active_form,
                    status: TaskStatus::Pending,
                    owner: None,
                    blocked_by: Vec::new(),
                    metadata: input.metadata,
                };
                task.validate()?;
                TaskStore::validate_size(records.values().chain(std::iter::once(&task)))?;
                if gated {
                    Ok((task, Some(access.context()?)))
                } else {
                    access.validate()?;
                    store.write(&task)?;
                    Ok((task, None))
                }
            })
            .await?;
        let Some(context) = pending else {
            return Ok(task);
        };
        allow(
            self.hooks
                .task_created(TaskCreatedEvent {
                    context: context.clone(),
                    task: hook_task(&task),
                })
                .await?,
        )?;
        self.run(move |store, records, access| {
            if store.next_id(records.values())? != task.id || access.context()? != context {
                return Err(TaskListError::Conflict);
            }
            TaskStore::validate_size(records.values().chain(std::iter::once(&task)))?;
            access.validate()?;
            store.write(&task)?;
            Ok(task)
        })
        .await
    }

    pub(super) async fn observe(&self) -> Result<super::TaskListView, TaskListError> {
        self.run(|_, records, access| {
            let mut tasks: Vec<_> = records.values().collect();
            tasks.sort_by_key(|task| number(&task.id));
            Ok(super::TaskListView {
                list_id: access.validate()?.list_id().to_owned(),
                tasks: tasks
                    .into_iter()
                    .map(|task| view(task, &records).into())
                    .collect(),
            })
        })
        .await
    }

    pub(super) async fn get(&self, id: TaskId) -> Result<Option<TaskDetails>, TaskListError> {
        self.run(move |_, records, _| Ok(records.get(&id).map(|task| view(task, &records))))
            .await
    }

    /// Every refresh rereads canonical files; missed notifications cannot preserve stale state.
    pub(super) async fn list(
        &self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<TaskPage, TaskListError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(TaskListError::PageSize);
        }
        self.run(move |_, records, _| {
            let mut tasks: Vec<_> = records
                .values()
                .filter(|task| {
                    after
                        .as_ref()
                        .is_none_or(|id| number(&task.id) > number(id))
                })
                .collect();
            tasks.sort_by_key(|task| number(&task.id));
            let next = (tasks.len() > limit).then(|| tasks[limit - 1].id.clone());
            Ok(TaskPage {
                tasks: tasks
                    .into_iter()
                    .take(limit)
                    .map(|task| view(task, &records))
                    .collect(),
                next,
            })
        })
        .await
    }

    pub(super) async fn update(
        &self,
        id: TaskId,
        patch: TaskPatch,
    ) -> Result<TaskDetails, TaskListError> {
        if patch.add_blocks.len() + patch.add_blocked_by.len() > super::store::MAX_TASKS
            || serde_json::to_vec(&patch)
                .map_err(|error| TaskListError::Worker(error.to_string()))?
                .len()
                > MAX_PATCH_BYTES
        {
            return Err(TaskStoreError::Limit("update payload").into());
        }
        let gated = self.hooks.has_task_completed();
        let (updated, pending) = self
            .run(move |store, mut records, access| {
                let previous = records.clone();
                apply_patch(&mut records, &id, patch, access)?;
                validate_records(&records)?;
                let updated = view(records.get(&id).expect("patched task"), &records);
                if gated
                    && previous.get(&id).expect("existing task").status != TaskStatus::Completed
                    && updated.task.status == TaskStatus::Completed
                {
                    Ok((updated, Some((previous, records, access.context()?))))
                } else {
                    persist(store, access, &previous, &records, None)?;
                    Ok((updated, None))
                }
            })
            .await?;
        let Some((previous, candidate, context)) = pending else {
            return Ok(updated);
        };
        allow(
            self.hooks
                .task_completed(TaskCompletedEvent {
                    context: context.clone(),
                    task: hook_task(&updated.task),
                    trigger: TaskCompletionTrigger::ExplicitUpdate,
                })
                .await?,
        )?;
        self.run(move |store, mut records, access| {
            let changed = candidate
                .values()
                .filter(|task| previous.get(&task.id) != Some(*task))
                .map(|task| task.id.clone())
                .collect::<Vec<_>>();
            let mut expected = task_scope(&previous, changed.iter());
            expected.extend(
                task_scope(&candidate, changed.iter())
                    .keys()
                    .filter_map(|id| previous.get(id).map(|task| (id.clone(), task.clone()))),
            );
            if expected
                .iter()
                .any(|(id, task)| records.get(id) != Some(task))
                || access.context()? != context
            {
                return Err(TaskListError::Conflict);
            }
            let current = records.clone();
            for id in changed {
                records.insert(
                    id.clone(),
                    candidate.get(&id).expect("changed candidate").clone(),
                );
            }
            validate_records(&records)?;
            persist(store, access, &current, &records, None)?;
            Ok(view(
                records.get(&updated.task.id).expect("completed task"),
                &records,
            ))
        })
        .await
    }

    pub(super) async fn check_end_turn(&self) -> Result<(), TaskListError> {
        let gated = self.hooks.has_task_completed();
        let snapshot = self
            .run(move |_, records, access| {
                let context = access.teammate_context()?;
                let mut tasks: Vec<_> = records
                    .values()
                    .filter(|task| {
                        task.status == TaskStatus::InProgress
                            && task
                                .owner
                                .as_ref()
                                .is_some_and(|owner| &owner.instance_id == access.caller_id())
                    })
                    .cloned()
                    .collect();
                tasks.sort_by_key(|task| number(&task.id));
                Ok((gated && !tasks.is_empty()).then_some((records, context, tasks)))
            })
            .await?;
        let Some((previous, context, tasks)) = snapshot else {
            return Ok(());
        };
        let outcome = async {
            for task in tasks {
                allow(
                    self.hooks
                        .task_completed(TaskCompletedEvent {
                            context: context.clone(),
                            task: hook_task(&task),
                            trigger: TaskCompletionTrigger::TeammateEndTurn,
                        })
                        .await?,
                )?;
            }
            Ok(())
        }
        .await;
        self.run(move |_, records, access| {
            let owned = |task: &&Task| {
                task.status == TaskStatus::InProgress
                    && task
                        .owner
                        .as_ref()
                        .is_some_and(|owner| &owner.instance_id == access.caller_id())
            };
            let expected = task_scope(
                &previous,
                previous.values().filter(owned).map(|task| &task.id),
            );
            let current = task_scope(
                &records,
                records.values().filter(owned).map(|task| &task.id),
            );
            if current != expected || access.teammate_context()? != context {
                return Err(TaskListError::Conflict);
            }
            outcome
        })
        .await
    }

    pub(super) async fn idle_candidate(&self) -> Result<Option<Task>, TaskListError> {
        self.run(|_, records, access| {
            access.teammate_context()?;
            Ok(idle_candidate(&records, &access))
        })
        .await
    }

    #[cfg(test)]
    async fn take_idle_work(&self) -> Result<Option<Task>, TaskListError> {
        let Some(candidate) = self.idle_candidate().await? else {
            return Ok(None);
        };
        let (cancellation, active) = tokio::sync::oneshot::channel();
        let result = self.take_idle_candidate(candidate, cancellation).await;
        drop(active);
        result
    }

    // Permission covers this exact record, never a replacement selected after the callback.
    pub(super) async fn take_idle_candidate(
        &self,
        candidate: Task,
        cancellation: tokio::sync::oneshot::Sender<()>,
    ) -> Result<Option<Task>, TaskListError> {
        self.run(move |store, records, access| {
            let context = access.teammate_context()?;
            let Some(mut task) = idle_candidate(&records, &access) else {
                return Ok(None);
            };
            if task != candidate || cancellation.is_closed() {
                return Ok(None);
            }
            if access.teammate_context()? != context {
                return Err(TaskListError::Conflict);
            }
            if task.owner.is_none() {
                task.owner = Some(TaskOwner {
                    instance_id: access.caller_id().clone(),
                });
                task.validate()?;
                TaskStore::validate_size(
                    records
                        .values()
                        .filter(|previous| previous.id != task.id)
                        .chain(std::iter::once(&task)),
                )?;
                access.validate()?;
                if cancellation.is_closed() {
                    return Ok(None);
                }
                store.write(&task)?;
            }
            Ok(Some(task))
        })
        .await
    }

    pub(super) async fn delete(&self, id: TaskId) -> Result<(), TaskListError> {
        self.run(move |store, mut records, access| {
            let previous = records.clone();
            records
                .remove(&id)
                .ok_or_else(|| TaskListError::NotFound(id.clone()))?;
            for task in records.values_mut() {
                task.blocked_by.retain(|dependency| dependency != &id);
            }
            validate_records(&records)?;
            persist(store, access, &previous, &records, Some(&id))
        })
        .await
    }

    async fn run<T, F>(&self, operation: F) -> Result<T, TaskListError>
    where
        T: Send + 'static,
        F: FnOnce(&TaskStore, Records, &TaskListAccess) -> Result<T, TaskListError>
            + Send
            + 'static,
    {
        let access = self.access.clone();
        tokio::task::spawn_blocking(move || {
            let store = TaskStore::lock(access.validate()?)?;
            access.validate()?;
            let records = read_records(&store)?;
            access.validate()?;
            operation(&store, records, &access)
        })
        .await
        .map_err(|error| TaskListError::Worker(error.to_string()))?
    }
}

impl TaskExitAccess {
    pub(crate) async fn release(&self) -> Result<Vec<String>, TaskExitError> {
        let access = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let store = TaskStore::lock(access.validate()?)?;
            access.validate()?;
            let records = read_records(&store)?;
            access.validate()?;
            let mut tasks = records
                .into_values()
                .filter(|task| {
                    task.status != TaskStatus::Completed
                        && task
                            .owner
                            .as_ref()
                            .is_some_and(|owner| &owner.instance_id == access.caller_id())
                })
                .collect::<Vec<_>>();
            tasks.sort_by_key(|task| number(&task.id));
            let mut written = Vec::new();
            for mut task in tasks {
                let result: Result<(), TaskListError> = (|| {
                    task.status = TaskStatus::Pending;
                    task.owner = None;
                    task.validate()?;
                    access.validate()?;
                    store.write(&task)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    return Err(partial_error(written, error));
                }
                written.push(task.id);
            }
            Ok(written)
        })
        .await
        .map_err(|error| TaskListError::Worker(error.to_string()))
        .and_then(|result| result);
        match result {
            Ok(ids) => Ok(ids.into_iter().map(String::from).collect()),
            Err(TaskListError::PartialMutation { written, source }) => Err(TaskExitError {
                released: written.into_iter().map(String::from).collect(),
                source: *source,
            }),
            Err(source) => Err(TaskExitError {
                released: Vec::new(),
                source,
            }),
        }
    }
}

fn allow(outcome: TaskGateOutcome) -> Result<(), TaskListError> {
    match outcome {
        TaskGateOutcome::Allow => Ok(()),
        TaskGateOutcome::Reject { reason } => Err(TaskListError::Rejected(reason)),
    }
}

fn hook_task(task: &Task) -> TaskLifecycleTask {
    TaskLifecycleTask {
        id: task.id.as_str().to_owned(),
        subject: task.subject.clone(),
        description: task.description.clone(),
        active_form: task.active_form.clone(),
        status: match task.status {
            TaskStatus::Pending => TaskLifecycleStatus::Pending,
            TaskStatus::InProgress => TaskLifecycleStatus::InProgress,
            TaskStatus::Completed => TaskLifecycleStatus::Completed,
        },
        owner: task.owner.as_ref().map(|owner| owner.instance_id.clone()),
        blocked_by: task
            .blocked_by
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect(),
        metadata: task.metadata.clone(),
    }
}

fn apply_patch(
    records: &mut Records,
    id: &TaskId,
    patch: TaskPatch,
    access: &TaskListAccess,
) -> Result<(), TaskListError> {
    let original = records
        .get(id)
        .ok_or_else(|| TaskListError::NotFound(id.clone()))?;
    let claiming = matches!(&patch.owner, OwnerUpdate::Assign(owner)
        if &owner.instance_id == access.caller_id() && original.owner.as_ref() != Some(owner));
    if claiming {
        if original.owner.is_some() {
            return Err(TaskListError::Claim("already assigned to another instance"));
        }
        if original.status == TaskStatus::Completed {
            return Err(TaskListError::Claim("already completed"));
        }
    }
    let task = records.get_mut(id).expect("existing task");
    if let Some(subject) = patch.subject {
        task.subject = subject;
    }
    if let Some(description) = patch.description {
        task.description = description;
    }
    if let Some(active_form) = patch.active_form {
        task.active_form = Some(active_form);
    }
    if let Some(status) = patch.status {
        task.status = status;
    }
    match patch.owner {
        OwnerUpdate::Unchanged => {}
        OwnerUpdate::Assign(owner) => task.owner = Some(owner),
        OwnerUpdate::Release => task.owner = None,
    }
    for dependency in patch.add_blocked_by {
        if !task.blocked_by.contains(&dependency) {
            task.blocked_by.push(dependency);
        }
    }
    for (key, value) in patch.metadata {
        if value.is_null() {
            task.metadata.remove(&key);
        } else {
            task.metadata.insert(key, value);
        }
    }
    for dependant in patch.add_blocks {
        let task = records
            .get_mut(&dependant)
            .ok_or_else(|| TaskListError::NotFound(dependant.clone()))?;
        if !task.blocked_by.contains(id) {
            task.blocked_by.push(id.clone());
        }
    }
    if claiming {
        for prerequisite in &records.get(id).expect("patched task").blocked_by {
            let task = records
                .get(prerequisite)
                .ok_or_else(|| TaskListError::NotFound(prerequisite.clone()))?;
            if task.status != TaskStatus::Completed {
                return Err(TaskListError::Claim("prerequisites are unfinished"));
            }
        }
    }
    Ok(())
}

fn task_scope<'a>(records: &Records, roots: impl IntoIterator<Item = &'a TaskId>) -> Records {
    let mut scope = Records::new();
    let mut pending = roots.into_iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if scope.contains_key(&id) {
            continue;
        }
        if let Some(task) = records.get(&id) {
            pending.extend(task.blocked_by.iter().cloned());
            scope.insert(id, task.clone());
        }
    }
    scope
}

fn read_records(store: &TaskStore) -> Result<Records, TaskListError> {
    let records = store
        .read_all()?
        .into_iter()
        .map(|task| (task.id.clone(), task))
        .collect();
    validate_records(&records)?;
    Ok(records)
}

fn validate_records(records: &Records) -> Result<(), TaskListError> {
    let mut remaining = HashMap::new();
    let mut dependants: HashMap<&TaskId, Vec<&TaskId>> = HashMap::new();
    let mut ready = VecDeque::new();
    for task in records.values() {
        task.validate()?;
        remaining.insert(&task.id, task.blocked_by.len());
        if task.blocked_by.is_empty() {
            ready.push_back(&task.id);
        }
        for prerequisite in &task.blocked_by {
            if !records.contains_key(prerequisite) {
                return Err(TaskListError::Dependency(format!(
                    "task {} references missing prerequisite {}",
                    task.id.as_str(),
                    prerequisite.as_str()
                )));
            }
            dependants.entry(prerequisite).or_default().push(&task.id);
        }
    }
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for dependant in dependants.get(id).into_iter().flatten() {
            let count = remaining.get_mut(dependant).expect("validated graph node");
            *count -= 1;
            if *count == 0 {
                ready.push_back(dependant);
            }
        }
    }
    if visited != records.len() {
        return Err(TaskListError::Dependency("cycle detected".to_owned()));
    }
    TaskStore::validate_size(records.values())?;
    Ok(())
}

fn view(task: &Task, records: &Records) -> TaskDetails {
    let mut blocks: Vec<_> = records
        .values()
        .filter(|other| other.blocked_by.contains(&task.id))
        .map(|other| other.id.clone())
        .collect();
    blocks.sort_by_key(number);
    let unresolved: Vec<_> = task
        .blocked_by
        .iter()
        .filter(|id| records.get(*id).expect("validated graph").status != TaskStatus::Completed)
        .cloned()
        .collect();
    TaskDetails {
        eligible: task.status != TaskStatus::Completed && unresolved.is_empty(),
        task: task.clone(),
        blocks,
        unresolved,
    }
}

fn idle_candidate(records: &Records, access: &TaskListAccess) -> Option<Task> {
    let eligible = |task: &&Task| {
        task.status != TaskStatus::Completed
            && task
                .blocked_by
                .iter()
                .all(|id| records.get(id).expect("validated graph").status == TaskStatus::Completed)
    };
    records
        .values()
        .filter(|task| {
            task.owner
                .as_ref()
                .is_some_and(|owner| &owner.instance_id == access.caller_id())
        })
        .filter(eligible)
        .min_by_key(|task| number(&task.id))
        .or_else(|| {
            records
                .values()
                .filter(|task| task.owner.is_none() && task.status == TaskStatus::Pending)
                .filter(eligible)
                .min_by_key(|task| number(&task.id))
        })
        .cloned()
}

fn number(id: &TaskId) -> u64 {
    id.as_str().parse().expect("validated task ID")
}

fn persist(
    store: &TaskStore,
    access: &TaskListAccess,
    previous: &Records,
    candidate: &Records,
    deleted: Option<&TaskId>,
) -> Result<(), TaskListError> {
    let mut changed: Vec<_> = candidate
        .values()
        .filter(|task| previous.get(&task.id) != Some(task))
        .collect();
    changed.sort_by_key(|task| number(&task.id));
    let mut written = Vec::new();
    for task in changed {
        let result = access
            .validate()
            .map_err(TaskListError::from)
            .and_then(|_| store.write(task).map_err(TaskListError::from));
        if let Err(error) = result {
            return Err(partial_error(written, error));
        }
        written.push(task.id.clone());
    }
    // Prune references before removal so an IO failure cannot leave dangling edges.
    if let Some(id) = deleted {
        let result = access
            .validate()
            .map_err(TaskListError::from)
            .and_then(|_| store.delete(id).map_err(TaskListError::from));
        if let Err(error) = result {
            return Err(partial_error(written, error));
        }
    }
    Ok(())
}

fn partial_error(written: Vec<TaskId>, error: TaskListError) -> TaskListError {
    if written.is_empty() {
        error
    } else {
        TaskListError::PartialMutation {
            written,
            source: Box::new(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::{Phi, PhiRuntimeServices};
    use crate::harness::instance::{AgentInstance, AgentInstanceStatus};
    use crate::harness::manager::AgentManager;
    use crate::models::{ModelCatalog, ModelCost, ModelDescriptor, ModelInput};
    use crate::providers::ProviderApi;
    use crate::tasks::TaskListBinding;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            Self(Path::new("/tmp").join(format!("phi-task-list-{}", uuid::Uuid::new_v4())))
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_phi() -> Phi {
        let descriptor = ModelDescriptor {
            provider: "openai-codex".to_owned(),
            id: "gpt-5.6-sol".to_owned(),
            name: "GPT-5.6-Sol".to_owned(),
            api: ProviderApi::OpenAiCodexResponses,
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            context_window: 272_000,
            max_output_tokens: 128_000,
            cost: ModelCost::default(),
        };
        Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .agent(crate::agent::custom::AgentDefinition::new(
                "reviewer",
                "Review",
                "Review the assigned work",
            ))
            .build()
            .unwrap()
            .with_runtime_services(
                PhiRuntimeServices::new()
                    .model_catalog(Arc::new(ModelCatalog::for_test_model(descriptor).unwrap())),
            )
    }

    async fn register(
        manager: &AgentManager,
        root: &Path,
        namespace: &str,
    ) -> (AgentInstance, TaskList) {
        let phi = test_phi();
        let candidate = phi
            .agent_instance_builder()
            .task_list(TaskListBinding::new(root, namespace).unwrap())
            .build()
            .await
            .unwrap();
        let instance = manager.insert(candidate).unwrap();
        let access = manager
            .for_agent(instance.id())
            .unwrap()
            .task_list()
            .unwrap();
        (instance, TaskList::new(access, phi.hooks.clone()))
    }

    async fn teammate(
        manager: &AgentManager,
        lead: &AgentInstance,
        alias: &str,
    ) -> (AgentInstance, TaskList) {
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        let mut request = crate::agent_manager::AgentSpawnRequest::new("reviewer", "Review work");
        request.alias = Some(crate::harness::manager::AgentAlias::new(alias).unwrap());
        let creation = manager.spawn_agent(lead.id(), &phi, request).await.unwrap();
        let instance = manager.get(creation.instance_id()).unwrap();
        manager.interrupt(instance.id()).await.unwrap();
        let access = manager
            .for_agent(instance.id())
            .unwrap()
            .task_list()
            .unwrap();
        (instance, TaskList::new(access, HookRegistry::new()))
    }

    fn input(subject: &str) -> TaskCreate {
        TaskCreate {
            subject: subject.to_owned(),
            description: format!("Finish {subject}"),
            active_form: Some(format!("Working on {subject}")),
            metadata: Map::new(),
        }
    }

    fn owner(instance: &AgentInstance) -> OwnerUpdate {
        OwnerUpdate::Assign(TaskOwner {
            instance_id: instance.id().clone(),
        })
    }

    #[tokio::test]
    async fn teammate_idle_gate_contract() {
        use crate::core::lifecycle::{TeammateIdleEvent, TeammateIdleOutcome};
        use crate::hooks::Hook;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, list) = register(&manager, &root.0, "idle-hooks").await;
        std::fs::create_dir_all(&root.0).unwrap();
        let source = root.0.join("idle.lua");
        std::fs::write(&source, r#"
            on("TeammateIdle", function(event)
                assert(event.name == "TeammateIdle")
                event.payload.context.list_id = "not a rewrite"
            end)
            on("TeammateIdle", function(event)
                local mode = event.payload.context.list_id
                assert(mode ~= "not a rewrite")
                if mode == "allow" then return { action = "allow" } end
                if mode == "continue" then return { action = "continue", feedback = "finish review" } end
                if mode == "stop" then return { action = "stop" } end
                if mode == "error" then error("idle unavailable") end
                if mode == "missing action" then return {} end
                if mode == "missing feedback" then return { action = "continue" } end
                if mode == "empty feedback" then return { action = "continue", feedback = "  " } end
                if mode == "large feedback" then return { action = "continue", feedback = string.rep("x", 16385) } end
                if mode == "ambiguous" then return { action = "stop", feedback = "more work" } end
                if mode == "bad type" then return { action = false } end
                if mode == "bad output" then return false end
                if mode == "unknown field" then return { action = "allow", block = true } end
                if mode == "cyclic" then
                    local result = { action = "continue" }
                    result.feedback = result
                    return result
                end
                if mode == "function" then return { action = "continue", feedback = function() end } end
                return nil
            end)
            on("TeammateIdle", function(event)
                local mode = event.payload.context.list_id
                assert(mode ~= "not a rewrite")
            end)
        "#).unwrap();
        let later_calls = Arc::new(AtomicUsize::new(0));
        let mut hooks = HookRegistry::new();
        hooks.register_hook(Hook::teammate_idle(|event| async move {
            match event.context.list_id.as_str() {
                "rust continue" => Ok(TeammateIdleOutcome::Continue {
                    feedback: "keep going".into(),
                }),
                "rust stop" => Ok(TeammateIdleOutcome::Stop),
                "rust error" => Err(HookError::new("idle unavailable")),
                "rust empty" => Ok(TeammateIdleOutcome::Continue {
                    feedback: "\n ".into(),
                }),
                "rust large" => Ok(TeammateIdleOutcome::Continue {
                    feedback: "x".repeat(16 * 1024 + 1),
                }),
                _ => Ok(TeammateIdleOutcome::Allow),
            }
        }));
        hooks.register_hook(Hook::teammate_idle({
            let calls = later_calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(TeammateIdleOutcome::Allow) }
            }
        }));
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut hooks);
        for (mode, expected) in [
            (
                "rust continue",
                TeammateIdleOutcome::Continue {
                    feedback: "keep going".into(),
                },
            ),
            ("rust stop", TeammateIdleOutcome::Stop),
            ("allow", TeammateIdleOutcome::Allow),
            ("nil", TeammateIdleOutcome::Allow),
            (
                "continue",
                TeammateIdleOutcome::Continue {
                    feedback: "finish review".into(),
                },
            ),
            ("stop", TeammateIdleOutcome::Stop),
        ] {
            let mut context = list.access.context().unwrap();
            context.list_id = mode.into();
            let before = later_calls.load(Ordering::SeqCst);
            assert_eq!(
                hooks
                    .teammate_idle(TeammateIdleEvent { context })
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(later_calls.load(Ordering::SeqCst) - before, 1);
        }
        for mode in [
            "rust error",
            "rust empty",
            "rust large",
            "error",
            "missing action",
            "missing feedback",
            "empty feedback",
            "large feedback",
            "ambiguous",
            "bad type",
            "bad output",
            "unknown field",
            "cyclic",
            "function",
        ] {
            let mut context = list.access.context().unwrap();
            context.list_id = mode.into();
            let before = later_calls.load(Ordering::SeqCst);
            assert!(
                hooks
                    .teammate_idle(TeammateIdleEvent { context })
                    .await
                    .is_err(),
                "{mode}"
            );
            assert_eq!(later_calls.load(Ordering::SeqCst) - before, 1);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn teammate_end_turn_checks_do_not_complete_work() {
        use crate::hooks::Hook;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, lead_list) = register(&manager, &root.0, "end-turn").await;
        assert!(matches!(
            lead_list.check_end_turn().await,
            Err(TaskListError::Access(TaskAccessError::NotTeammate))
        ));
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            lead_list.take_idle_work().await,
            Err(TaskListError::Access(TaskAccessError::NotTeammate))
        ));
        let (member, mut list) = teammate(&manager, &lead, "security").await;
        let (_, peer) = teammate(&manager, &lead, "performance").await;
        let mut checked = Vec::new();
        for (status, assigned) in [
            (TaskStatus::Pending, true),
            (TaskStatus::InProgress, true),
            (TaskStatus::InProgress, false),
            (TaskStatus::Completed, true),
            (TaskStatus::InProgress, true),
        ] {
            let task = list.create(input("review")).await.unwrap();
            lead_list
                .update(
                    task.id.clone(),
                    TaskPatch {
                        status: Some(status),
                        owner: if assigned {
                            owner(&member)
                        } else {
                            OwnerUpdate::Unchanged
                        },
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            if assigned && status == TaskStatus::InProgress {
                checked.push(task.id.as_str().to_owned());
            }
        }
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mode = Arc::new(AtomicUsize::new(0));
        list.hooks.register_hook(Hook::task_completed({
            let calls = calls.clone();
            let mode = mode.clone();
            let instance_id = member.id().clone();
            move |event| {
                assert_eq!(event.trigger, TaskCompletionTrigger::TeammateEndTurn);
                assert_eq!(event.task.status, TaskLifecycleStatus::InProgress);
                assert_eq!(event.task.owner.as_ref(), Some(&instance_id));
                assert_eq!(event.context.alias.as_ref().unwrap().as_str(), "security");
                calls.lock().unwrap().push(event.task.id);
                let mode = mode.load(Ordering::SeqCst);
                async move {
                    match mode {
                        1 => Ok(TaskGateOutcome::Reject {
                            reason: "finish checks".into(),
                        }),
                        2 => Err(HookError::new("gate unavailable")),
                        _ => Ok(TaskGateOutcome::Allow),
                    }
                }
            }
        }));
        let before = list.run(|_, records, _| Ok(records)).await.unwrap();
        list.check_end_turn().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), checked);
        for outcome in [1, 2] {
            mode.store(outcome, Ordering::SeqCst);
            let result = list.check_end_turn().await;
            if outcome == 1 {
                assert!(matches!(result, Err(TaskListError::Rejected(_))));
            } else {
                assert!(matches!(result, Err(TaskListError::Hook(_))));
            }
            assert_eq!(list.run(|_, records, _| Ok(records)).await.unwrap(), before);
        }
        let mut peer = peer;
        peer.hooks = list.hooks.clone();
        let count = calls.lock().unwrap().len();
        peer.check_end_turn().await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), count);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn teammate_end_turn_revalidates_unlocked_hooks() {
        use crate::hooks::Hook;
        for change in ["owner", "delete", "close"] {
            let root = TestRoot::new();
            let manager = AgentManager::new();
            let (lead, lead_list) = register(&manager, &root.0, "end-turn-race").await;
            manager
                .initialize_team(
                    lead.id(),
                    crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
                )
                .await
                .unwrap();
            let (member, mut list) = teammate(&manager, &lead, "security").await;
            let task = list.create(input("review")).await.unwrap();
            list.update(
                task.id.clone(),
                TaskPatch {
                    status: Some(TaskStatus::InProgress),
                    owner: owner(&member),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let id = task.id.clone();
            let peer = lead_list.clone();
            let member = member.clone();
            list.hooks.register_hook(Hook::task_completed(move |_| {
                let peer = peer.clone();
                let id = id.clone();
                let member = member.clone();
                async move {
                    match change {
                        "owner" => {
                            peer.update(
                                id,
                                TaskPatch {
                                    owner: OwnerUpdate::Release,
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap();
                        }
                        "delete" => peer.delete(id).await.unwrap(),
                        _ => member.close_registration(),
                    }
                    Ok(TaskGateOutcome::Reject {
                        reason: "stale feedback".into(),
                    })
                }
            }));
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(5), list.check_end_turn())
                    .await
                    .unwrap();
            if change == "close" {
                assert!(
                    matches!(result, Err(TaskListError::Access(_))),
                    "{result:?}"
                );
            } else {
                assert!(matches!(result, Err(TaskListError::Conflict)), "{result:?}");
            }
            if let Some(current) = lead_list.get(task.id).await.unwrap() {
                assert_eq!(current.task.status, TaskStatus::InProgress);
                if change == "owner" {
                    assert!(current.task.owner.is_none());
                }
            } else {
                assert_eq!(change, "delete");
            }
            manager.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn teammate_exit_releases_only_unfinished_owned_work() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, list) = register(&manager, &root.0, "exit-work").await;
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (security, security_list) = teammate(&manager, &lead, "security").await;
        let (performance, _) = teammate(&manager, &lead, "performance").await;
        let mut before = Vec::new();
        for status in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
        ] {
            for assigned in [None, Some(&security), Some(&performance)] {
                let task = list.create(input("retained work")).await.unwrap();
                let task = list
                    .update(
                        task.id,
                        TaskPatch {
                            status: Some(status),
                            owner: assigned.map_or(OwnerUpdate::Unchanged, owner),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap()
                    .task;
                before.push(task);
            }
        }
        list.update(
            before[0].id.clone(),
            TaskPatch {
                add_blocked_by: vec![before[1].id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        before[0] = list.get(before[0].id.clone()).await.unwrap().unwrap().task;
        manager.interrupt(security.id()).await.unwrap();
        for task in &before {
            assert_eq!(
                &list.get(task.id.clone()).await.unwrap().unwrap().task,
                task
            );
        }
        let (closing, epoch, captured) = {
            let mut registry = manager.lock().unwrap();
            let epoch = registry.member(security.id()).unwrap().epoch;
            assert!(TaskExitAccess::capture(&manager, &registry, security.id(), epoch,).is_err());
            let closing = registry.close_members(|member| member.epoch == epoch);
            assert!(
                TaskExitAccess::capture(&manager, &registry, security.id(), uuid::Uuid::new_v4(),)
                    .is_err()
            );
            let captured =
                TaskExitAccess::capture(&manager, &registry, security.id(), epoch).unwrap();
            (closing, epoch, captured)
        };
        assert!(security_list.access.validate().is_err());
        let store = TaskStore::lock(lead.task_list().unwrap()).unwrap();
        let captured_manager = manager.clone();
        let stopping = tokio::spawn(async move { captured_manager.stop_members(closing).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while security.snapshot().status
                != crate::harness::instance::AgentInstanceStatus::Stopped
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        stopping.abort();
        assert!(stopping.await.unwrap_err().is_cancelled());
        drop(store);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while manager.lock().unwrap().members.contains_key(security.id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for mut task in before {
            if task.status != TaskStatus::Completed
                && task
                    .owner
                    .as_ref()
                    .is_some_and(|owner| &owner.instance_id == security.id())
            {
                task.status = TaskStatus::Pending;
                task.owner = None;
            }
            assert_eq!(list.get(task.id.clone()).await.unwrap().unwrap().task, task);
        }
        assert!(captured.release().await.is_err());
        let mut transcript = crate::transcript::Transcript::new();
        transcript.id = security.id().as_str().into();
        let phi = test_phi();
        let replacement = manager
            .insert(
                phi.agent_instance_builder()
                    .transcript(transcript)
                    .task_list(lead.task_list().unwrap().clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert_ne!(
            manager
                .lock()
                .unwrap()
                .member(replacement.id())
                .unwrap()
                .epoch,
            epoch
        );
        assert!(captured.release().await.is_err());
        manager.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn teammate_partial_exit_release_retries_without_losing_notice_ids() {
        const CHILD: &str = "PHI_TASK_EXIT_IO_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "harness::task::list::tests::teammate_partial_exit_release_retries_without_losing_notice_ids", "--nocapture"])
                .env(CHILD, "1")
                .kill_on_drop(true)
                .output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, list) = register(&manager, &root.0, "partial-exit").await;
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (member, _) = teammate(&manager, &lead, "security").await;
        let mut tasks = Vec::new();
        for description in ["small".to_owned(), "large".repeat(1024)] {
            let task = list
                .create(TaskCreate {
                    description,
                    ..input("release")
                })
                .await
                .unwrap();
            let task = list
                .update(
                    task.id,
                    TaskPatch {
                        owner: owner(&member),
                        status: Some(TaskStatus::InProgress),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .task;
            tasks.push(task);
        }
        let mut previous = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut previous), 0);
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            let limit = libc::rlimit {
                rlim_cur: 1024,
                rlim_max: previous.rlim_max,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
        }
        let result = manager.stop(member.id()).await;
        unsafe {
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &previous), 0);
        }
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("1"));
        let first = list.get(tasks[0].id.clone()).await.unwrap().unwrap().task;
        assert_eq!(first.status, TaskStatus::Pending);
        assert!(first.owner.is_none());
        assert_eq!(
            list.get(tasks[1].id.clone()).await.unwrap().unwrap().task,
            tasks[1]
        );
        assert!(manager.lock().unwrap().member(member.id()).unwrap().closing);
        let namespace = root
            .0
            .join("teams")
            .join(crate::agent_manager::team_namespace(lead.id().as_str()).unwrap());
        let lead_inbox = namespace.join("inboxes/lead.json");
        let record: Value = serde_json::from_slice(&std::fs::read(&lead_inbox).unwrap()).unwrap();
        assert!(record["messages"].as_array().unwrap().is_empty());
        let (left, right) = tokio::join!(manager.stop(member.id()), manager.stop(member.id()));
        left.unwrap();
        right.unwrap();
        let record: Value = serde_json::from_slice(&std::fs::read(&lead_inbox).unwrap()).unwrap();
        let notices = record["messages"].as_array().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]["content"]
                .as_str()
                .unwrap()
                .contains("[\"1\",\"2\"]")
        );
        assert!(!namespace.join("inboxes/security.json").exists());
        manager.shutdown().await.unwrap();
        assert!(!namespace.exists());
        assert!(root.0.join("partial-exit/1.json").is_file());
    }

    #[tokio::test]
    async fn teammate_claim_does_not_reuse_stale_or_cancelled_permission() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, list) = register(&manager, &root.0, "idle-permission").await;
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (_, member_list) = teammate(&manager, &lead, "security").await;
        let first = list.create(input("first")).await.unwrap();
        let second = list.create(input("second")).await.unwrap();
        for change in ["content", "completed", "cancelled"] {
            let candidate = member_list.idle_candidate().await.unwrap().unwrap();
            let (cancellation, active) = tokio::sync::oneshot::channel();
            let mut active = Some(active);
            match change {
                "content" => {
                    list.update(
                        first.id.clone(),
                        TaskPatch {
                            subject: Some("changed after authorization".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                }
                "completed" => {
                    list.update(
                        first.id.clone(),
                        TaskPatch {
                            status: Some(TaskStatus::Completed),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                }
                _ => drop(active.take()),
            }
            assert!(
                member_list
                    .take_idle_candidate(candidate, cancellation)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                list.get(first.id.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .task
                    .owner
                    .is_none()
            );
            assert!(
                list.get(second.id.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .task
                    .owner
                    .is_none()
            );
            drop(active);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn teammate_idle_claims_are_ordered_and_owner_only() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, list) = register(&manager, &root.0, "idle-claims").await;
        let team = manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (security, security_list) = teammate(&manager, &lead, "security").await;
        let (performance, performance_list) = teammate(&manager, &lead, "performance").await;
        let task = list.create(input("one winner")).await.unwrap();
        let (left, right) = tokio::join!(
            security_list.take_idle_work(),
            performance_list.take_idle_work()
        );
        let results = [left.unwrap(), right.unwrap()];
        assert_eq!(results.iter().filter(|task| task.is_some()).count(), 1);
        let claimed = list.get(task.id.clone()).await.unwrap().unwrap().task;
        assert_eq!(claimed.status, TaskStatus::Pending);
        assert!(claimed.owner.is_some());
        for _ in 0..10 {
            list.create(input("ordered work")).await.unwrap();
        }
        let assigned = list.create(input("assigned first")).await.unwrap();
        list.update(
            assigned.id.clone(),
            TaskPatch {
                owner: owner(&security),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        list.update(
            task.id.clone(),
            TaskPatch {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            security_list.take_idle_work().await.unwrap().unwrap().id,
            assigned.id
        );
        list.update(
            assigned.id.clone(),
            TaskPatch {
                add_blocked_by: vec![TaskId::try_from("2".to_owned()).unwrap()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        list.update(
            TaskId::try_from("3".to_owned()).unwrap(),
            TaskPatch {
                status: Some(TaskStatus::InProgress),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        manager
            .set_team_capability(&team, crate::agent_manager::TeamCapability::Disabled)
            .unwrap();
        let selected = security_list.take_idle_work().await.unwrap().unwrap();
        assert_eq!(selected.id.as_str(), "2");
        assert_eq!(selected.status, TaskStatus::Pending);
        assert_eq!(selected.owner.unwrap().instance_id, *security.id());
        manager.interrupt(security.id()).await.unwrap();
        assert_eq!(
            security_list
                .take_idle_work()
                .await
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "2"
        );
        assert_eq!(
            performance_list
                .take_idle_work()
                .await
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "4"
        );
        for id in ["2", "4", "5", "6", "7", "8", "9", "10", "11"] {
            list.update(
                TaskId::try_from(id.to_owned()).unwrap(),
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            security_list.take_idle_work().await.unwrap().unwrap().id,
            assigned.id
        );
        list.update(
            assigned.id,
            TaskPatch {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(performance_list.take_idle_work().await.unwrap().is_none());
        assert_eq!(
            list.get(TaskId::try_from("3".to_owned()).unwrap())
                .await
                .unwrap()
                .unwrap()
                .task
                .owner,
            None
        );
        security.close_registration();
        assert!(matches!(
            security_list.take_idle_work().await,
            Err(TaskListError::Access(_))
        ));
        assert_ne!(performance.id(), security.id());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn teammate_idle_claim_respects_list_size() {
        const LIST_BYTE_LIMIT: usize = 16 * 1024 * 1024;
        const DESCRIPTION_BYTES: usize = 64 * 1024;
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (lead, list) = register(&manager, &root.0, "idle-size").await;
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.0.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (_, member_list) = teammate(&manager, &lead, "security").await;
        list.run(|store, _, _| {
            let mut tasks: Vec<_> = (1..=LIST_BYTE_LIMIT / DESCRIPTION_BYTES)
                .map(|id| Task {
                    id: TaskId::try_from(id.to_string()).unwrap(),
                    subject: "review".into(),
                    description: "x".repeat(DESCRIPTION_BYTES),
                    active_form: None,
                    status: TaskStatus::Pending,
                    owner: None,
                    blocked_by: Vec::new(),
                    metadata: Map::new(),
                })
                .collect();
            let bytes: usize = tasks
                .iter()
                .map(|task| serde_json::to_vec(task).unwrap().len())
                .sum();
            tasks
                .last_mut()
                .unwrap()
                .description
                .truncate(DESCRIPTION_BYTES - (bytes - LIST_BYTE_LIMIT));
            TaskStore::validate_size(&tasks)?;
            for task in tasks {
                store.write(&task)?;
            }
            Ok(())
        })
        .await
        .unwrap();
        assert!(matches!(
            member_list.take_idle_work().await,
            Err(TaskListError::Store(TaskStoreError::Limit("list size")))
        ));
        list.run(|_, records, _| {
            assert!(records.values().all(|task| task.owner.is_none()));
            Ok(())
        })
        .await
        .unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_gates_preserve_state() {
        use crate::hooks::Hook;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (instance, mut list) = register(&manager, &root.0, "gates").await;
        let calls = Arc::new(AtomicUsize::new(0));
        list.hooks
            .register_hook(Hook::task_created(|event| async move {
                assert_eq!(event.context.list_id, "gates");
                assert_eq!(event.task.status, TaskLifecycleStatus::Pending);
                match event.task.subject.as_str() {
                    "reject" => Ok(TaskGateOutcome::Reject {
                        reason: "revise scope".into(),
                    }),
                    "error" => Err(HookError::new("gate unavailable")),
                    _ => Ok(TaskGateOutcome::Allow),
                }
            }));
        assert!(
            matches!(list.create(input("reject")).await, Err(TaskListError::Rejected(reason)) if reason == "revise scope")
        );
        assert!(matches!(
            list.create(input("error")).await,
            Err(TaskListError::Hook(_))
        ));
        assert!(list.list(None, 10).await.unwrap().tasks.is_empty());
        let task = list.create(input("accepted")).await.unwrap();
        assert_eq!(task.id.as_str(), "1");
        let dependant = list.create(input("dependant")).await.unwrap();
        list.update(
            dependant.id.clone(),
            TaskPatch {
                add_blocked_by: vec![task.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        list.hooks.register_hook(Hook::task_completed({
            let calls = calls.clone();
            let instance_id = instance.id().clone();
            move |event| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(event.context.instance_id, instance_id);
                async move {
                    assert_eq!(event.trigger, TaskCompletionTrigger::ExplicitUpdate);
                    assert_eq!(event.task.status, TaskLifecycleStatus::Completed);
                    match event.task.description.as_str() {
                        "allow" => Ok(TaskGateOutcome::Allow),
                        "error" => Err(HookError::new("completion unavailable")),
                        _ => Ok(TaskGateOutcome::Reject {
                            reason: "run checks".into(),
                        }),
                    }
                }
            }
        }));
        list.hooks.register_hook(Hook::task_completed(|event| {
            assert!(matches!(
                event.task.description.as_str(),
                "allow" | "reject" | "error"
            ));
            async { Ok(TaskGateOutcome::Allow) }
        }));
        for description in ["reject", "error"] {
            assert!(
                list.update(
                    task.id.clone(),
                    TaskPatch {
                        description: Some(description.into()),
                        status: Some(TaskStatus::Completed),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
            );
            assert_eq!(list.get(task.id.clone()).await.unwrap().unwrap().task, task);
            assert!(
                !list
                    .get(dependant.id.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .eligible
            );
        }
        for (status, expected_calls) in [
            (TaskStatus::Completed, 3),
            (TaskStatus::Completed, 3),
            (TaskStatus::Pending, 3),
            (TaskStatus::Completed, 4),
        ] {
            list.update(
                task.id.clone(),
                TaskPatch {
                    description: Some("allow".into()),
                    status: Some(status),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                list.get(dependant.id.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .eligible,
                status == TaskStatus::Completed
            );
            list.list(None, 10).await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        }
        list.delete(task.id).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lua_task_gate_contract() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, mut list) = register(&manager, &root.0, "lua-gates").await;
        std::fs::create_dir_all(&root.0).unwrap();
        let source = root.0.join("gates.lua");
        std::fs::write(
            &source,
            r#"
            on("TaskCreated", function(event)
                assert(event.name == "TaskCreated")
                assert(event.payload.context.list_id == "lua-gates")
                assert(event.payload.context.team == nil)
                assert(event.payload.task.status == "pending")
                if event.payload.task.subject == "reject" then
                    return { block = true, reason = "narrow scope" }
                end
                event.payload.task.subject = "not a rewrite"
            end)
            on("TaskCreated", function(event)
                assert(event.payload.task.subject ~= "not a rewrite")
            end)
            on("TaskCompleted", function(event)
                if event.payload.trigger == "teammate_end_turn" then
                    assert(event.payload.task.status == "in_progress")
                    return { block = true, reason = "finish review" }
                end
                assert(event.payload.trigger == "explicit_update")
                assert(event.payload.task.status == "completed")
                local mode = event.payload.task.description
                if mode == "reject" then return { cancel = true, reason = "missing checks" } end
                if mode == "mixed decision" then return { block = true, cancel = false } end
                if mode == "error" then error("failed completion") end
                if mode == "bad boolean" then return { block = "false" } end
                if mode == "bad output" then return false end
                if mode == "rewrite" then return { subject = "rewrite" } end
                if mode == "cyclic reason" then
                    local cyclic = {}
                    cyclic.reason = cyclic
                    return cyclic
                end
                if mode == "function reason" then return { reason = function() end } end
                return {}
            end)
        "#,
        )
        .unwrap();
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut list.hooks);
        assert!(
            matches!(list.create(input("reject")).await, Err(TaskListError::Rejected(reason)) if reason == "narrow scope")
        );
        let task = list.create(input("original")).await.unwrap();
        assert_eq!(task.subject, "original");
        let mut end_turn_task = hook_task(&task);
        end_turn_task.status = TaskLifecycleStatus::InProgress;
        assert_eq!(
            list.hooks
                .task_completed(TaskCompletedEvent {
                    context: list.access.context().unwrap(),
                    task: end_turn_task,
                    trigger: TaskCompletionTrigger::TeammateEndTurn,
                })
                .await
                .unwrap(),
            TaskGateOutcome::Reject {
                reason: "finish review".into()
            }
        );
        assert_eq!(list.get(task.id.clone()).await.unwrap().unwrap().task, task);
        for description in [
            "reject",
            "mixed decision",
            "error",
            "bad boolean",
            "bad output",
            "rewrite",
            "cyclic reason",
            "function reason",
        ] {
            let result = list
                .update(
                    task.id.clone(),
                    TaskPatch {
                        description: Some(description.into()),
                        status: Some(TaskStatus::Completed),
                        ..Default::default()
                    },
                )
                .await;
            assert!(result.is_err(), "{description}");
            assert_eq!(list.get(task.id.clone()).await.unwrap().unwrap().task, task);
        }
        list.update(
            task.id,
            TaskPatch {
                description: Some("allow".into()),
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn creation_gate_conflicts() {
        use crate::hooks::Hook;
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (instance, mut list) = register(&manager, &root.0, "creation-race").await;
        let peer = list.clone();
        list.hooks.register_hook(Hook::task_created(move |_| {
            let peer = peer.clone();
            async move {
                let competing = peer.create(input("competing")).await.unwrap();
                peer.delete(competing.id).await.unwrap();
                Ok(TaskGateOutcome::Allow)
            }
        }));
        assert!(matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                list.create(input("candidate"))
            )
            .await
            .unwrap(),
            Err(TaskListError::Conflict)
        ));
        assert!(list.list(None, 10).await.unwrap().tasks.is_empty());
        list.hooks = HookRegistry::new();
        let team_manager = manager.clone();
        let lead_id = instance.id().clone();
        list.hooks.register_hook(Hook::task_created(move |_| {
            let manager = team_manager.clone();
            let lead = lead_id.clone();
            async move {
                manager
                    .initialize_team(
                        &lead,
                        crate::agent_manager::TeamStorageRoot::new(
                            std::env::temp_dir()
                                .join(format!("phi-task-gate-team-{}", uuid::Uuid::new_v4())),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                Ok(TaskGateOutcome::Allow)
            }
        }));
        assert!(matches!(
            list.create(input("changed team")).await,
            Err(TaskListError::Conflict)
        ));
        list.hooks = HookRegistry::new();
        let closing = instance.clone();
        list.hooks.register_hook(Hook::task_created(move |_| {
            closing.close_registration();
            async { Ok(TaskGateOutcome::Allow) }
        }));
        assert!(matches!(
            list.create(input("closed caller")).await,
            Err(TaskListError::Access(_))
        ));
        let store = TaskStore::lock(instance.task_list().unwrap()).unwrap();
        assert!(store.read_all().unwrap().is_empty());
        drop(store);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completion_gate_preserves_unrelated_concurrent_changes() {
        use crate::hooks::Hook;
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, mut list) = register(&manager, &root.0, "independent-completion").await;
        let task = list.create(input("work")).await.unwrap();
        let unrelated = list.create(input("other work")).await.unwrap();
        let peer = list.clone();
        let unrelated_id = unrelated.id.clone();
        list.hooks.register_hook(Hook::task_completed(move |_| {
            let peer = peer.clone();
            let id = unrelated_id.clone();
            async move {
                peer.update(
                    id,
                    TaskPatch {
                        description: Some("concurrent edit".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                Ok(TaskGateOutcome::Allow)
            }
        }));
        let completed = list
            .update(
                task.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(completed.task.status, TaskStatus::Completed);
        assert_eq!(
            list.get(unrelated.id)
                .await
                .unwrap()
                .unwrap()
                .task
                .description,
            "concurrent edit"
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completion_gate_conflicts() {
        use crate::hooks::Hook;
        for change in ["content", "owner", "prerequisite", "delete"] {
            let root = TestRoot::new();
            let manager = AgentManager::new();
            let (_, mut list) = register(&manager, &root.0, "completion-race").await;
            let task = list.create(input("work")).await.unwrap();
            let prerequisite = list.create(input("prerequisite")).await.unwrap();
            list.update(
                task.id.clone(),
                TaskPatch {
                    add_blocked_by: vec![prerequisite.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let peer = list.clone();
            let id = task.id.clone();
            let owner_id = crate::harness::instance::AgentInstanceId::new("assigned-peer");
            list.hooks.register_hook(Hook::task_completed(move |_| {
                let peer = peer.clone();
                let id = id.clone();
                let prerequisite = prerequisite.id.clone();
                let owner_id = owner_id.clone();
                async move {
                    match change {
                        "delete" => peer.delete(id).await.unwrap(),
                        "prerequisite" => {
                            peer.update(
                                prerequisite,
                                TaskPatch {
                                    status: Some(TaskStatus::Completed),
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap();
                        }
                        _ => {
                            peer.update(
                                id,
                                TaskPatch {
                                    description: (change == "content").then(|| "peer edit".into()),
                                    owner: if change == "owner" {
                                        OwnerUpdate::Assign(TaskOwner {
                                            instance_id: owner_id,
                                        })
                                    } else {
                                        OwnerUpdate::Unchanged
                                    },
                                    ..Default::default()
                                },
                            )
                            .await
                            .unwrap();
                        }
                    }
                    Ok(TaskGateOutcome::Allow)
                }
            }));
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                list.update(
                    task.id.clone(),
                    TaskPatch {
                        status: Some(TaskStatus::Completed),
                        ..Default::default()
                    },
                ),
            )
            .await
            .unwrap();
            assert!(
                matches!(result, Err(TaskListError::Conflict)),
                "{change}: {result:?}"
            );
            if let Some(current) = list.get(task.id).await.unwrap() {
                assert_eq!(current.task.status, TaskStatus::Pending);
                if change == "content" {
                    assert_eq!(current.task.description, "peer edit");
                }
                if change == "owner" {
                    assert_eq!(
                        current.task.owner.unwrap().instance_id.as_str(),
                        "assigned-peer"
                    );
                }
            } else {
                assert_eq!(change, "delete");
            }
            manager.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn cooperative_file_work() {
        let root = TestRoot::new();
        let first_manager = AgentManager::new();
        let second_manager = AgentManager::new();
        let (lead, list) = register(&first_manager, &root.0, "shared").await;
        let (worker, peer) = register(&second_manager, &root.0, "shared").await;
        let (_, isolated) = register(&second_manager, &root.0, "isolated").await;
        let prerequisite = list.create(input("Review security")).await.unwrap();
        let dependant = list.create(input("Review integration")).await.unwrap();
        assert_eq!(prerequisite.id.as_str(), "1");
        assert!(isolated.list(None, 10).await.unwrap().tasks.is_empty());
        let assigned = list
            .update(
                dependant.id.clone(),
                TaskPatch {
                    owner: owner(&worker),
                    add_blocked_by: vec![prerequisite.id.clone()],
                    metadata: json!({"keep": true, "nested": {"old": true}})
                        .as_object()
                        .unwrap()
                        .clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(assigned.task.status, TaskStatus::Pending);
        assert!(!assigned.eligible);
        assert_eq!(worker.snapshot().status, AgentInstanceStatus::Idle);
        assert_eq!(lead.snapshot().status, AgentInstanceStatus::Idle);
        assert!(worker.transcript().await.unwrap().entries.is_empty());
        assert!(lead.transcript().await.unwrap().entries.is_empty());
        assert_eq!(first_manager.discover().unwrap().len(), 1);
        assert_eq!(second_manager.discover().unwrap().len(), 2);
        assert_eq!(
            peer.get(prerequisite.id.clone())
                .await
                .unwrap()
                .unwrap()
                .blocks,
            [dependant.id.clone()]
        );
        let claimed = peer
            .update(
                prerequisite.id.clone(),
                TaskPatch {
                    owner: owner(&worker),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(claimed.task.status, TaskStatus::Pending);
        peer.update(
            prerequisite.id.clone(),
            TaskPatch {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            list.get(dependant.id.clone())
                .await
                .unwrap()
                .unwrap()
                .eligible
        );
        list.update(
            dependant.id.clone(),
            TaskPatch {
                status: Some(TaskStatus::Completed),
                metadata: json!({"keep": null, "nested": {"new": true}})
                    .as_object()
                    .unwrap()
                    .clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Reopening prerequisites never rolls back downstream work or stops its instance.
        list.update(
            prerequisite.id.clone(),
            TaskPatch {
                status: Some(TaskStatus::InProgress),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let current = peer.get(dependant.id.clone()).await.unwrap().unwrap();
        assert_eq!(current.task.status, TaskStatus::Completed);
        assert_eq!(current.unresolved, [prerequisite.id.clone()]);
        assert_eq!(
            current.task.metadata,
            json!({"nested": {"new": true}})
                .as_object()
                .unwrap()
                .clone()
        );
        peer.update(
            dependant.id.clone(),
            TaskPatch {
                owner: OwnerUpdate::Release,
                status: Some(TaskStatus::Pending),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            peer.update(
                dependant.id.clone(),
                TaskPatch {
                    owner: owner(&worker),
                    ..Default::default()
                }
            )
            .await,
            Err(TaskListError::Claim(_))
        ));
        list.delete(prerequisite.id.clone()).await.unwrap();
        assert!(peer.get(prerequisite.id).await.unwrap().is_none());
        let current = peer.get(dependant.id.clone()).await.unwrap().unwrap();
        assert!(current.task.blocked_by.is_empty());
        assert!(current.eligible);
        peer.update(
            dependant.id.clone(),
            TaskPatch {
                owner: owner(&worker),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            list.update(
                dependant.id.clone(),
                TaskPatch {
                    owner: owner(&lead),
                    ..Default::default()
                }
            )
            .await,
            Err(TaskListError::Claim(_))
        ));
        peer.delete(dependant.id).await.unwrap();
        second_manager.shutdown().await.unwrap();
        let next = list.create(input("Keep IDs after deletion")).await.unwrap();
        assert_eq!(next.id.as_str(), "3");
        first_manager.shutdown().await.unwrap();
        let third_manager = AgentManager::new();
        let (_, reopened) = register(&third_manager, &root.0, "shared").await;
        assert_eq!(
            reopened.get(next.id.clone()).await.unwrap().unwrap().task,
            next
        );
        assert!(matches!(
            list.create(input("Revoked")).await,
            Err(TaskListError::Access(_))
        ));
        third_manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_dependency_edits() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, list) = register(&manager, &root.0, "graph").await;
        let created = futures::future::join_all((0..16).map(|index| {
            let list = list.clone();
            async move { list.create(input(&format!("Work {index}"))).await.unwrap() }
        }))
        .await;
        let mut ids: Vec<_> = created.into_iter().map(|task| task.id).collect();
        ids.sort_by_key(number);
        assert_eq!(
            ids.iter().map(number).collect::<Vec<_>>(),
            (1..=16).collect::<Vec<_>>()
        );
        for pair in ids.windows(2) {
            list.update(
                pair[0].clone(),
                TaskPatch {
                    add_blocks: vec![pair[1].clone(), pair[1].clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        for index in 0..ids.len() {
            let previous = list.get(ids[index].clone()).await.unwrap().unwrap().task;
            for dependency in [&ids[index], ids.last().unwrap()] {
                assert!(
                    list.update(
                        ids[index].clone(),
                        TaskPatch {
                            add_blocked_by: vec![dependency.clone()],
                            ..Default::default()
                        }
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    list.get(ids[index].clone()).await.unwrap().unwrap().task,
                    previous
                );
            }
        }
        let foreign = TaskId::try_from("99".to_owned()).unwrap();
        assert!(matches!(
            list.update(
                ids[0].clone(),
                TaskPatch {
                    add_blocked_by: vec![foreign],
                    ..Default::default()
                }
            )
            .await,
            Err(TaskListError::Dependency(_))
        ));
        let mut cursor = None;
        let mut observed = Vec::new();
        loop {
            let page = list.list(cursor, 3).await.unwrap();
            observed.extend(page.tasks.into_iter().map(|view| view.task.id));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(observed, ids);
        assert!(matches!(
            list.list(None, 0).await,
            Err(TaskListError::PageSize)
        ));
        list.delete(ids[7].clone()).await.unwrap();
        assert!(
            list.get(ids[8].clone())
                .await
                .unwrap()
                .unwrap()
                .task
                .blocked_by
                .is_empty()
        );
        let path = root
            .0
            .join("graph")
            .join(format!("{}.json", ids[0].as_str()));
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            list.list(None, 10).await,
            Err(TaskListError::Dependency(_))
        ));
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(list.list(None, 100).await.unwrap().tasks.len(), 15);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cross_process_claims() {
        const ROOT_ENV: &str = "PHI_TEST_TASK_CLAIM_ROOT";
        const WORKER_ENV: &str = "PHI_TEST_TASK_CLAIM_WORKER";
        if let Some(root) = std::env::var_os(ROOT_ENV) {
            let root = PathBuf::from(root);
            let worker_name = std::env::var(WORKER_ENV).unwrap();
            let manager = AgentManager::new();
            let (instance, list) = register(&manager, &root, "shared").await;
            std::fs::write(root.join(format!("ready-{worker_name}")), b"ready").unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !root.join("start").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let result = list
                .update(
                    TaskId::try_from("1".to_owned()).unwrap(),
                    TaskPatch {
                        owner: owner(&instance),
                        ..Default::default()
                    },
                )
                .await;
            let outcome = match result {
                Ok(task) => {
                    assert_eq!(task.task.status, TaskStatus::Pending);
                    "won"
                }
                Err(TaskListError::Claim(_)) => "lost",
                Err(error) => panic!("unexpected claim failure: {error}"),
            };
            std::fs::write(root.join(format!("outcome-{worker_name}")), outcome).unwrap();
            manager.shutdown().await.unwrap();
            return;
        }
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, list) = register(&manager, &root.0, "shared").await;
        let task = list.create(input("Competing claims")).await.unwrap();
        let mut children = Vec::new();
        for worker in ["one", "two"] {
            children.push(
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "harness::task::list::tests::cross_process_claims",
                        "--nocapture",
                    ])
                    .env(ROOT_ENV, &root.0)
                    .env(WORKER_ENV, worker)
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !["one", "two"]
                .iter()
                .all(|worker| root.0.join(format!("ready-{worker}")).exists())
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        std::fs::write(root.0.join("start"), b"start").unwrap();
        for child in &mut children {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success()
            );
        }
        let outcomes: Vec<_> = ["one", "two"]
            .iter()
            .map(|worker| {
                std::fs::read_to_string(root.0.join(format!("outcome-{worker}"))).unwrap()
            })
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.as_str() == "won")
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.as_str() == "lost")
                .count(),
            1
        );
        assert!(
            list.get(task.id)
                .await
                .unwrap()
                .unwrap()
                .task
                .owner
                .is_some()
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn claim_checks_candidate_edges() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (instance, list) = register(&manager, &root.0, "shared").await;
        let prerequisite = list.create(input("Prerequisite")).await.unwrap();
        let task = list.create(input("Claim work")).await.unwrap();
        let patch = TaskPatch {
            owner: owner(&instance),
            add_blocked_by: vec![prerequisite.id.clone()],
            ..Default::default()
        };
        assert!(matches!(
            list.update(task.id.clone(), patch.clone()).await,
            Err(TaskListError::Claim(_))
        ));
        assert_eq!(list.get(task.id.clone()).await.unwrap().unwrap().task, task);
        list.update(
            prerequisite.id,
            TaskPatch {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            list.update(task.id, patch)
                .await
                .unwrap()
                .task
                .owner
                .is_some()
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejected_writes_preserve_files() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (instance, list) = register(&manager, &root.0, "shared").await;
        let mut invalid = input("Invalid");
        invalid.subject.clear();
        assert!(matches!(
            list.create(invalid).await,
            Err(TaskListError::Validation(_))
        ));
        assert!(!root.0.join("shared/.highwatermark").exists());
        let task = list.create(input("Valid")).await.unwrap();
        let path = root.0.join("shared/1.json");
        let original = std::fs::read(&path).unwrap();
        for patch in [
            TaskPatch {
                subject: Some(String::new()),
                ..Default::default()
            },
            TaskPatch {
                description: Some("a".repeat(MAX_PATCH_BYTES + 1)),
                ..Default::default()
            },
        ] {
            assert!(list.update(task.id.clone(), patch).await.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        let result = list
            .run(move |store, mut records, access| {
                let previous = records.clone();
                records.get_mut(&task.id).unwrap().subject = "Revoked write".to_owned();
                instance.close_registration();
                persist(store, access, &previous, &records, None)
            })
            .await;
        assert!(matches!(result, Err(TaskListError::Access(_))));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn partial_delete_failure() {
        let root = TestRoot::new();
        let manager = AgentManager::new();
        let (_, list) = register(&manager, &root.0, "shared").await;
        let first = list.create(input("First")).await.unwrap();
        let second = list.create(input("Second")).await.unwrap();
        let prerequisite = list.create(input("Prerequisite")).await.unwrap();
        list.update(
            prerequisite.id.clone(),
            TaskPatch {
                add_blocks: vec![first.id.clone(), second.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let directory = root.0.join("shared");
        let broken = directory.join("2.json");
        let original = std::fs::read(&broken).unwrap();
        let deleted = prerequisite.id.clone();
        let result = list
            .run(move |store, mut records, access| {
                let previous = records.clone();
                records.remove(&deleted);
                for record in records.values_mut() {
                    record.blocked_by.clear();
                }
                std::fs::remove_file(&broken).unwrap();
                std::fs::create_dir(&broken).unwrap();
                persist(store, access, &previous, &records, Some(&deleted))
            })
            .await;
        assert!(
            matches!(result, Err(TaskListError::PartialMutation { written, .. }) if written == [first.id.clone()])
        );
        assert!(directory.join("3.json").is_file());
        let saved: Task =
            serde_json::from_slice(&std::fs::read(directory.join("1.json")).unwrap()).unwrap();
        assert!(saved.blocked_by.is_empty());
        assert!(list.list(None, 100).await.is_err());
        std::fs::remove_dir(directory.join("2.json")).unwrap();
        std::fs::write(directory.join("2.json"), original).unwrap();
        assert_eq!(list.list(None, 100).await.unwrap().tasks.len(), 3);
        manager.shutdown().await.unwrap();
    }
}
