use super::registry::{AgentManager, AgentManagerError, Registry, instance_error};
use crate::harness::instance::AgentInstance;
use crate::harness::instance::child::InitialCompletion;
use crate::harness::instance::child::SubagentAdmission;
pub(crate) use crate::harness::manager::{AgentFinalizationStatus, AgentRegistrationId};
use crate::transcript::Transcript;

// Captured local cancellation and creation-subtree shutdown; closure precedes actor awaits.

pub(super) struct TeammateExit {
    tasks: crate::harness::task::TaskExitAccess,
    alias: super::AgentAlias,
    released: std::collections::BTreeSet<String>,
    release_finished: bool,
    target: Option<super::message::MessageTarget>,
    notice: Option<super::message::AgentMessageEnvelope>,
    notice_finished: bool,
}

impl TeammateExit {
    async fn finish(
        &mut self,
        manager: &AgentManager,
        instance: &crate::harness::instance::AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<(), AgentManagerError> {
        if !self.release_finished {
            match self.tasks.release().await {
                Ok(released) => {
                    self.released.extend(released);
                    self.release_finished = true;
                }
                Err(error) => {
                    self.released.extend(error.released.iter().cloned());
                    return Err(AgentManagerError::StopCoordination {
                        message: error.to_string(),
                    });
                }
            }
        }
        if !self.notice_finished {
            if let Some(target) = &self.target {
                let notice = self.notice.get_or_insert_with(|| {
                    let mut notice = target.envelope(format!(
                        "Teammate {} ({instance}) exited. Released unfinished Task IDs: {}.",
                        self.alias.as_str(),
                        serde_json::to_string(&self.released).expect("Task ID strings"),
                    ));
                    notice.kind = super::message::AgentMessageKind::TeammateExit {
                        released_tasks: self.released.iter().cloned().collect(),
                    };
                    notice
                });
                manager.publish_exit_notice(target, notice.clone()).await?;
            }
            self.notice_finished = true;
        }
        manager.cleanup_teammate(instance, epoch).await
    }
}

#[derive(Default)]
pub(crate) struct ChildCancellation {
    pub(crate) changed: bool,
    pub(crate) completion: Option<InitialCompletion>,
}

impl ChildCancellation {
    pub(crate) async fn finish(self) -> Result<bool, AgentManagerError> {
        if let Some(completion) = self.completion {
            if let Some(message) = completion.wait_admission_finished().await {
                return Err(AgentManagerError::Association { message });
            }
            if completion.admission() == SubagentAdmission::Accepted {
                completion.wait_complete().await;
            }
        }
        Ok(self.changed)
    }
}

impl Registry {
    pub(crate) fn close_members(
        &mut self,
        includes: impl Fn(&super::registry::Member) -> bool,
    ) -> Vec<(AgentInstance, uuid::Uuid)> {
        let mut members = Vec::new();
        for member in self.members.values_mut().filter(|member| includes(member)) {
            member.closing = true;
            member.instance.close_registration();
            members.push((member.instance.clone(), member.epoch));
        }
        for (_, epoch) in &members {
            self.messages.reject(*epoch);
        }
        members.sort_by(|left, right| left.0.id().as_str().cmp(right.0.id().as_str()));
        members
    }
}

impl AgentManager {
    pub(crate) async fn stop_registered(
        &self,
        instance: AgentInstance,
        epoch: uuid::Uuid,
    ) -> Result<Transcript, AgentManagerError> {
        instance.close_registration();
        let mut descendants = match self.close_subtree(epoch) {
            Ok(descendants) => descendants,
            Err(error) => {
                instance.finish_registration(Some(error.to_string()));
                return Err(error);
            }
        };
        descendants.retain(|(_, member_epoch)| *member_epoch != epoch);
        let mut members = vec![(instance, epoch)];
        members.extend(descendants);
        let mut transcripts = self.stop_members(members).await?;
        Ok(transcripts.remove(0))
    }

    fn close_subtree(
        &self,
        epoch: uuid::Uuid,
    ) -> Result<Vec<(AgentInstance, uuid::Uuid)>, AgentManagerError> {
        Ok(self
            .lock()?
            .close_members(|member| member.epoch == epoch || member.ancestors.contains(&epoch)))
    }

    pub(crate) async fn stop_members(
        &self,
        members: Vec<(AgentInstance, uuid::Uuid)>,
    ) -> Result<Vec<Transcript>, AgentManagerError> {
        let manager = self.clone();
        let mut ordered = {
            let registry = self.lock()?;
            members
                .into_iter()
                .enumerate()
                .map(|(index, (instance, epoch))| {
                    let depth = registry
                        .members
                        .get(instance.id())
                        .filter(|member| member.epoch == epoch)
                        .map_or(0, |member| member.ancestors.len());
                    (index, instance, epoch, depth)
                })
                .collect::<Vec<_>>()
        };
        ordered.sort_by_key(|(index, _, _, depth)| (std::cmp::Reverse(*depth), *index));
        // Closure has committed; dropping the caller must not abandon captured peers.
        tokio::spawn(async move {
            let mut transcripts = Vec::with_capacity(ordered.len());
            let mut failure = None;
            for (index, instance, epoch, _) in ordered {
                match manager.stop_member(instance, epoch).await {
                    Ok(transcript) => transcripts.push((index, transcript)),
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
            transcripts.sort_by_key(|(index, _)| *index);
            failure.map_or_else(
                || {
                    Ok(transcripts
                        .into_iter()
                        .map(|(_, transcript)| transcript)
                        .collect())
                },
                Err,
            )
        })
        .await
        .map_err(|error| AgentManagerError::StopCoordination {
            message: error.to_string(),
        })?
    }

    async fn stop_member(
        &self,
        instance: AgentInstance,
        epoch: uuid::Uuid,
    ) -> Result<Transcript, AgentManagerError> {
        let result = self.finalize_member(&instance, epoch).await;
        instance.finish_registration(result.as_ref().err().map(ToString::to_string));
        result
    }

    fn capture_teammate_exit(
        &self,
        instance: &crate::harness::instance::AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Result<Option<std::sync::Arc<tokio::sync::Mutex<TeammateExit>>>, AgentManagerError> {
        let mut registry = self.lock()?;
        let Some(member) = registry
            .members
            .get(instance)
            .filter(|member| member.epoch == epoch)
        else {
            return Ok(None);
        };
        if !matches!(member.snapshot().kind, super::AgentKind::Teammate { .. }) {
            return Ok(None);
        }
        if let Some(exit) = &member.exit {
            return Ok(Some(exit.clone()));
        }
        let tasks = crate::harness::task::TaskExitAccess::capture(self, &registry, instance, epoch)
            .map_err(|error| AgentManagerError::StopCoordination {
                message: error.to_string(),
            })?;
        let target = super::message::MessageTarget::teammate_exit(&registry, instance, epoch)?;
        let alias = member
            .alias
            .clone()
            .ok_or(AgentManagerError::AdmissionInterrupted)?;
        let exit = std::sync::Arc::new(tokio::sync::Mutex::new(TeammateExit {
            tasks,
            alias,
            released: Default::default(),
            release_finished: false,
            target,
            notice: None,
            notice_finished: false,
        }));
        registry
            .members
            .get_mut(instance)
            .expect("captured teammate")
            .exit = Some(exit.clone());
        Ok(Some(exit))
    }

    async fn finalize_member(
        &self,
        instance: &AgentInstance,
        epoch: uuid::Uuid,
    ) -> Result<Transcript, AgentManagerError> {
        let exit = self.capture_teammate_exit(instance.id(), epoch)?;
        let transcript = match instance.stop_local().await {
            Ok(transcript) => transcript,
            Err(error) => instance
                .final_transcript()
                .ok_or_else(|| instance_error(instance.id(), error))?,
        };
        if let Some(exit) = exit {
            exit.lock().await.finish(self, instance.id(), epoch).await?;
        }
        self.cleanup_team(epoch).await?;
        let mut registry = self.lock()?;
        if registry
            .members
            .get(instance.id())
            .is_some_and(|member| member.epoch == epoch)
        {
            registry.remove_membership(instance.id(), epoch);
            registry.members.remove(instance.id());
        }
        registry.prune_stopped();
        Ok(transcript)
    }
}
