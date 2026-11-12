use crate::harness::instance::AgentInstanceId;
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

// Bounded ordinary messages and process-local initial completion.

const MAX_MESSAGES_PER_RECIPIENT: usize = 64;

const MAX_PENDING_MESSAGES: usize = 1024;

const RECENT_MESSAGE_LIMIT: usize = 64;

const MAX_PENDING_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MESSAGE_DELIVERED_EVENT: &str = "phi_sdk.message_delivered";

#[derive(Clone, Copy)]
#[repr(u8)]
enum MessagePublication {
    Recording,
    Ready,
}

pub(crate) struct MessageRoute {
    pub(crate) message: AgentMessageEnvelope,
    sender_epoch: uuid::Uuid,
    recipient_epoch: uuid::Uuid,
    pub(crate) delivery: tokio::sync::watch::Sender<AgentMessageDelivery>,
    publication: AtomicU8,
}

impl MessageRoute {
    fn is_ready(&self) -> bool {
        self.publication.load(Ordering::Acquire) == MessagePublication::Ready as u8
    }

    fn publish(&self) -> Result<(), super::AgentManagerError> {
        let mut published = false;
        self.delivery.send_if_modified(|state| {
            if *state == AgentMessageDelivery::Queued {
                self.publication
                    .store(MessagePublication::Ready as u8, Ordering::Release);
                published = true;
            }
            false
        });
        if published {
            Ok(())
        } else {
            Err(super::AgentManagerError::AdmissionInterrupted)
        }
    }

    pub(crate) fn updates(&self) -> AgentMessageUpdates {
        AgentMessageUpdates {
            fact: AgentMessageUpdate {
                message_id: self.message.id.clone(),
                sender: self.message.sender.clone(),
                recipient: self.message.recipient.clone(),
                kind: self.message.kind.clone(),
                delivery: self.delivery.borrow().clone(),
            },
            sender_epoch: self.sender_epoch,
            recipient_epoch: self.recipient_epoch,
            delivery: self.delivery.clone(),
            updates: self.delivery.subscribe(),
        }
    }

    pub(crate) fn admit(&self) -> bool {
        if !self.is_ready() {
            return false;
        }
        self.delivery.send_if_modified(|state| {
            if *state != AgentMessageDelivery::Queued {
                return false;
            }
            *state = AgentMessageDelivery::Admitted;
            true
        })
    }

    pub(crate) async fn closed(&self) {
        let mut delivery = self.delivery.subscribe();
        let _ = delivery.wait_for(AgentMessageDelivery::is_terminal).await;
    }

    pub(crate) fn release_admission(&self) {
        self.delivery.send_if_modified(|state| {
            if *state != AgentMessageDelivery::Admitted {
                return false;
            }
            *state = AgentMessageDelivery::Queued;
            true
        });
    }

    pub(crate) fn reject(&self, reason: &str) {
        self.delivery.send_if_modified(|state| {
            if state.is_terminal() {
                return false;
            }
            *state = AgentMessageDelivery::Rejected {
                reason: reason.into(),
            };
            true
        });
    }

    // Serialize canonical actor effects with closure; Manager never writes history.
    pub(crate) fn commit<T>(&self, commit: impl FnOnce() -> (T, String)) -> Option<T> {
        let mut committed = None;
        self.delivery.send_if_modified(|state| {
            if *state != AgentMessageDelivery::Admitted {
                return false;
            }
            let (result, entry_id) = commit();
            *state = AgentMessageDelivery::Committed { entry_id };
            committed = Some(result);
            true
        });
        committed
    }
}

#[derive(Default)]
pub(crate) struct MessageMailbox {
    pending: VecDeque<Arc<MessageRoute>>,
    recent: VecDeque<AgentMessageUpdates>,
}

impl Drop for MessageMailbox {
    fn drop(&mut self) {
        for route in &self.pending {
            route.reject("message manager unavailable before commit");
        }
    }
}

impl MessageMailbox {
    fn prune(&mut self) {
        self.pending.retain(|route| {
            if !route.delivery.borrow().is_terminal() {
                return true;
            }
            self.recent.push_back(route.updates());
            while self.recent.len() > RECENT_MESSAGE_LIMIT {
                self.recent.pop_front();
            }
            false
        });
    }

    pub(crate) fn capture(&mut self, id: &AgentMessageId) -> Option<AgentMessageUpdates> {
        self.prune();
        self.pending
            .iter()
            .find(|route| route.message.id == *id)
            .map(|route| route.updates())
            .or_else(|| {
                self.recent
                    .iter()
                    .find(|updates| updates.fact.message_id == *id)
                    .cloned()
            })
    }

    fn validate(
        &mut self,
        recipient: uuid::Uuid,
        bytes: usize,
    ) -> Result<(), super::AgentManagerError> {
        self.prune();
        let pending_bytes = self.pending.iter().fold(bytes, |total, route| {
            total.saturating_add(route.message.content.len())
        });
        if pending_bytes > MAX_PENDING_MESSAGE_BYTES {
            return Err(super::AgentManagerError::MessageBytesLimit {
                limit: MAX_PENDING_MESSAGE_BYTES,
            });
        }
        if self.pending.len() >= MAX_PENDING_MESSAGES {
            return Err(super::AgentManagerError::MessageLimit {
                limit: MAX_PENDING_MESSAGES,
            });
        }
        if self
            .pending
            .iter()
            .filter(|route| route.recipient_epoch == recipient)
            .count()
            >= MAX_MESSAGES_PER_RECIPIENT
        {
            return Err(super::AgentManagerError::MessageLimit {
                limit: MAX_MESSAGES_PER_RECIPIENT,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn enqueue(
        &mut self,
        sender: AgentInstanceId,
        recipient: AgentInstanceId,
        recipient_epoch: uuid::Uuid,
        sender_epoch: uuid::Uuid,
        content: String,
        publication: MessagePublication,
    ) -> Result<AgentMessageReceipt, super::AgentManagerError> {
        self.enqueue_envelope(
            AgentMessageEnvelope {
                id: AgentMessageId(uuid::Uuid::new_v4().to_string()),
                sender,
                recipient,
                kind: AgentMessageKind::Ordinary,
                content,
            },
            recipient_epoch,
            sender_epoch,
            publication,
        )
    }

    fn enqueue_envelope(
        &mut self,
        message: AgentMessageEnvelope,
        recipient_epoch: uuid::Uuid,
        sender_epoch: uuid::Uuid,
        publication: MessagePublication,
    ) -> Result<AgentMessageReceipt, super::AgentManagerError> {
        self.validate(recipient_epoch, message.content.len())?;
        let route = Arc::new(MessageRoute {
            message,
            sender_epoch,
            recipient_epoch,
            delivery: tokio::sync::watch::channel(AgentMessageDelivery::Queued).0,
            publication: AtomicU8::new(publication as u8),
        });
        self.pending.push_back(route.clone());
        Ok(AgentMessageReceipt {
            updates: route.delivery.subscribe(),
            route,
        })
    }

    pub(crate) fn has_pending_messages(&self, recipient: uuid::Uuid) -> bool {
        self.pending.iter().any(|route| {
            route.recipient_epoch == recipient && !route.delivery.borrow().is_terminal()
        })
    }

    pub(crate) fn pending(&mut self, recipient: uuid::Uuid) -> Vec<Arc<MessageRoute>> {
        self.prune();
        self.pending
            .iter()
            .filter(|route| route.recipient_epoch == recipient)
            .take_while(|route| route.is_ready())
            .filter(|route| *route.delivery.borrow() == AgentMessageDelivery::Queued)
            .cloned()
            .collect()
    }

    pub(crate) fn reject(&mut self, recipient: uuid::Uuid) {
        for route in &self.pending {
            if route.recipient_epoch == recipient {
                route.reject("recipient stopped before message commit");
            }
        }
        self.prune();
    }
}

#[derive(Clone)]
pub(super) struct MessageTeam {
    pub(super) id: super::AgentTeamId,
    pub(super) alias: super::AgentAlias,
    pub(super) files: Arc<super::team::TeamFiles>,
}

#[derive(Clone, Copy)]
enum MessagePurpose {
    Live,
    TeammateExit,
}

#[derive(Clone)]
pub(super) struct MessageTarget {
    sender: AgentInstanceId,
    sender_epoch: uuid::Uuid,
    recipient: AgentInstanceId,
    recipient_epoch: uuid::Uuid,
    purpose: MessagePurpose,
    pub(super) team: Option<MessageTeam>,
}

impl MessageTarget {
    fn live(
        registry: &super::registry::Registry,
        sender: &super::registry::Member,
        recipient: &super::registry::Member,
    ) -> Result<Self, super::AgentManagerError> {
        let team = recipient
            .team
            .as_ref()
            .map(|id| {
                Ok::<_, super::AgentManagerError>(MessageTeam {
                    id: id.clone(),
                    alias: recipient
                        .alias
                        .clone()
                        .ok_or(super::AgentManagerError::AdmissionInterrupted)?,
                    files: registry.team(id)?.files.clone(),
                })
            })
            .transpose()?;
        Ok(Self {
            sender: sender.instance.id().clone(),
            sender_epoch: sender.epoch,
            recipient: recipient.instance.id().clone(),
            recipient_epoch: recipient.epoch,
            purpose: MessagePurpose::Live,
            team,
        })
    }

    pub(super) fn teammate_exit(
        registry: &super::registry::Registry,
        sender: &AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<Option<Self>, super::AgentManagerError> {
        let member = registry.member(sender)?;
        if member.epoch != epoch || !member.closing {
            return Err(super::AgentManagerError::AdmissionInterrupted);
        }
        if !registry.is_published(member) {
            return Ok(None);
        }
        let id = member
            .team
            .as_ref()
            .ok_or(super::AgentManagerError::AdmissionInterrupted)?;
        let state = registry.team(id)?;
        let target = Self {
            sender: sender.clone(),
            sender_epoch: epoch,
            recipient: state.config.lead().clone(),
            recipient_epoch: state.lead_epoch,
            purpose: MessagePurpose::TeammateExit,
            team: Some(MessageTeam {
                id: id.clone(),
                alias: super::AgentAlias::new(super::LEAD_ALIAS).expect("reserved lead alias"),
                files: state.files.clone(),
            }),
        };
        if target.recipient_closed(registry) {
            return Ok(None);
        }
        target.validate(registry)?;
        Ok(Some(target))
    }

    fn recipient_closed(&self, registry: &super::registry::Registry) -> bool {
        registry.shutting_down
            || registry.members.get(&self.recipient).is_none_or(|member| {
                member.closing
                    || member.epoch != self.recipient_epoch
                    || member
                        .instance
                        .subscribe_finalization()
                        .is_none_or(|state| {
                            !matches!(*state.borrow(), super::AgentFinalizationStatus::Open)
                        })
            })
    }

    pub(super) fn envelope(&self, content: String) -> AgentMessageEnvelope {
        AgentMessageEnvelope {
            id: AgentMessageId(uuid::Uuid::new_v4().to_string()),
            sender: self.sender.clone(),
            recipient: self.recipient.clone(),
            kind: AgentMessageKind::Ordinary,
            content,
        }
    }

    pub(super) fn validate(
        &self,
        registry: &super::registry::Registry,
    ) -> Result<(), super::AgentManagerError> {
        let sender = registry.member(&self.sender)?;
        let recipient = registry.member(&self.recipient)?;
        if registry.shutting_down
            || recipient.closing
            || sender.epoch != self.sender_epoch
            || recipient.epoch != self.recipient_epoch
            || !registry.is_published(sender)
            || !registry.is_published(recipient)
        {
            return Err(super::AgentManagerError::AdmissionInterrupted);
        }
        match self.purpose {
            MessagePurpose::Live if sender.closing => {
                return Err(super::AgentManagerError::AdmissionInterrupted);
            }
            MessagePurpose::TeammateExit => {
                let scope = self
                    .team
                    .as_ref()
                    .ok_or(super::AgentManagerError::AdmissionInterrupted)?;
                let state = registry.team(&scope.id)?;
                if self.recipient_closed(registry)
                    || !sender.closing
                    || sender.team.as_ref() != Some(&scope.id)
                    || !matches!(sender.snapshot().kind, super::AgentKind::Teammate { .. })
                    || state.config.lead() != &self.recipient
                    || state.lead_epoch != self.recipient_epoch
                {
                    return Err(super::AgentManagerError::AdmissionInterrupted);
                }
            }
            _ => {}
        }
        if let Some(scope) = &self.team {
            let state = registry.team(&scope.id)?;
            if recipient.team.as_ref() != Some(&scope.id)
                || recipient.alias.as_ref() != Some(&scope.alias)
                || !Arc::ptr_eq(&state.files, &scope.files)
            {
                return Err(super::AgentManagerError::AdmissionInterrupted);
            }
        } else if recipient.team.is_some() {
            return Err(super::AgentManagerError::AdmissionInterrupted);
        }
        Ok(())
    }
}

struct PendingRecording(Arc<MessageRoute>);

impl Drop for PendingRecording {
    fn drop(&mut self) {
        if !self.0.is_ready() {
            self.0
                .reject("message recording interrupted before publication");
        }
    }
}

impl super::AgentManager {
    pub(crate) async fn publish_idle_report(
        &self,
        sender: &AgentInstanceId,
        epoch: uuid::Uuid,
        content: String,
    ) -> Result<(), super::AgentManagerError> {
        use super::AgentKind;
        let (target, message) = {
            let registry = self.lock_live()?;
            let member = registry.member(sender)?;
            if member.epoch != epoch || member.closing || !registry.is_published(member) {
                return Err(super::AgentManagerError::AdmissionInterrupted);
            }
            let (recipient, recipient_epoch, kind) = match member.snapshot().kind {
                AgentKind::Master => return Ok(()),
                AgentKind::Subagent { creator } => (
                    creator,
                    *member
                        .ancestors
                        .last()
                        .ok_or(super::AgentManagerError::AdmissionInterrupted)?,
                    AgentMessageKind::SubagentResult,
                ),
                AgentKind::Teammate { team, .. } => {
                    let team = registry.team(&team)?;
                    (
                        team.config.lead().clone(),
                        team.lead_epoch,
                        AgentMessageKind::TeammateIdle,
                    )
                }
            };
            let Some(recipient_member) = registry.members.get(&recipient) else {
                return Ok(());
            };
            if recipient_member.epoch != recipient_epoch {
                return Ok(());
            }
            let target = MessageTarget::live(&registry, member, recipient_member)?;
            if target.recipient_closed(&registry) {
                return Ok(());
            }
            target.validate(&registry)?;
            let mut message = target.envelope(content);
            message.kind = kind;
            (target, message)
        };
        self.publish_message(target, message).await?;
        Ok(())
    }

    pub(crate) async fn queue_message(
        &self,
        sender: &AgentInstanceId,
        recipient: &AgentInstanceId,
        content: String,
        access: Option<&super::policy::AgentAccess>,
    ) -> Result<AgentMessageReceipt, super::AgentManagerError> {
        let target = {
            let registry = self.lock_live()?;
            if let Some(access) = access {
                access.require_message(&registry, recipient)?;
            }
            let sender_member = registry.member(sender)?;
            let recipient = if access.is_some() {
                registry.resolve_target(sender_member, recipient)?
            } else {
                registry.member(recipient)?
            };
            let target = MessageTarget::live(&registry, sender_member, recipient)?;
            target.validate(&registry)?;
            target
        };
        let message = target.envelope(content);
        self.publish_message(target, message).await
    }

    pub(super) async fn publish_exit_notice(
        &self,
        target: &MessageTarget,
        message: AgentMessageEnvelope,
    ) -> Result<(), super::AgentManagerError> {
        if !matches!(target.purpose, MessagePurpose::TeammateExit)
            || message.sender != target.sender
            || message.recipient != target.recipient
        {
            return Err(super::AgentManagerError::AdmissionInterrupted);
        }
        if target.recipient_closed(&*self.lock()?) {
            return Ok(());
        }
        match self.publish_message(target.clone(), message).await {
            Ok(_) => Ok(()),
            Err(_) if target.recipient_closed(&*self.lock()?) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn publish_message(
        &self,
        target: MessageTarget,
        message: AgentMessageEnvelope,
    ) -> Result<AgentMessageReceipt, super::AgentManagerError> {
        let Some(team) = &target.team else {
            let mut registry = self.lock_live()?;
            target.validate(&registry)?;
            return registry.messages.enqueue_envelope(
                message,
                target.recipient_epoch,
                target.sender_epoch,
                MessagePublication::Ready,
            );
        };
        let files = team.files.clone();
        let lane = files.lock().await;
        let receipt = {
            let mut registry = self.lock_live()?;
            target.validate(&registry)?;
            registry.messages.enqueue_envelope(
                message,
                target.recipient_epoch,
                target.sender_epoch,
                MessagePublication::Recording,
            )?
        };
        let pending = PendingRecording(receipt.route.clone());
        let manager = self.clone();
        tokio::spawn(async move {
            let result: Result<_, super::AgentManagerError> = async {
                files
                    .record_message(manager.clone(), target.clone(), receipt.route.clone(), lane)
                    .await?;
                let registry = manager.lock_live()?;
                target.validate(&registry)?;
                receipt.route.publish()?;
                Ok(receipt)
            }
            .await;
            if let Err(error) = &result {
                pending.0.reject(&error.to_string());
            }
            result
        })
        .await
        .map_err(|error| super::AgentManagerError::TeamStorage {
            message: error.to_string(),
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::manager::message::AgentMessageWait;
    use crate::harness::manager::message::agent_wait_timeout;

    #[tokio::test]
    async fn inbox_pruning_survives_receipt_eviction() {
        use crate::agent_manager::{AgentManager, TeamStorageRoot};
        use crate::factory::{Phi, PhiRuntimeServices};
        use crate::models::{ModelCatalog, ModelDescriptor, ModelInput};

        let root = std::path::Path::new("/tmp").join(format!("phi-inbox-{}", uuid::Uuid::new_v4()));
        let catalog = ModelCatalog::for_test_model(ModelDescriptor {
            provider: "openai-codex".into(),
            id: "gpt-5.6-sol".into(),
            name: "Test".into(),
            api: crate::provider::ProviderApi::OpenAiCodexResponses,
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: Default::default(),
        })
        .unwrap();
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .hooks(vec![crate::hooks::Hook::user_prompt_submit(|_| async {
                Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
            })])
            .build()
            .unwrap()
            .with_runtime_services(PhiRuntimeServices::new().model_catalog(Arc::new(catalog)));
        let manager = AgentManager::new();
        let lead = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(
                        crate::tasks::TaskListBinding::new(root.join("tasks"), "inbox").unwrap(),
                    )
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let noise = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        manager
            .initialize_team(lead.id(), TeamStorageRoot::new(root.join("teams")).unwrap())
            .await
            .unwrap();
        const INBOX_MESSAGE_LIMIT: usize = 128;
        for _ in 0..=INBOX_MESSAGE_LIMIT {
            let receipt = manager
                .send_message(noise.id(), lead.id(), "work")
                .await
                .unwrap();
            receipt.route.admit();
            receipt.route.commit(|| ((), "canonical-entry".into()));
            assert!(receipt.delivery().is_terminal());
            for _ in 0..=RECENT_MESSAGE_LIMIT {
                let receipt = manager
                    .send_message(noise.id(), noise.id(), "noise")
                    .await
                    .unwrap();
                receipt.route.admit();
                receipt.route.commit(|| ((), "noise-entry".into()));
                assert!(receipt.delivery().is_terminal());
            }
        }
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn ordinary_message_routes_bound_capacity_and_serialize_commit_with_stop() {
        let mut mailbox = MessageMailbox::default();
        let recipient = uuid::Uuid::new_v4();
        let mut receipts = Vec::new();
        for _ in 0..MAX_MESSAGES_PER_RECIPIENT {
            receipts.push(
                mailbox
                    .enqueue(
                        AgentInstanceId::new("sender"),
                        AgentInstanceId::new("recipient"),
                        recipient,
                        recipient,
                        "same text".into(),
                        MessagePublication::Ready,
                    )
                    .unwrap(),
            );
        }
        assert_ne!(receipts[0].message().id, receipts[1].message().id);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), receipts[0].changed())
                .await
                .is_err()
        );
        assert!(receipts[0].wait(std::time::Duration::ZERO).await.timed_out);
        assert!(agent_wait_timeout(MAX_AGENT_WAIT_MS + 1).is_err());
        assert!(matches!(
            mailbox.enqueue(
                AgentInstanceId::new("sender"),
                AgentInstanceId::new("recipient"),
                recipient,
                recipient,
                "overflow".into(),
                MessagePublication::Ready,
            ),
            Err(super::super::AgentManagerError::MessageLimit { .. })
        ));
        assert!(mailbox.pending(uuid::Uuid::new_v4()).is_empty());
        let pending = mailbox.pending(recipient);
        let first = &pending[0];
        assert!(first.admit());
        assert!(!first.admit());
        first.release_admission();
        assert!(first.admit());
        assert_eq!(first.commit(|| ((), "entry".into())), Some(()));
        assert_eq!(first.commit::<()>(|| panic!("duplicate commit")), None);
        mailbox.reject(recipient);
        assert!(matches!(
            receipts[0].delivery(),
            AgentMessageDelivery::Committed { .. }
        ));
        assert!(matches!(
            receipts[1].delivery(),
            AgentMessageDelivery::Rejected { .. }
        ));
        assert!(
            pending[1]
                .commit::<()>(|| panic!("stopped recipient"))
                .is_none()
        );
        assert!(mailbox.pending(recipient).is_empty());
        for receipt in &mut receipts[..2] {
            let waited = receipt.wait(std::time::Duration::ZERO).await;
            assert!(!waited.timed_out);
            assert!(waited.delivery.is_terminal());
            let decoded: AgentMessageWait =
                serde_json::from_value(serde_json::to_value(&waited).unwrap()).unwrap();
            assert_eq!(decoded.message.id, waited.message.id);
        }
        mailbox
            .enqueue(
                AgentInstanceId::new("sender"),
                AgentInstanceId::new("recipient"),
                recipient,
                recipient,
                "x".repeat(64 * 1024 + 1),
                MessagePublication::Ready,
            )
            .unwrap();
        assert!(matches!(
            mailbox.validate(recipient, MAX_PENDING_MESSAGE_BYTES),
            Err(super::super::AgentManagerError::MessageBytesLimit { .. })
        ));
        let abandoned = mailbox.pending(recipient)[0].clone();
        drop(mailbox);
        assert!(matches!(
            *abandoned.delivery.borrow(),
            AgentMessageDelivery::Rejected { .. }
        ));
    }

    #[test]
    fn recording_blocks_admission_and_overtaking_without_blocking_other_recipients() {
        for reject_recording in [false, true] {
            let mut mailbox = MessageMailbox::default();
            let recipient = uuid::Uuid::new_v4();
            let sender = uuid::Uuid::new_v4();
            let first = mailbox
                .enqueue(
                    AgentInstanceId::new("sender"),
                    AgentInstanceId::new("recipient"),
                    recipient,
                    sender,
                    "recording".into(),
                    MessagePublication::Recording,
                )
                .unwrap();
            let second = mailbox
                .enqueue(
                    AgentInstanceId::new("sender"),
                    AgentInstanceId::new("recipient"),
                    recipient,
                    sender,
                    "ready".into(),
                    MessagePublication::Ready,
                )
                .unwrap();
            let other_epoch = uuid::Uuid::new_v4();
            let other = mailbox
                .enqueue(
                    AgentInstanceId::new("sender"),
                    AgentInstanceId::new("other"),
                    other_epoch,
                    sender,
                    "independent".into(),
                    MessagePublication::Ready,
                )
                .unwrap();
            assert!(!first.route.admit());
            assert!(mailbox.has_pending_messages(recipient));
            assert!(mailbox.pending(recipient).is_empty());
            assert_eq!(
                mailbox.pending(other_epoch)[0].message.id,
                other.message().id
            );
            assert!(
                mailbox
                    .validate(recipient, MAX_PENDING_MESSAGE_BYTES)
                    .is_err()
            );
            if reject_recording {
                first.route.reject("recording failed");
                assert!(first.route.publish().is_err());
                assert_eq!(mailbox.pending(recipient).len(), 1);
                assert_eq!(
                    mailbox.pending(recipient)[0].message.id,
                    second.message().id
                );
            } else {
                first.route.publish().unwrap();
                let pending = mailbox.pending(recipient);
                assert_eq!(pending.len(), 2);
                assert_eq!(pending[0].message.id, first.message().id);
                assert_eq!(pending[1].message.id, second.message().id);
                assert!(pending[0].admit());
            }
        }
        let mut mailbox = MessageMailbox::default();
        let recipient = uuid::Uuid::new_v4();
        for _ in 0..MAX_MESSAGES_PER_RECIPIENT {
            mailbox
                .enqueue(
                    AgentInstanceId::new("sender"),
                    AgentInstanceId::new("recipient"),
                    recipient,
                    uuid::Uuid::new_v4(),
                    "recording".into(),
                    MessagePublication::Recording,
                )
                .unwrap();
        }
        assert!(matches!(
            mailbox.validate(recipient, 0),
            Err(super::super::AgentManagerError::MessageLimit { .. })
        ));
        assert!(mailbox.pending(recipient).is_empty());
    }

    #[tokio::test]
    async fn ordinary_receipt_recapture_retains_only_bounded_terminal_facts() {
        let mut mailbox = MessageMailbox::default();
        let recipient = uuid::Uuid::new_v4();
        let sent = mailbox
            .enqueue(
                AgentInstanceId::new("sender"),
                AgentInstanceId::new("recipient"),
                recipient,
                uuid::Uuid::new_v4(),
                "payload".into(),
                MessagePublication::Ready,
            )
            .unwrap();
        let id = sent.message().id.clone();
        assert!(mailbox.has_pending_messages(recipient));
        assert!(!mailbox.has_pending_messages(uuid::Uuid::new_v4()));
        let mut observer = mailbox.capture(&id).unwrap();
        assert!(observer.wait(std::time::Duration::ZERO).await.timed_out);
        let route = mailbox.pending(recipient).pop().unwrap();
        assert!(route.admit());
        assert!(mailbox.has_pending_messages(recipient));
        route.commit(|| ((), "entry".into())).unwrap();
        assert!(!mailbox.has_pending_messages(recipient));
        mailbox.prune();
        let mut late = mailbox.capture(&id).unwrap();
        assert!(!late.wait(std::time::Duration::ZERO).await.timed_out);
        assert!(
            !serde_json::to_string(&late.snapshot())
                .unwrap()
                .contains("payload")
        );
        for _ in 0..RECENT_MESSAGE_LIMIT {
            let sent = mailbox
                .enqueue(
                    AgentInstanceId::new("sender"),
                    AgentInstanceId::new("recipient"),
                    recipient,
                    uuid::Uuid::new_v4(),
                    "later".into(),
                    MessagePublication::Ready,
                )
                .unwrap();
            sent.route.reject("test rejection");
            mailbox.prune();
        }
        assert!(mailbox.capture(&id).is_none());
        drop(mailbox);
        assert!(!observer.wait(std::time::Duration::ZERO).await.timed_out);
        assert_eq!(
            observer.snapshot().delivery,
            AgentMessageDelivery::Committed {
                entry_id: "entry".into()
            }
        );
    }
}

// Addressed message contracts, bounded waits and non-consuming delivery observations.

pub const MAX_AGENT_WAIT_MS: u64 = 30_000;

pub fn agent_wait_timeout(
    milliseconds: u64,
) -> Result<std::time::Duration, crate::harness::manager::AgentManagerError> {
    if milliseconds > MAX_AGENT_WAIT_MS {
        return Err(
            crate::harness::manager::AgentManagerError::InvalidWaitTimeout {
                limit: MAX_AGENT_WAIT_MS,
            },
        );
    }
    Ok(std::time::Duration::from_millis(milliseconds))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentMessageWait {
    pub message: AgentMessageEnvelope,
    pub delivery: AgentMessageDelivery,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AgentMessageId(pub(crate) String);

impl AgentMessageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    Ordinary,
    TeammateIdle,
    SubagentResult,
    TeammateExit { released_tasks: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentMessageEnvelope {
    pub id: AgentMessageId,
    pub sender: AgentInstanceId,
    pub recipient: AgentInstanceId,
    pub kind: AgentMessageKind,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentMessageDelivery {
    Queued,
    Admitted,
    Committed { entry_id: String },
    Rejected { reason: String },
}

impl AgentMessageDelivery {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Committed { .. } | Self::Rejected { .. })
    }
}

/// Queue acceptance is not canonical commit or completion of recipient processing.
#[derive(Clone)]
pub struct AgentMessageReceipt {
    pub(crate) route: Arc<MessageRoute>,
    pub(crate) updates: tokio::sync::watch::Receiver<AgentMessageDelivery>,
}

impl AgentMessageReceipt {
    pub fn message(&self) -> &AgentMessageEnvelope {
        &self.route.message
    }

    pub fn updates(&self) -> AgentMessageUpdates {
        self.route.updates()
    }

    pub fn delivery(&self) -> AgentMessageDelivery {
        self.route.delivery.borrow().clone()
    }

    /// Waits for canonical commit or rejection, never for recipient model processing.
    pub async fn wait(&mut self, timeout: std::time::Duration) -> AgentMessageWait {
        let _ = tokio::time::timeout(timeout, async {
            while !self.delivery().is_terminal() {
                self.changed().await;
            }
        })
        .await;
        let delivery = self.delivery();
        AgentMessageWait {
            timed_out: !delivery.is_terminal(),
            message: self.message().clone(),
            delivery,
        }
    }

    /// Cancellation-safe, non-consuming wait; admission retries may coalesce.
    pub async fn changed(&mut self) -> AgentMessageDelivery {
        self.updates
            .changed()
            .await
            .expect("message retains update sender");
        self.delivery()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentMessageUpdate {
    pub message_id: AgentMessageId,
    pub sender: AgentInstanceId,
    pub recipient: AgentInstanceId,
    pub kind: AgentMessageKind,
    pub delivery: AgentMessageDelivery,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentMessageUpdateWait {
    pub update: AgentMessageUpdate,
    pub timed_out: bool,
}

#[derive(Clone)]
pub struct AgentMessageUpdates {
    pub(crate) fact: AgentMessageUpdate,
    pub(crate) sender_epoch: uuid::Uuid,
    pub(crate) recipient_epoch: uuid::Uuid,
    pub(crate) delivery: tokio::sync::watch::Sender<AgentMessageDelivery>,
    pub(crate) updates: tokio::sync::watch::Receiver<AgentMessageDelivery>,
}

impl AgentMessageUpdates {
    pub fn snapshot(&self) -> AgentMessageUpdate {
        let mut fact = self.fact.clone();
        fact.delivery = self.delivery.borrow().clone();
        fact
    }

    pub async fn changed(&mut self) -> AgentMessageUpdate {
        self.updates
            .changed()
            .await
            .expect("receipt retains update sender");
        self.snapshot()
    }

    pub async fn wait(&mut self, timeout: std::time::Duration) -> AgentMessageUpdateWait {
        let _ = tokio::time::timeout(timeout, async {
            while !self.delivery.borrow().is_terminal() {
                self.changed().await;
            }
        })
        .await;
        let update = self.snapshot();
        AgentMessageUpdateWait {
            timed_out: !update.delivery.is_terminal(),
            update,
        }
    }

    pub(crate) fn belongs_to(
        &self,
        registration: crate::harness::manager::AgentRegistrationId,
    ) -> bool {
        [self.sender_epoch, self.recipient_epoch]
            .into_iter()
            .any(|epoch| crate::harness::manager::AgentRegistrationId::new(epoch) == registration)
    }
}

impl crate::harness::manager::AgentManager {
    pub fn message_updates(
        &self,
        id: &AgentMessageId,
    ) -> Result<Option<AgentMessageUpdates>, crate::harness::manager::AgentManagerError> {
        Ok(self.lock_live()?.messages.capture(id))
    }

    pub fn participant_message_updates(
        &self,
        registration: crate::harness::manager::AgentRegistrationId,
        id: &AgentMessageId,
    ) -> Result<Option<AgentMessageUpdates>, crate::harness::manager::AgentManagerError> {
        Ok(self
            .lock_live()?
            .messages
            .capture(id)
            .filter(|updates| updates.belongs_to(registration)))
    }

    pub async fn send_message(
        &self,
        sender: &AgentInstanceId,
        recipient: &AgentInstanceId,
        content: impl Into<String>,
    ) -> Result<AgentMessageReceipt, crate::harness::manager::AgentManagerError> {
        self.queue_message(sender, recipient, content.into(), None)
            .await
    }
}
