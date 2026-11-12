mod delegation;
mod messages;
pub(super) mod support;
mod tools;

use crate::agent_instance::{AgentInstance, AgentInstanceEvent, AgentInstanceId};
use crate::agents::{SubagentActivity, SubagentAdmission, SubagentStatus};
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::core::turn::TurnPrompt;
use crate::harness::instance::builder::DirectedDelegation;
use crate::harness::instance::handle::{SubmissionDispatch, SubmissionDisposition};
use crate::harness::manager::preparation::AgentSpawnRequest;
use crate::transcript::{AgentMessage, TranscriptEntryKind};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::*;
use tokio::net::TcpListener;

async fn wait_child_completion(parent: &AgentInstance, id: &AgentInstanceId) -> SubagentStatus {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = parent.subagent_status(id.clone()).await.unwrap().unwrap();
            if status.outcome.is_some() {
                return status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
