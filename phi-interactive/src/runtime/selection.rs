//! Client selection policy is separate from hosted Session lifetime.

use super::attachment::AgentAttachment;
use super::preparation::{AcceptedTransition, SessionWorkflows};
use super::{RuntimeView, SavedSessionLocator};
use phi_sdk::agent_instance::AgentInstanceStatus;

pub(crate) enum SessionSelection {
    Embedded(SessionWorkflows),
    Hosted,
}

impl SessionSelection {
    pub(crate) async fn choices(
        &self,
        current: &AgentAttachment,
        query: &str,
    ) -> anyhow::Result<Vec<crate::session::discovery::SessionSummary>> {
        match self {
            Self::Embedded(_) => crate::session::discovery::discover_sessions(query),
            Self::Hosted => current.saved_session_choices(query).await,
        }
    }

    pub(crate) async fn new_session(
        &self,
        current: &AgentAttachment,
    ) -> anyhow::Result<AcceptedTransition> {
        match self {
            Self::Embedded(workflows) => {
                ensure_idle(current)?;
                workflows.new_session(current.id()).await
            }
            Self::Hosted => hosted_transition(current.new_hosted_session().await?).await,
        }
    }

    pub(crate) async fn resume_session(
        &self,
        current: &AgentAttachment,
        id: &str,
    ) -> anyhow::Result<AcceptedTransition> {
        match self {
            Self::Embedded(workflows) => {
                ensure_idle(current)?;
                workflows
                    .resume_session(current.id(), SavedSessionLocator::IdOrPath(id))
                    .await
            }
            Self::Hosted => hosted_transition(current.resume_hosted_session(id).await?).await,
        }
    }
}

fn ensure_idle(current: &AgentAttachment) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(
            current.snapshot().status,
            AgentInstanceStatus::Idle | AgentInstanceStatus::Failed
        ),
        "cannot replace an embedded Session while the agent is running"
    );
    Ok(())
}

async fn hosted_transition(attachment: AgentAttachment) -> anyhow::Result<AcceptedTransition> {
    let mut events = attachment.subscribe();
    let recovery = attachment.recover_events(&mut events).await?;
    Ok(AcceptedTransition {
        attachment,
        accepted: RuntimeView {
            session: recovery.session,
            snapshot: recovery.snapshot,
            declarations: recovery.declarations,
        },
        navigation_error: None,
    })
}
