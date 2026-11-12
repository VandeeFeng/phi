mod bash;
mod execution;
mod interruption;
mod observation;
mod progress;
mod runtime;
mod teammate;

use crate::core::event::{AgentEvent, PhiEvent};
use crate::core::interaction::{InteractionResponse, PermissionDecision};
use crate::core::turn::TurnPrompt;
use crate::factory::{Phi, PhiRuntimeServices};
use crate::harness::instance::builder::AgentInstanceCandidate;
use crate::harness::instance::handle::{
    AgentInstance, AgentInstanceError, AgentInstanceEvent, AgentInstanceStatus, EVENT_CAPACITY,
    PhiStreamError, SubmissionDispatch, SubmissionDisposition, snapshot_is_current,
};
use crate::models::{ModelCatalog, ModelCost, ModelDescriptor, ModelInput};
use crate::permission::command::CommandPermissionRequest;
use crate::permission::{Action, PermissionConfig, ToolPerm};
use crate::provider::ProviderApi;
use crate::transcript::Transcript;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

pub(super) fn test_catalog() -> Arc<ModelCatalog> {
    let descriptor = ModelDescriptor {
        provider: "openai-codex".to_string(),
        id: "gpt-5.6-sol".to_string(),
        name: "GPT-5.6-Sol".to_string(),
        api: ProviderApi::OpenAiCodexResponses,
        reasoning: true,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        context_window: 272_000,
        max_output_tokens: 128_000,
        cost: ModelCost::default(),
    };
    Arc::new(ModelCatalog::for_test_model(descriptor).unwrap())
}

async fn test_prepare_agent_instance_with_transcript(
    transcript: Option<Transcript>,
    delegated_child: bool,
) -> AgentInstanceCandidate {
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .build()
        .unwrap()
        .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
    let mut builder = phi.agent_instance_builder();
    if let Some(transcript) = transcript {
        builder = builder.transcript(transcript);
    }
    if delegated_child {
        builder = builder.delegated_child();
    }
    builder.build().await.unwrap()
}

pub(super) async fn test_client() -> AgentInstanceCandidate {
    test_prepare_agent_instance_with_transcript(None, false).await
}

async fn test_instance() -> AgentInstance {
    AgentInstance::spawn(test_client().await)
}

async fn test_running_instance() -> (AgentInstance, mpsc::Sender<AgentEvent>) {
    let mut state = test_client().await;
    let events = state.install_test_main_run("active-turn");
    (AgentInstance::spawn(state), events)
}

async fn wait_for_status(instance: &AgentInstance, expected: AgentInstanceStatus) {
    let mut snapshots = instance.subscribe_snapshots();
    tokio::time::timeout(Duration::from_secs(1), async {
        while snapshots.snapshot().status != expected {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}
