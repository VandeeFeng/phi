use super::AgentTeamId;
use super::{AgentAlias, LEAD_ALIAS};
use crate::harness::instance::AgentInstanceId;
use crate::harness::manager::message::{AgentMessageEnvelope, AgentMessageId};
use crate::tasks::TaskListBinding;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamCapability {
    #[default]
    Disabled,
    Enabled,
}

pub(crate) struct LiveTeam {
    pub(crate) config: AgentTeam,
    pub(crate) lead_epoch: uuid::Uuid,
    pub(crate) capability: TeamCapability,
    pub(super) registrations: HashMap<AgentAlias, AliasRegistration>,
    pub(crate) reservations: HashMap<AgentAlias, AliasReservation>,
    pub(super) files: Arc<TeamFiles>,
    pub(super) ready: bool,
}

pub(super) struct AliasRegistration {
    pub(super) epoch: uuid::Uuid,
    pub(super) publication: AliasPublication,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AliasPublication {
    Starting,
    Ready,
}

/// Explicit SDK storage intent, independent of application Session storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamStorageRoot {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("Team storage root must be absolute and contain no parent components")]
pub struct TeamStorageRootError;

impl TeamStorageRoot {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, TeamStorageRootError> {
        let root = root.into();
        if !root.is_absolute() || root.components().any(|part| part == Component::ParentDir) {
            return Err(TeamStorageRootError);
        }
        Ok(Self { root })
    }
}

pub(super) struct TeamFiles {
    root: TeamStorageRoot,
    operations: Arc<tokio::sync::Mutex<()>>,
    created: AtomicBool,
    recorded: std::sync::Mutex<HashMap<AgentAlias, Vec<super::message::AgentMessageUpdates>>>,
}

impl TeamFiles {
    pub(super) fn reserve(root: TeamStorageRoot) -> (Arc<Self>, tokio::sync::OwnedMutexGuard<()>) {
        let files = Arc::new(Self {
            root,
            operations: Arc::new(tokio::sync::Mutex::new(())),
            created: AtomicBool::new(false),
            recorded: Default::default(),
        });
        let lane = files
            .operations
            .clone()
            .try_lock_owned()
            .expect("new Team lane");
        (files, lane)
    }

    pub(super) async fn lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.operations.clone().lock_owned().await
    }

    async fn run<F>(self: &Arc<Self>, operation: F) -> Result<(), super::AgentManagerError>
    where
        F: FnOnce(Arc<Self>, &TeamStore) -> Result<(), super::AgentManagerError> + Send + 'static,
    {
        let files = self.clone();
        tokio::task::spawn_blocking(move || {
            let store = TeamStore::lock(&files.root.root).map_err(storage_error)?;
            operation(files, &store)
        })
        .await
        .map_err(|error| super::AgentManagerError::TeamStorage {
            message: error.to_string(),
        })?
    }

    async fn initialize(
        self: &Arc<Self>,
        manager: super::AgentManager,
        team: AgentTeam,
    ) -> Result<(), super::AgentManagerError> {
        self.run(move |files, store| {
            {
                let registry = manager.lock()?;
                let state = registry.team(team.id())?;
                let lead = registry.member(team.lead())?;
                if registry.shutting_down
                    || lead.closing
                    || lead.epoch != state.lead_epoch
                    || state.ready
                    || lead.instance.task_list() != Some(team.task_list())
                {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
            }
            let result = store.initialize(&team);
            if result.is_ok() || matches!(&result, Err(TeamStoreError::Compensation { .. })) {
                files.created.store(true, Ordering::Release);
            }
            result.map_err(storage_error)
        })
        .await
    }

    pub(super) async fn add_member(
        self: &Arc<Self>,
        manager: super::AgentManager,
        creation: super::preparation::AgentCreation,
        epoch: uuid::Uuid,
    ) -> Result<(), super::AgentManagerError> {
        let lane = self.lock().await;
        self.run(move |files, store| {
            let _lane = lane;
            let (team, member) = {
                let registry = manager.lock_live()?;
                registry.validate_team_start(&creation, epoch)?;
                let live = registry.member(creation.instance_id())?;
                let state = registry.team(live.team.as_ref().expect("teammate scope"))?;
                if !Arc::ptr_eq(&state.files, &files) {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
                let member = state
                    .config
                    .members()
                    .iter()
                    .find(|member| member.instance_id() == creation.instance_id())
                    .expect("registered teammate")
                    .clone();
                (state.config.clone(), member)
            };
            store.add_member(&team, &member).map_err(storage_error)
        })
        .await
    }

    pub(super) async fn record_message(
        self: &Arc<Self>,
        manager: super::AgentManager,
        target: super::message::MessageTarget,
        route: Arc<super::message::MessageRoute>,
        lane: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), super::AgentManagerError> {
        self.run(move |files, store| {
            let _lane = lane;
            let (team, alias) = {
                let registry = manager.lock_live()?;
                target.validate(&registry)?;
                let scope = target
                    .team
                    .as_ref()
                    .ok_or(super::AgentManagerError::AdmissionInterrupted)?;
                if !Arc::ptr_eq(&files, &scope.files) {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
                (
                    registry.team(&scope.id)?.config.clone(),
                    scope.alias.clone(),
                )
            };
            let mut recorded =
                files
                    .recorded
                    .lock()
                    .map_err(|error| super::AgentManagerError::TeamStorage {
                        message: error.to_string(),
                    })?;
            let messages = recorded.entry(alias.clone()).or_default();
            let terminal = messages
                .iter()
                .filter(|message| message.delivery.borrow().is_terminal())
                .map(|message| message.fact.message_id.clone())
                .collect::<HashSet<_>>();
            store
                .record_message(&team, &alias, &route.message, &terminal)
                .map_err(storage_error)?;
            messages.retain(|message| {
                !terminal.contains(&message.fact.message_id)
                    && message.fact.message_id != route.message.id
            });
            messages.push(route.updates());
            Ok(())
        })
        .await
    }

    pub(super) async fn remove_member(
        self: &Arc<Self>,
        manager: super::AgentManager,
        instance: AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<(), super::AgentManagerError> {
        let lane = self.lock().await;
        self.run(move |files, store| {
            let _lane = lane;
            let (team, member) = {
                let registry = manager.lock()?;
                let Some(live) = registry
                    .members
                    .get(&instance)
                    .filter(|member| member.epoch == epoch)
                else {
                    return Ok(());
                };
                if !live.closing {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
                let state = registry.team(live.team.as_ref().expect("teammate scope"))?;
                if !Arc::ptr_eq(&state.files, &files) {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
                let member = state
                    .config
                    .members()
                    .iter()
                    .find(|member| member.instance_id() == &instance)
                    .expect("registered teammate")
                    .clone();
                (state.config.clone(), member)
            };
            store.remove_member(&team, &member).map_err(storage_error)?;
            files
                .recorded
                .lock()
                .map_err(|error| super::AgentManagerError::TeamStorage {
                    message: error.to_string(),
                })?
                .remove(member.alias());
            Ok(())
        })
        .await
    }

    pub(super) async fn cleanup(
        self: &Arc<Self>,
        team: AgentTeam,
    ) -> Result<(), super::AgentManagerError> {
        if !self.created.load(Ordering::Acquire) {
            return Ok(());
        }
        self.run(move |files, store| {
            store.cleanup(&team).map_err(storage_error)?;
            files.created.store(false, Ordering::Release);
            Ok(())
        })
        .await
    }
}

fn storage_error(error: TeamStoreError) -> super::AgentManagerError {
    match error {
        TeamStoreError::NamespaceConflict { namespace } => {
            super::AgentManagerError::TeamNamespaceConflict { namespace }
        }
        error => super::AgentManagerError::TeamStorage {
            message: error.to_string(),
        },
    }
}

impl super::AgentManager {
    /// Initializes files before publishing one fixed-lead Team; creates no workers.
    pub async fn initialize_team(
        &self,
        lead: &AgentInstanceId,
        root: TeamStorageRoot,
    ) -> Result<AgentTeamId, super::AgentManagerError> {
        let (team, files, lane) = self.lock_live()?.reserve_team(lead, root)?;
        let manager = self.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // The reservation must survive caller loss until IO and compensation finish.
        tokio::spawn(async move {
            let _lane = lane;
            let mut result = files.initialize(manager.clone(), team.clone()).await;
            if result.is_ok() {
                result = (|| {
                    let mut registry = manager.lock()?;
                    let state = registry.team(team.id())?;
                    let epoch = state.lead_epoch;
                    let member = registry.member(team.lead())?;
                    if sender.is_closed()
                        || registry.shutting_down
                        || member.closing
                        || member.epoch != epoch
                        || member.instance.task_list() != Some(team.task_list())
                    {
                        return Err(super::AgentManagerError::AdmissionInterrupted);
                    }
                    let member = registry
                        .members
                        .get_mut(team.lead())
                        .expect("validated lead");
                    member.team = Some(team.id().clone());
                    member.alias = Some(AgentAlias::new(LEAD_ALIAS).expect("reserved lead alias"));
                    registry
                        .teams
                        .get_mut(team.id())
                        .expect("reserved Team")
                        .ready = true;
                    Ok(())
                })();
            }
            if let Err(cause) = result {
                result = match files.cleanup(team.clone()).await {
                    Ok(()) => match manager.lock() {
                        Ok(mut registry) => {
                            registry.teams.remove(team.id());
                            Err(cause)
                        }
                        Err(cleanup) => Err(super::AgentManagerError::Compensation {
                            cause: Box::new(cause),
                            cleanup: Box::new(cleanup),
                        }),
                    },
                    Err(cleanup) => Err(super::AgentManagerError::Compensation {
                        cause: Box::new(cause),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
            let _ = sender.send(result.map(|()| team.id().clone()));
        });
        receiver
            .await
            .map_err(|error| super::AgentManagerError::TeamStorage {
                message: error.to_string(),
            })?
    }

    pub(super) async fn cleanup_teammate(
        &self,
        instance: &AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<(), super::AgentManagerError> {
        let files = {
            let registry = self.lock()?;
            let Some(member) = registry
                .members
                .get(instance)
                .filter(|member| member.epoch == epoch)
            else {
                return Ok(());
            };
            match &member.team {
                Some(id) => {
                    let state = registry.team(id)?;
                    (state.lead_epoch != epoch).then(|| state.files.clone())
                }
                None => None,
            }
        };
        if let Some(files) = files {
            files
                .remove_member(self.clone(), instance.clone(), epoch)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn cleanup_team(
        &self,
        epoch: uuid::Uuid,
    ) -> Result<(), super::AgentManagerError> {
        let capture = {
            let registry = self.lock()?;
            registry
                .teams
                .values()
                .find(|team| team.lead_epoch == epoch)
                .map(|team| (team.config.clone(), team.files.clone()))
        };
        let Some((team, files)) = capture else {
            return Ok(());
        };
        let _lane = files.lock().await;
        {
            let registry = self.lock()?;
            if !registry.teams.contains_key(team.id()) {
                return Ok(());
            }
            let state = registry.team(team.id())?;
            if !state.reservations.is_empty()
                || state
                    .registrations
                    .values()
                    .any(|registered| registered.epoch != epoch)
            {
                return Err(super::AgentManagerError::TeamStorage {
                    message: "Team members or startup reservations still require finalization"
                        .into(),
                });
            }
        }
        files.cleanup(team.clone()).await?;
        self.lock()?.teams.remove(team.id());
        Ok(())
    }
}

pub(crate) struct AliasReservation {
    pub(crate) instance_id: AgentInstanceId,
    pub(crate) token: uuid::Uuid,
}

const TEAM_NAMESPACE_PREFIX: &str = "session-";
const TEAM_SESSION_PREFIX_LEN: usize = 8;
const MAX_SESSION_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "Team namespace requires a bounded Session ID containing only ASCII letters, digits, hyphens or underscores"
)]
pub struct TeamNamespaceError;

/// Derives a storage component, never a live Team identity or access grant.
pub fn team_namespace(session_id: &str) -> Result<String, TeamNamespaceError> {
    if session_id.is_empty()
        || session_id.len() > MAX_SESSION_ID_BYTES
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(TeamNamespaceError);
    }
    let prefix = &session_id[..session_id.len().min(TEAM_SESSION_PREFIX_LEN)];
    Ok(format!("{TEAM_NAMESPACE_PREFIX}{prefix}"))
}

/// Team configuration references; live registration and permissions remain in Registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentTeam {
    id: AgentTeamId,
    namespace: String,
    lead: AgentInstanceId,
    task_list: TaskListBinding,
    members: Vec<AgentTeamMember>,
}

/// A definition name is reusable; an alias identifies one member in this Team.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentTeamMember {
    instance_id: AgentInstanceId,
    alias: AgentAlias,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<String>,
    working_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TeamDefinitionError {
    #[error("Team lead must use the reserved alias `lead`")]
    InvalidLeadAlias,
    #[error("invalid Team namespace derived from lead Session")]
    InvalidNamespace,
    #[error("duplicate Team alias `{alias}`")]
    DuplicateAlias { alias: String },
    #[error("instance `{instance}` occurs more than once in the Team")]
    DuplicateInstance { instance: String },
    #[error("teammate `{instance}` requires an Agent definition")]
    MissingDefinition { instance: String },
    #[error("Team member `{instance}` requires an absolute working directory")]
    InvalidWorkingDir { instance: String },
}

impl AgentTeamMember {
    pub(crate) fn new(
        instance_id: AgentInstanceId,
        alias: AgentAlias,
        definition: Option<&str>,
        working_dir: PathBuf,
    ) -> Self {
        Self {
            instance_id,
            alias,
            definition: definition.map(str::to_owned),
            working_dir,
        }
    }

    pub(crate) fn instance_id(&self) -> &AgentInstanceId {
        &self.instance_id
    }

    pub(crate) fn alias(&self) -> &AgentAlias {
        &self.alias
    }
}

impl AgentTeam {
    pub(crate) fn new(
        id: AgentTeamId,
        lead: AgentTeamMember,
        task_list: TaskListBinding,
        teammates: Vec<AgentTeamMember>,
    ) -> Result<Self, TeamDefinitionError> {
        if lead.alias.as_str() != LEAD_ALIAS {
            return Err(TeamDefinitionError::InvalidLeadAlias);
        }
        let namespace = team_namespace(lead.instance_id.as_str())
            .map_err(|_| TeamDefinitionError::InvalidNamespace)?;
        let lead_id = lead.instance_id.clone();
        let members = std::iter::once(lead).chain(teammates).collect::<Vec<_>>();
        let mut aliases = HashSet::new();
        let mut instances = HashSet::new();
        for member in &members {
            if !member.working_dir.is_absolute() {
                return Err(TeamDefinitionError::InvalidWorkingDir {
                    instance: member.instance_id.to_string(),
                });
            }
            if !aliases.insert(&member.alias) {
                return Err(TeamDefinitionError::DuplicateAlias {
                    alias: member.alias.as_str().into(),
                });
            }
            if !instances.insert(&member.instance_id) {
                return Err(TeamDefinitionError::DuplicateInstance {
                    instance: member.instance_id.to_string(),
                });
            }
        }
        if let Some(member) = members
            .iter()
            .skip(1)
            .find(|member| member.definition.is_none())
        {
            return Err(TeamDefinitionError::MissingDefinition {
                instance: member.instance_id.to_string(),
            });
        }
        Ok(Self {
            id,
            namespace,
            lead: lead_id,
            task_list,
            members,
        })
    }

    pub(crate) fn id(&self) -> &AgentTeamId {
        &self.id
    }

    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn lead(&self) -> &AgentInstanceId {
        &self.lead
    }

    pub(crate) fn task_list(&self) -> &TaskListBinding {
        &self.task_list
    }

    pub(crate) fn members(&self) -> &[AgentTeamMember] {
        &self.members
    }

    pub(super) fn add_member(&mut self, member: AgentTeamMember) {
        self.members.push(member);
    }

    pub(super) fn remove_member(&mut self, instance: &AgentInstanceId) {
        self.members
            .retain(|member| member.instance_id() != instance);
    }
}

const MAX_NAMESPACE_OWNER_BYTES: u64 = (MAX_SESSION_ID_BYTES + 2) as u64;

const CONFIG_FILE: &str = "config.json";
const INBOX_DIRECTORY: &str = "inboxes";
const TEMP_PREFIX: &str = ".team-write-";
const MAX_CONFIG_BYTES: u64 = 512 * 1024;
const MAX_INBOX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INBOX_MESSAGES: usize = 128;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);

#[derive(Debug, thiserror::Error)]
pub(in crate::harness::manager) enum TeamStoreError {
    #[error("Team storage root must be absolute and contain no parent components")]
    InvalidRoot,
    #[error("Team namespace `{namespace}` is already occupied")]
    NamespaceConflict { namespace: String },
    #[error("Team storage lock contention exceeded its deadline")]
    LockTimeout,
    #[error("Team file operation failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid Team storage at {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("Team file operation failed: {cause}; compensation failed: {cleanup}")]
    Compensation {
        #[source]
        cause: Box<TeamStoreError>,
        cleanup: Box<TeamStoreError>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamConfigRecord {
    team_id: AgentTeamId,
    namespace: String,
    lead_id: AgentInstanceId,
    task_list_id: String,
    members: Vec<AgentTeamMember>,
}

impl From<&AgentTeam> for TeamConfigRecord {
    fn from(team: &AgentTeam) -> Self {
        Self {
            team_id: team.id().clone(),
            namespace: team.namespace().to_owned(),
            lead_id: team.lead().clone(),
            task_list_id: team.task_list().list_id().to_owned(),
            members: team.members().to_vec(),
        }
    }
}

impl TeamConfigRecord {
    fn validate(&self, team: &AgentTeam, path: &Path) -> Result<(), TeamStoreError> {
        if self.team_id != *team.id()
            || self.namespace != team.namespace()
            || self.lead_id != *team.lead()
            || self.task_list_id != team.task_list().list_id()
        {
            return Err(invalid(path, "Team identity or list association differs"));
        }
        let (lead, teammates) = self
            .members
            .split_first()
            .ok_or_else(|| invalid(path, "missing Team lead"))?;
        if lead.instance_id() != &self.lead_id {
            return Err(invalid(path, "lead and member identity differ"));
        }
        AgentTeam::new(
            self.team_id.clone(),
            lead.clone(),
            team.task_list().clone(),
            teammates.to_vec(),
        )
        .map_err(|error| invalid(path, error.to_string()))?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamInboxRecord {
    recipient: AgentInstanceId,
    messages: Vec<AgentMessageEnvelope>,
}

impl TeamInboxRecord {
    fn validate(&self, recipient: &AgentInstanceId, path: &Path) -> Result<(), TeamStoreError> {
        if &self.recipient != recipient || self.messages.len() > MAX_INBOX_MESSAGES {
            return Err(invalid(path, "inbox recipient or message count differs"));
        }
        let mut identities = HashSet::new();
        for message in &self.messages {
            if &message.recipient != recipient || !identities.insert(&message.id) {
                return Err(invalid(
                    path,
                    "invalid inbox message recipient or duplicate ID",
                ));
            }
        }
        Ok(())
    }
}

/// Blocking file mechanism; the caller supplies live authorization outside this lock.
pub(in crate::harness::manager) struct TeamStore {
    root: PathBuf,
    _lock: File,
}

impl TeamStore {
    pub(in crate::harness::manager) fn lock(root: &Path) -> Result<Self, TeamStoreError> {
        if !root.is_absolute()
            || root
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(TeamStoreError::InvalidRoot);
        }
        create_directory(root)?;
        let path = root.join(".lock");
        let lock = open_file(
            &path,
            OpenOptions::new().read(true).write(true).create(true),
        )?;
        let started = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= LOCK_TIMEOUT {
                        return Err(TeamStoreError::LockTimeout);
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(io_error(&path, error)),
            }
        }
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    pub(in crate::harness::manager) fn initialize(
        &self,
        team: &AgentTeam,
    ) -> Result<(), TeamStoreError> {
        let directory = self.root.join(team.namespace());
        let config_path = directory.join(CONFIG_FILE);
        let record = TeamConfigRecord::from(team);
        record.validate(team, &config_path)?;
        let bytes = encode(&record, &config_path, MAX_CONFIG_BYTES)?;
        match fs::symlink_metadata(&directory) {
            Ok(_) => {
                return Err(TeamStoreError::NamespaceConflict {
                    namespace: team.namespace().to_owned(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&directory, error)),
        }
        // Runtime cleanup must not erase the full identity behind a shortened namespace.
        let owner_path = self
            .root
            .join(format!(".namespace-{}.json", team.namespace()));
        match read_record::<AgentInstanceId>(&owner_path, MAX_NAMESPACE_OWNER_BYTES) {
            Ok(owner) if &owner == team.lead() => {}
            Ok(_) => {
                return Err(TeamStoreError::NamespaceConflict {
                    namespace: team.namespace().to_owned(),
                });
            }
            Err(error) if is_missing(&error) => {
                replace(
                    &owner_path,
                    &encode(team.lead(), &owner_path, MAX_NAMESPACE_OWNER_BYTES)?,
                )?;
            }
            Err(error) => return Err(error),
        }
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(TeamStoreError::NamespaceConflict {
                    namespace: team.namespace().to_owned(),
                });
            }
            Err(error) => return Err(io_error(&directory, error)),
        }
        let result = (|| {
            replace(&config_path, &bytes)?;
            let inboxes = directory.join(INBOX_DIRECTORY);
            fs::create_dir(&inboxes).map_err(|error| io_error(&inboxes, error))?;
            for member in &record.members {
                let path = inboxes.join(inbox_name(member.alias()));
                let inbox = TeamInboxRecord {
                    recipient: member.instance_id().clone(),
                    messages: Vec::new(),
                };
                replace(&path, &encode(&inbox, &path, MAX_INBOX_BYTES)?)?;
            }
            Ok(())
        })();
        if let Err(cause) = result {
            return match self.remove_files(&directory, &record) {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(TeamStoreError::Compensation {
                    cause: Box::new(cause),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        Ok(())
    }

    fn add_member(&self, team: &AgentTeam, member: &AgentTeamMember) -> Result<(), TeamStoreError> {
        let directory = self.root.join(team.namespace());
        require_directory(&directory)?;
        let config = directory.join(CONFIG_FILE);
        let mut record: TeamConfigRecord = read_record(&config, MAX_CONFIG_BYTES)?;
        record.validate(team, &config)?;
        let inboxes = directory.join(INBOX_DIRECTORY);
        require_directory(&inboxes)?;
        let path = inboxes.join(inbox_name(member.alias()));
        if let Some(existing) = record.members.iter().find(|existing| {
            existing.alias() == member.alias() || existing.instance_id() == member.instance_id()
        }) {
            if existing != member {
                return Err(invalid(&config, "member registration differs"));
            }
        } else {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(&path, error)),
                Ok(_) => return Err(invalid(&path, "unregistered inbox already exists")),
            }
            record.members.push(member.clone());
            record.validate(team, &config)?;
            replace(&config, &encode(&record, &config, MAX_CONFIG_BYTES)?)?;
        }
        match read_record::<TeamInboxRecord>(&path, MAX_INBOX_BYTES) {
            Ok(inbox) => inbox.validate(member.instance_id(), &path),
            Err(error) if is_missing(&error) => {
                let inbox = TeamInboxRecord {
                    recipient: member.instance_id().clone(),
                    messages: Vec::new(),
                };
                replace(&path, &encode(&inbox, &path, MAX_INBOX_BYTES)?)
            }
            Err(error) => Err(error),
        }
    }

    fn record_message(
        &self,
        team: &AgentTeam,
        alias: &AgentAlias,
        message: &AgentMessageEnvelope,
        terminal: &HashSet<AgentMessageId>,
    ) -> Result<(), TeamStoreError> {
        let directory = self.root.join(team.namespace());
        require_directory(&directory)?;
        let config = directory.join(CONFIG_FILE);
        let record: TeamConfigRecord = read_record(&config, MAX_CONFIG_BYTES)?;
        record.validate(team, &config)?;
        if !record
            .members
            .iter()
            .any(|member| member.alias() == alias && member.instance_id() == &message.recipient)
        {
            return Err(invalid(&config, "message recipient registration differs"));
        }
        let inboxes = directory.join(INBOX_DIRECTORY);
        require_directory(&inboxes)?;
        let path = inboxes.join(inbox_name(alias));
        let mut inbox: TeamInboxRecord = read_record(&path, MAX_INBOX_BYTES)?;
        inbox.validate(&message.recipient, &path)?;
        let existing = inbox
            .messages
            .iter()
            .find(|existing| existing.id == message.id);
        if existing.is_some_and(|existing| existing != message) {
            return Err(invalid(&path, "message identity has different content"));
        }
        let recorded = existing.is_some();
        inbox
            .messages
            .retain(|existing| existing.id == message.id || !terminal.contains(&existing.id));
        if !recorded {
            inbox.messages.push(message.clone());
        }
        inbox.validate(&message.recipient, &path)?;
        replace(&path, &encode(&inbox, &path, MAX_INBOX_BYTES)?)
    }

    fn remove_member(
        &self,
        team: &AgentTeam,
        member: &AgentTeamMember,
    ) -> Result<(), TeamStoreError> {
        let directory = self.root.join(team.namespace());
        require_directory(&directory)?;
        let config = directory.join(CONFIG_FILE);
        let mut record: TeamConfigRecord = read_record(&config, MAX_CONFIG_BYTES)?;
        record.validate(team, &config)?;
        let inboxes = directory.join(INBOX_DIRECTORY);
        require_directory(&inboxes)?;
        let path = inboxes.join(inbox_name(member.alias()));
        let registered = record.members.iter().find(|existing| {
            existing.alias() == member.alias() || existing.instance_id() == member.instance_id()
        });
        if registered.is_some_and(|existing| existing != member)
            || member.instance_id() == team.lead()
        {
            return Err(invalid(&config, "member registration differs"));
        }
        match read_record::<TeamInboxRecord>(&path, MAX_INBOX_BYTES) {
            Ok(inbox) => {
                if registered.is_none() {
                    return Err(invalid(&path, "unregistered inbox already exists"));
                }
                inbox.validate(member.instance_id(), &path)?;
                fs::remove_file(&path).map_err(|error| io_error(&path, error))?;
            }
            Err(error) if is_missing(&error) => {}
            Err(error) => return Err(error),
        }
        if registered.is_some() {
            record
                .members
                .retain(|existing| existing.instance_id() != member.instance_id());
            replace(&config, &encode(&record, &config, MAX_CONFIG_BYTES)?)?;
        }
        Ok(())
    }

    pub(in crate::harness::manager) fn cleanup(
        &self,
        team: &AgentTeam,
    ) -> Result<(), TeamStoreError> {
        let directory = self.root.join(team.namespace());
        match require_directory(&directory) {
            Ok(()) => {}
            Err(error) if is_missing(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
        let path = directory.join(CONFIG_FILE);
        let record: TeamConfigRecord = read_record(&path, MAX_CONFIG_BYTES)?;
        record.validate(team, &path)?;
        self.remove_files(&directory, &record)
    }

    fn remove_files(
        &self,
        directory: &Path,
        record: &TeamConfigRecord,
    ) -> Result<(), TeamStoreError> {
        let inboxes = directory.join(INBOX_DIRECTORY);
        let recipients: HashMap<_, _> = record
            .members
            .iter()
            .map(|member| (inbox_name(member.alias()), member.instance_id()))
            .collect();
        let mut files = Vec::new();
        let mut has_inboxes = false;
        for (index, entry) in fs::read_dir(directory)
            .map_err(|error| io_error(directory, error))?
            .enumerate()
        {
            if index >= 2 {
                return Err(invalid(directory, "unexpected Team directory entry count"));
            }
            let entry = entry.map_err(|error| io_error(directory, error))?;
            let path = entry.path();
            if entry.file_name() == CONFIG_FILE {
                let config: TeamConfigRecord = read_record(&path, MAX_CONFIG_BYTES)?;
                if config.team_id != record.team_id
                    || config.lead_id != record.lead_id
                    || config.namespace != record.namespace
                    || config.task_list_id != record.task_list_id
                    || config.members != record.members
                {
                    return Err(invalid(&path, "Team configuration changed before cleanup"));
                }
            } else if entry.file_name() == INBOX_DIRECTORY {
                require_directory(&path)?;
                has_inboxes = true;
                for (index, inbox) in fs::read_dir(&path)
                    .map_err(|error| io_error(&path, error))?
                    .enumerate()
                {
                    if index >= recipients.len() {
                        return Err(invalid(&path, "unexpected inbox entry count"));
                    }
                    let inbox = inbox.map_err(|error| io_error(&path, error))?;
                    let inbox_path = inbox.path();
                    let name = inbox.file_name();
                    let recipient = name
                        .to_str()
                        .and_then(|name| recipients.get(name))
                        .ok_or_else(|| invalid(&inbox_path, "unexpected inbox entry"))?;
                    let inbox: TeamInboxRecord = read_record(&inbox_path, MAX_INBOX_BYTES)?;
                    inbox.validate(recipient, &inbox_path)?;
                    files.push(inbox_path);
                }
            } else {
                return Err(invalid(&path, "unexpected Team directory entry"));
            }
        }
        // Validate every entry before deleting anything; never follow foreign files or links.
        for path in files {
            fs::remove_file(&path).map_err(|error| io_error(&path, error))?;
        }
        if has_inboxes {
            fs::remove_dir(&inboxes).map_err(|error| io_error(&inboxes, error))?;
        }
        let config = directory.join(CONFIG_FILE);
        match fs::remove_file(&config) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&config, error)),
        }
        if let Err(error) = fs::remove_dir(directory) {
            let cause = io_error(directory, error);
            let restored = encode(record, &config, MAX_CONFIG_BYTES)
                .and_then(|bytes| replace(&config, &bytes));
            return match restored {
                Ok(()) => Err(cause),
                Err(cleanup) => Err(TeamStoreError::Compensation {
                    cause: Box::new(cause),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        Ok(())
    }
}

fn inbox_name(alias: &AgentAlias) -> String {
    format!("{}.json", alias.as_str())
}

fn encode(record: &impl Serialize, path: &Path, limit: u64) -> Result<Vec<u8>, TeamStoreError> {
    let bytes = serde_json::to_vec(record).map_err(|error| invalid(path, error.to_string()))?;
    if bytes.len() as u64 > limit {
        return Err(invalid(path, "record exceeds its byte limit"));
    }
    Ok(bytes)
}

fn read_record<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<T, TeamStoreError> {
    let file = open_file(path, OpenOptions::new().read(true))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() as u64 > limit {
        return Err(invalid(path, "record exceeds its byte limit"));
    }
    serde_json::from_slice(&bytes).map_err(|error| invalid(path, error.to_string()))
}

fn replace(path: &Path, bytes: &[u8]) -> Result<(), TeamStoreError> {
    match require_file(path) {
        Ok(()) => {}
        Err(error) if is_missing(&error) => {}
        Err(error) => return Err(error),
    }
    let temporary = path
        .parent()
        .expect("Team record has a parent")
        .join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = open_file(&temporary, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error(&temporary, error))?;
        fs::rename(&temporary, path).map_err(|error| io_error(path, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_directory(path: &Path) -> Result<(), TeamStoreError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(&current, error)),
        }
        require_directory(&current)?;
    }
    Ok(())
}

fn require_directory(path: &Path) -> Result<(), TeamStoreError> {
    let kind = fs::symlink_metadata(path)
        .map_err(|error| io_error(path, error))?
        .file_type();
    if !kind.is_dir() {
        return Err(invalid(path, "expected a directory, not a symlink"));
    }
    Ok(())
}

fn require_file(path: &Path) -> Result<(), TeamStoreError> {
    let kind = fs::symlink_metadata(path)
        .map_err(|error| io_error(path, error))?
        .file_type();
    if !kind.is_file() {
        return Err(invalid(path, "expected a regular file, not a symlink"));
    }
    Ok(())
}

fn open_file(path: &Path, options: &mut OpenOptions) -> Result<File, TeamStoreError> {
    match require_file(path) {
        Ok(()) => {}
        Err(error) if is_missing(&error) => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| io_error(path, error))?;
    if !file
        .metadata()
        .map_err(|error| io_error(path, error))?
        .is_file()
    {
        return Err(invalid(path, "expected a regular file"));
    }
    Ok(file)
}

fn is_missing(error: &TeamStoreError) -> bool {
    matches!(error, TeamStoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
}

fn invalid(path: &Path, reason: impl Into<String>) -> TeamStoreError {
    TeamStoreError::Invalid {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn io_error(path: &Path, source: std::io::Error) -> TeamStoreError {
    TeamStoreError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(instance: &str, alias: &str, definition: Option<&str>) -> AgentTeamMember {
        AgentTeamMember::new(
            AgentInstanceId::new(instance),
            AgentAlias::new(alias).unwrap(),
            definition,
            PathBuf::from("/tmp/phi-team-model"),
        )
    }

    fn team(teammates: Vec<AgentTeamMember>) -> Result<AgentTeam, TeamDefinitionError> {
        AgentTeam::new(
            serde_json::from_value(serde_json::json!("team-runtime-id")).unwrap(),
            member("12345678-root-session", LEAD_ALIAS, None),
            TaskListBinding::new("/tmp/phi-team-model", "explicit-shared-list").unwrap(),
            teammates,
        )
    }

    #[test]
    fn team_initialization_has_one_fixed_lead_and_no_workers() {
        let team = team(Vec::new()).unwrap();
        assert_eq!(team.id().as_str(), "team-runtime-id");
        assert_eq!(team.namespace(), "session-12345678");
        assert_eq!(team.task_list().list_id(), "explicit-shared-list");
        assert_eq!(team.members().len(), 1);
        assert_eq!(team.members()[0].instance_id(), team.lead());
        assert_eq!(team.members()[0].alias().as_str(), LEAD_ALIAS);
        let record = serde_json::to_value(&team.members()[0]).unwrap();
        assert!(record.get("definition").is_none());
        assert_eq!(record["alias"], "lead");
        assert!(record.get("role").is_none());
        assert!(record.get("registration").is_none());
    }

    #[test]
    fn duplicate_alias_or_instance_rejects_the_whole_candidate() {
        for alias in [LEAD_ALIAS, "reviewer"] {
            let members = if alias == LEAD_ALIAS {
                vec![member("another-instance", alias, None)]
            } else {
                vec![member("first", alias, None), member("second", alias, None)]
            };
            assert!(matches!(
                team(members),
                Err(TeamDefinitionError::DuplicateAlias { .. })
            ));
        }
        assert!(matches!(
            team(vec![
                member("same", "security", None),
                member("same", "performance", None)
            ]),
            Err(TeamDefinitionError::DuplicateInstance { .. })
        ));
    }

    #[test]
    fn only_the_lead_can_be_unspecialized() {
        assert!(matches!(
            team(vec![member("worker", "security", None)]),
            Err(TeamDefinitionError::MissingDefinition { .. })
        ));
    }

    #[test]
    fn namespace_validates_the_whole_session_identity() {
        for length in 1..=MAX_SESSION_ID_BYTES {
            let identity = "a".repeat(length);
            assert_eq!(
                team_namespace(&identity).unwrap(),
                format!(
                    "session-{}",
                    "a".repeat(length.min(TEAM_SESSION_PREFIX_LEN))
                )
            );
        }
        for identity in [
            "",
            "../other",
            "12345678/other",
            "12345678\\other",
            "12345678\n",
            "12345678界",
        ] {
            assert!(team_namespace(identity).is_err());
        }
        assert!(team_namespace(&"a".repeat(MAX_SESSION_ID_BYTES + 1)).is_err());
        assert_eq!(
            team_namespace("12345678-first"),
            team_namespace("12345678-second")
        );
        let mut lead = member("root", LEAD_ALIAS, None);
        lead.working_dir = "relative".into();
        assert!(matches!(
            AgentTeam::new(
                AgentTeamId("team".into()),
                lead,
                TaskListBinding::new("/tmp/phi-team-model", "shared").unwrap(),
                Vec::new(),
            ),
            Err(TeamDefinitionError::InvalidWorkingDir { .. })
        ));
    }

    #[test]
    fn lead_and_namespace_are_validated_before_publication() {
        for (instance, alias, expected) in [
            ("root", "reviewer", TeamDefinitionError::InvalidLeadAlias),
            ("", LEAD_ALIAS, TeamDefinitionError::InvalidNamespace),
            (
                "../other",
                LEAD_ALIAS,
                TeamDefinitionError::InvalidNamespace,
            ),
        ] {
            let result = AgentTeam::new(
                serde_json::from_value(serde_json::json!("team")).unwrap(),
                member(instance, alias, None),
                TaskListBinding::new("/tmp/phi-team-model", "list").unwrap(),
                Vec::new(),
            );
            assert_eq!(result, Err(expected));
        }
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = Path::new("/tmp").join(format!("phi-team-store-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn stored_team(root: &Path, lead: &str) -> AgentTeam {
        let member = |instance: &str, alias: &str, definition| {
            AgentTeamMember::new(
                AgentInstanceId::new(instance),
                AgentAlias::new(alias).unwrap(),
                definition,
                root.to_owned(),
            )
        };
        AgentTeam::new(
            AgentTeamId(uuid::Uuid::new_v4().to_string()),
            member(lead, LEAD_ALIAS, None),
            TaskListBinding::new(root.join("tasks"), "explicit-shared-list").unwrap(),
            vec![
                member("security-instance", "security", Some("reviewer")),
                member("performance-instance", "performance", Some("reviewer")),
            ],
        )
        .unwrap()
    }

    #[test]
    fn records_and_cleanup_are_separate_from_tasks() {
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let store_root = root.0.join("teams");
        let store = TeamStore::lock(&store_root).unwrap();
        fs::create_dir(root.0.join("tasks")).unwrap();
        fs::write(root.0.join("tasks/keep"), b"task facts").unwrap();
        fs::create_dir(store_root.join("unrelated")).unwrap();
        fs::write(store_root.join("unrelated/keep"), b"foreign Team").unwrap();
        store.initialize(&team).unwrap();
        let directory = store_root.join(team.namespace());
        let record: serde_json::Value =
            read_record(&directory.join(CONFIG_FILE), MAX_CONFIG_BYTES).unwrap();
        assert_eq!(record["team_id"], team.id().as_str());
        assert_eq!(record["lead_id"], team.lead().as_str());
        assert_eq!(record["task_list_id"], "explicit-shared-list");
        assert_eq!(record["members"].as_array().unwrap().len(), 3);
        for (member, saved) in team
            .members()
            .iter()
            .zip(record["members"].as_array().unwrap())
        {
            assert_eq!(saved["instance_id"], member.instance_id().as_str());
            assert_eq!(saved["working_dir"], member.working_dir.to_str().unwrap());
            assert!(saved.get("epoch").is_none());
            let inbox: TeamInboxRecord = read_record(
                &directory
                    .join(INBOX_DIRECTORY)
                    .join(inbox_name(member.alias())),
                MAX_INBOX_BYTES,
            )
            .unwrap();
            inbox.validate(member.instance_id(), &directory).unwrap();
            assert!(inbox.messages.is_empty());
        }
        assert!(record.get("task_list").is_none());
        assert!(record.get("capability").is_none());
        store.cleanup(&team).unwrap();
        store.cleanup(&team).unwrap();
        assert!(!directory.exists());
        assert_eq!(fs::read(root.0.join("tasks/keep")).unwrap(), b"task facts");
        assert_eq!(
            fs::read(store_root.join("unrelated/keep")).unwrap(),
            b"foreign Team"
        );
        assert!(store_root.join(".lock").is_file());
        let owner_path = store_root.join(format!(".namespace-{}.json", team.namespace()));
        let owner = fs::read(&owner_path).unwrap();
        for invalid in [b"null".as_slice(), b"broken JSON", br#""different-lead""#] {
            fs::write(&owner_path, invalid).unwrap();
            assert!(store.initialize(&team).is_err());
            assert!(!directory.exists());
            assert_eq!(fs::read(&owner_path).unwrap(), invalid);
        }
        #[cfg(unix)]
        {
            fs::remove_file(&owner_path).unwrap();
            std::os::unix::fs::symlink(root.0.join("tasks/keep"), &owner_path).unwrap();
            assert!(store.initialize(&team).is_err());
            assert_eq!(fs::read(root.0.join("tasks/keep")).unwrap(), b"task facts");
            fs::remove_file(&owner_path).unwrap();
        }
        fs::write(&owner_path, owner).unwrap();
        store.initialize(&team).unwrap();
        store.cleanup(&team).unwrap();
        let other = stored_team(&root.0, "12345678-other");
        assert!(matches!(
            store.initialize(&other),
            Err(TeamStoreError::NamespaceConflict { .. })
        ));
        assert!(!directory.exists());
    }

    #[test]
    fn message_records_preserve_identity_bounds_and_unknown_pending_messages() {
        use crate::harness::manager::message::{AgentMessageId, AgentMessageKind};
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let store = TeamStore::lock(&root.0.join("teams")).unwrap();
        store.initialize(&team).unwrap();
        let member = &team.members()[1];
        let path = root
            .0
            .join("teams")
            .join(team.namespace())
            .join(INBOX_DIRECTORY)
            .join(inbox_name(member.alias()));
        let mut message = AgentMessageEnvelope {
            id: AgentMessageId("first".into()),
            sender: team.lead().clone(),
            recipient: member.instance_id().clone(),
            kind: AgentMessageKind::Ordinary,
            content: "review findings".into(),
        };
        store
            .record_message(&team, member.alias(), &message, &HashSet::new())
            .unwrap();
        let original = fs::read(&path).unwrap();
        store
            .record_message(&team, member.alias(), &message, &HashSet::new())
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
        message.content = "conflicting retry".into();
        assert!(
            store
                .record_message(&team, member.alias(), &message, &HashSet::new())
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        message.id = AgentMessageId("wrong-recipient".into());
        message.recipient = AgentInstanceId::new("successor");
        assert!(
            store
                .record_message(&team, member.alias(), &message, &HashSet::new())
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        message.recipient = member.instance_id().clone();
        for index in 1..MAX_INBOX_MESSAGES {
            message.id = AgentMessageId(format!("message-{index}"));
            store
                .record_message(&team, member.alias(), &message, &HashSet::new())
                .unwrap();
        }
        let full = fs::read(&path).unwrap();
        message.id = AgentMessageId("overflow".into());
        assert!(
            store
                .record_message(&team, member.alias(), &message, &HashSet::new())
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), full);
        let terminal = HashSet::from([
            AgentMessageId("first".into()),
            AgentMessageId("message-1".into()),
        ]);
        store
            .record_message(&team, member.alias(), &message, &terminal)
            .unwrap();
        let inbox: TeamInboxRecord = read_record(&path, MAX_INBOX_BYTES).unwrap();
        assert_eq!(inbox.messages.len(), MAX_INBOX_MESSAGES - 1);
        assert!(
            inbox
                .messages
                .iter()
                .all(|message| !terminal.contains(&message.id))
        );
        assert_eq!(inbox.messages.last().unwrap(), &message);
        let retained = fs::read(&path).unwrap();
        message.id = AgentMessageId("oversized".into());
        message.content = "x".repeat(MAX_INBOX_BYTES as usize);
        assert!(
            store
                .record_message(&team, member.alias(), &message, &HashSet::new())
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), retained);
        store.cleanup(&team).unwrap();
    }

    #[test]
    fn member_effects_are_captured_and_retry_partial_publication() {
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let lead_only = AgentTeam::new(
            team.id().clone(),
            team.members()[0].clone(),
            team.task_list().clone(),
            Vec::new(),
        )
        .unwrap();
        let store = TeamStore::lock(&root.0.join("teams")).unwrap();
        store.initialize(&lead_only).unwrap();
        let directory = root.0.join("teams").join(team.namespace());
        let config = directory.join(CONFIG_FILE);
        let member = &team.members()[1];
        let path = directory
            .join(INBOX_DIRECTORY)
            .join(inbox_name(member.alias()));
        fs::write(&path, b"foreign").unwrap();
        assert!(store.add_member(&team, member).is_err());
        assert!(store.remove_member(&team, member).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(
            read_record::<TeamConfigRecord>(&config, MAX_CONFIG_BYTES)
                .unwrap()
                .members
                .len(),
            1
        );
        fs::remove_file(&path).unwrap();
        let mut record = TeamConfigRecord::from(&lead_only);
        record.members.push(member.clone());
        replace(
            &config,
            &encode(&record, &config, MAX_CONFIG_BYTES).unwrap(),
        )
        .unwrap();
        store.add_member(&team, member).unwrap();
        store.add_member(&team, member).unwrap();
        let mut inbox: TeamInboxRecord = read_record(&path, MAX_INBOX_BYTES).unwrap();
        inbox.recipient = AgentInstanceId::new("successor");
        replace(&path, &encode(&inbox, &path, MAX_INBOX_BYTES).unwrap()).unwrap();
        assert!(store.remove_member(&team, member).is_err());
        assert!(store.add_member(&team, member).is_err());
        assert_eq!(
            read_record::<TeamInboxRecord>(&path, MAX_INBOX_BYTES)
                .unwrap()
                .recipient
                .as_str(),
            "successor"
        );
        fs::remove_file(&path).unwrap();
        store.remove_member(&team, member).unwrap();
        store.remove_member(&team, member).unwrap();
        assert_eq!(
            read_record::<TeamConfigRecord>(&config, MAX_CONFIG_BYTES)
                .unwrap()
                .members
                .len(),
            1
        );
        assert!(store.remove_member(&team, &team.members()[0]).is_err());
        for member in &team.members()[1..] {
            store.add_member(&team, member).unwrap();
        }
        assert_eq!(
            read_record::<TeamConfigRecord>(&config, MAX_CONFIG_BYTES)
                .unwrap()
                .members,
            team.members()
        );
        let stale = &team.members()[1];
        store.remove_member(&team, stale).unwrap();
        let mut successor = stale.clone();
        successor.instance_id = AgentInstanceId::new("successor");
        store.add_member(&team, &successor).unwrap();
        let original = fs::read(&config).unwrap();
        assert!(store.add_member(&team, stale).is_err());
        assert!(store.remove_member(&team, stale).is_err());
        assert_eq!(fs::read(&config).unwrap(), original);
        store.cleanup(&team).unwrap();
    }

    #[test]
    fn cleanup_checks_full_identity_and_members_before_effects() {
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let store = TeamStore::lock(&root.0.join("teams")).unwrap();
        store.initialize(&team).unwrap();
        let directory = root.0.join("teams").join(team.namespace());
        let path = directory.join(CONFIG_FILE);
        let original = fs::read(&path).unwrap();
        let mut same_prefix = stored_team(&root.0, "12345678-other");
        assert!(matches!(
            store.initialize(&same_prefix),
            Err(TeamStoreError::NamespaceConflict { .. })
        ));
        assert!(store.cleanup(&same_prefix).is_err());
        same_prefix = stored_team(&root.0, "12345678-root");
        assert!(store.cleanup(&same_prefix).is_err());
        for (pointer, value) in [
            ("/team_id", serde_json::json!("foreign")),
            ("/lead_id", serde_json::json!("12345678-foreign")),
            ("/namespace", serde_json::json!("session-foreign")),
            ("/task_list_id", serde_json::json!("foreign")),
            ("/members/0/instance_id", serde_json::json!("foreign")),
            ("/members/1/alias", serde_json::json!("lead")),
            ("/members/1/working_dir", serde_json::json!("relative")),
            ("/members/1/definition", serde_json::Value::Null),
        ] {
            let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
            *record.pointer_mut(pointer).unwrap() = value;
            let changed = serde_json::to_vec(&record).unwrap();
            fs::write(&path, &changed).unwrap();
            assert!(store.cleanup(&team).is_err(), "accepted {pointer}");
            assert_eq!(fs::read(&path).unwrap(), changed);
            assert_eq!(
                fs::read_dir(directory.join(INBOX_DIRECTORY))
                    .unwrap()
                    .count(),
                team.members().len()
            );
        }
        fs::write(&path, original).unwrap();
        let unexpected = directory.join("foreign");
        fs::write(&unexpected, b"keep").unwrap();
        assert!(store.cleanup(&team).is_err());
        assert!(directory.join("inboxes/lead.json").is_file());
        fs::remove_file(unexpected).unwrap();
        let inbox = directory.join("inboxes/security.json");
        let original_inbox = fs::read(&inbox).unwrap();
        fs::write(&inbox, b"broken JSON").unwrap();
        assert!(store.cleanup(&team).is_err());
        assert!(directory.join("inboxes/lead.json").is_file());
        fs::write(&inbox, original_inbox).unwrap();
        fs::remove_file(directory.join("inboxes/lead.json")).unwrap();
        store.cleanup(&team).unwrap();
    }

    #[test]
    fn bounded_files_and_replacement_fail_closed() {
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let store = TeamStore::lock(&root.0.join("teams")).unwrap();
        store.initialize(&team).unwrap();
        let directory = root.0.join("teams").join(team.namespace());
        let path = directory.join(CONFIG_FILE);
        let original = fs::read(&path).unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        assert!(store.cleanup(&team).is_err());
        assert!(directory.join("inboxes/lead.json").is_file());
        fs::write(&path, original).unwrap();
        let inbox_path = directory.join("inboxes/security.json");
        let mut inbox: TeamInboxRecord = read_record(&inbox_path, MAX_INBOX_BYTES).unwrap();
        inbox.recipient = AgentInstanceId::new("foreign");
        fs::write(&inbox_path, serde_json::to_vec(&inbox).unwrap()).unwrap();
        assert!(store.cleanup(&team).is_err());
        assert!(path.is_file());
        fs::remove_file(&inbox_path).unwrap();
        fs::create_dir(&inbox_path).unwrap();
        assert!(replace(&inbox_path, b"replacement").is_err());
        assert!(inbox_path.is_dir());
        assert!(store.cleanup(&team).is_err());
        fs::remove_dir(&inbox_path).unwrap();
        store.cleanup(&team).unwrap();
        assert!(matches!(
            TeamStore::lock(Path::new("relative")),
            Err(TeamStoreError::InvalidRoot)
        ));
        assert!(matches!(
            TeamStore::lock(&root.0.join("../other")),
            Err(TeamStoreError::InvalidRoot)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_partial_cleanup_preserve_association() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = TestRoot::new();
        let team = stored_team(&root.0, "12345678-root");
        let store_root = root.0.join("teams");
        let store = TeamStore::lock(&store_root).unwrap();
        store.initialize(&team).unwrap();
        let directory = store_root.join(team.namespace());
        let config = directory.join(CONFIG_FILE);
        let original = fs::read(&config).unwrap();
        let target = root.0.join("foreign.json");
        fs::write(&target, b"foreign").unwrap();
        for path in [config.clone(), directory.join("inboxes/lead.json")] {
            let bytes = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            symlink(&target, &path).unwrap();
            assert!(store.cleanup(&team).is_err());
            assert_eq!(fs::read(&target).unwrap(), b"foreign");
            fs::remove_file(&path).unwrap();
            fs::write(&path, bytes).unwrap();
        }
        let linked = root.0.join("linked");
        symlink(&store_root, &linked).unwrap();
        assert!(TeamStore::lock(&linked.join("nested")).is_err());
        assert!(!store_root.join("nested").exists());
        if unsafe { libc::geteuid() } != 0 {
            fs::set_permissions(&store_root, fs::Permissions::from_mode(0o500)).unwrap();
            let result = store.cleanup(&team);
            fs::set_permissions(&store_root, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(result.is_err());
            assert_eq!(fs::read(&config).unwrap(), original);
        }
        store.cleanup(&team).unwrap();
        symlink(&target, &directory).unwrap();
        assert!(matches!(
            store.initialize(&team),
            Err(TeamStoreError::NamespaceConflict { .. })
        ));
        assert!(store.cleanup(&team).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"foreign");
    }

    const WORKER_ROOT: &str = "PHI_TEAM_STORE_TEST_ROOT";
    const WORKER_CASE: &str = "PHI_TEAM_STORE_TEST_CASE";
    const WORKER_TEST: &str = "harness::manager::team::storage_tests::file_worker";

    #[test]
    fn file_worker() {
        let Some(root) = std::env::var_os(WORKER_ROOT).map(PathBuf::from) else {
            return;
        };
        let case = std::env::var(WORKER_CASE).unwrap();
        let team = stored_team(&root, &format!("12345678-{case}"));
        let store_root = root.join("teams");
        #[cfg(unix)]
        if case == "write-failure" {
            let store = TeamStore::lock(&store_root).unwrap();
            let mut previous = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            unsafe {
                assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut previous), 0);
                assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: previous.rlim_max,
                };
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            let result = store.initialize(&team);
            unsafe {
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &previous), 0);
            }
            assert!(result.is_err());
            assert!(!store_root.join(team.namespace()).exists());
            return;
        }
        fs::write(root.join(format!("ready-{case}")), b"ready").unwrap();
        let started = Instant::now();
        while !root.join("start").exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(LOCK_RETRY);
        }
        let store = TeamStore::lock(&store_root).unwrap();
        let outcome = match store.initialize(&team) {
            Ok(()) => "won",
            Err(TeamStoreError::NamespaceConflict { .. }) => "lost",
            Err(error) => panic!("unexpected initialization failure: {error}"),
        };
        fs::write(root.join(format!("outcome-{case}")), outcome).unwrap();
    }

    #[tokio::test]
    async fn competing_processes_cannot_merge_namespaces() {
        let root = TestRoot::new();
        let mut children = Vec::new();
        for case in ["one", "two"] {
            children.push(
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", WORKER_TEST, "--nocapture"])
                    .env(WORKER_ROOT, &root.0)
                    .env(WORKER_CASE, case)
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while !["one", "two"]
                .iter()
                .all(|case| root.0.join(format!("ready-{case}")).exists())
            {
                tokio::time::sleep(LOCK_RETRY).await;
            }
        })
        .await
        .unwrap();
        fs::write(root.0.join("start"), b"start").unwrap();
        for child in &mut children {
            assert!(
                tokio::time::timeout(Duration::from_secs(10), child.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success()
            );
        }
        let outcomes: Vec<_> = ["one", "two"]
            .iter()
            .map(|case| fs::read_to_string(root.0.join(format!("outcome-{case}"))).unwrap())
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
        let store_root = root.0.join("teams");
        let store = TeamStore::lock(&store_root).unwrap();
        let started = Instant::now();
        let blocked_root = store_root.clone();
        let blocked = tokio::task::spawn_blocking(move || TeamStore::lock(&blocked_root))
            .await
            .unwrap();
        assert!(matches!(blocked, Err(TeamStoreError::LockTimeout)));
        assert!(started.elapsed() >= LOCK_TIMEOUT);
        drop(store);
        assert!(TeamStore::lock(&store_root).is_ok());
        #[cfg(unix)]
        {
            let failure_root = TestRoot::new();
            let status = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", WORKER_TEST, "--nocapture"])
                .env(WORKER_ROOT, &failure_root.0)
                .env(WORKER_CASE, "write-failure")
                .kill_on_drop(true)
                .status()
                .await
                .unwrap();
            assert!(status.success());
            assert!(failure_root.0.join("teams/.lock").is_file());
        }
    }
}
