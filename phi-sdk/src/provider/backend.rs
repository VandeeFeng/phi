use super::transport::ProviderTransport;
use super::{
    ProviderAdapter, ProviderApi, auth, create_chatgpt_client, generation, resolve_api_key,
    resolve_provider,
};
use crate::agent::options::AgentOptions;
use crate::agent::{builder, options as prompt};
use crate::config::{CustomProviderConfig, RuntimeConfig};
use crate::context::ContextFiles;
use crate::extension::mcp::McpClientManager;
use crate::harness::agentic_loop::AgentRuntime;
use crate::messages::{GenerationSettings, MessageError, MessageErrorKind, MessageRequest};
use crate::models::{ModelDescriptor, ModelInput};
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;
use rig::client::{CompletionClient, ModelListingClient};
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Message,
    ProviderCapabilities,
};
use rig::message::{ToolResultContent, UserContent};
use rig::providers::openai::responses_api::{ResponsesCompletionModel, ResponsesToolDefinition};
use rig::providers::{anthropic, deepseek, gemini, ollama, openai, openrouter};
use rig::streaming::StreamingCompletionResponse;
use std::collections::HashMap;

#[derive(Clone)]
pub(crate) struct ResponsesModel(ResponsesCompletionModel<ProviderTransport>);

impl From<ResponsesCompletionModel<ProviderTransport>> for ResponsesModel {
    fn from(model: ResponsesCompletionModel<ProviderTransport>) -> Self {
        Self(model)
    }
}

impl CompletionModel for ResponsesModel {
    async fn completion(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        self.0
            .clone()
            .with_tools(take_responses_tools(&mut request))
            .completion(request)
            .await
    }

    async fn stream(
        &self,
        mut request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        self.0
            .clone()
            .with_tools(take_responses_tools(&mut request))
            .stream(request)
            .await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }
}

pub(super) fn take_responses_tools(
    request: &mut CompletionRequest,
) -> Vec<ResponsesToolDefinition> {
    std::mem::take(&mut request.tools)
        .into_iter()
        .map(|tool| {
            let tool = ResponsesToolDefinition::from(tool);
            if tool.name == "Agent" {
                // Rig skips its false strict field; flattened config preserves explicit opt-out.
                tool.with_config("strict", false.into())
            } else {
                tool
            }
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
pub(crate) enum ProviderHistoryPolicy {
    #[default]
    Native,
    Attached,
    TextOnly,
}

impl ProviderHistoryPolicy {
    pub(crate) fn for_model(api: ProviderApi, input: &[ModelInput]) -> Self {
        if !input.contains(&ModelInput::Image) {
            return Self::TextOnly;
        }
        match api {
            ProviderApi::OpenAiCompletions
            | ProviderApi::DeepSeekChat
            | ProviderApi::OllamaChat => Self::Attached,
            _ => Self::Native,
        }
    }

    pub(crate) fn prepare(self, messages: Vec<Message>) -> Vec<Message> {
        if matches!(self, Self::Native) {
            return messages;
        }
        let mut prepared = Vec::new();
        let mut attachments = Vec::new();
        for mut message in messages {
            let tool_message = matches!(&message, Message::User { content } if !content.is_empty() && content.iter().all(|block| matches!(block, UserContent::ToolResult(_))));
            if !tool_message && !attachments.is_empty() {
                prepared.push(Message::User {
                    content: std::mem::take(&mut attachments),
                });
            }
            if let Message::User { content } = &mut message {
                for block in content {
                    let UserContent::ToolResult(result) = block else {
                        continue;
                    };
                    if !result
                        .content
                        .iter()
                        .any(|block| matches!(block, ToolResultContent::Image(_)))
                    {
                        continue;
                    }
                    if matches!(self, Self::Attached) {
                        attachments.push(UserContent::text(format!(
                            "Images from {} tool result:",
                            result.name
                        )));
                        attachments.extend(result.content.iter().map(|block| match block {
                            ToolResultContent::Image(image) => UserContent::Image(image.clone()),
                            ToolResultContent::Text(text) => UserContent::Text(text.clone()),
                            ToolResultContent::Json { value } => {
                                UserContent::text(value.to_string())
                            }
                        }));
                    }
                    result
                        .content
                        .retain(|block| !matches!(block, ToolResultContent::Image(_)));
                    if result.content.is_empty() {
                        result.content.push(ToolResultContent::text(match self {
                            Self::Attached => "(see attached image)",
                            _ => "(image omitted: model does not accept images)",
                        }));
                    }
                }
            }
            prepared.push(message);
        }
        if !attachments.is_empty() {
            prepared.push(Message::User {
                content: attachments,
            });
        }
        prepared
    }
}

#[derive(Clone)]
pub(crate) struct AnyClient {
    provider_id: String,
    backend: ClientBackend,
}

#[derive(Clone)]
enum ClientBackend {
    OpenRouter(openrouter::Client<ProviderTransport>),
    OpenAI(openai::Client<ProviderTransport>),
    OpenAICompletions(openai::CompletionsClient<ProviderTransport>),
    ChatGPT(auth::ChatGPTClient),
    Anthropic(anthropic::Client<ProviderTransport>),
    Gemini(gemini::Client<ProviderTransport>),
    DeepSeek(deepseek::Client<ProviderTransport>),
    Ollama(ollama::Client<ProviderTransport>),
}

impl AnyClient {
    pub(crate) fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn resolve_model(&self, descriptor: &ModelDescriptor) -> anyhow::Result<ResolvedModel> {
        if crate::models::canonical_provider(&self.provider_id)
            != crate::models::canonical_provider(&descriptor.provider)
        {
            anyhow::bail!(
                "model {}/{} cannot run on provider {}",
                descriptor.provider,
                descriptor.id,
                self.provider_id
            );
        }
        let backend = self.completion_model(descriptor.id.clone());
        if backend.api() != descriptor.api {
            anyhow::bail!(
                "model {}/{} expects {:?}, but provider resolves {:?}",
                descriptor.provider,
                descriptor.id,
                descriptor.api,
                backend.api()
            );
        }
        Ok(ResolvedModel {
            descriptor: descriptor.clone(),
            backend,
        })
    }

    fn completion_model(&self, name: impl Into<String>) -> AnyModel {
        let name = name.into();
        match &self.backend {
            ClientBackend::OpenRouter(client) => {
                AnyModel::OpenRouter(client.completion_model(name))
            }
            ClientBackend::OpenAI(client) => AnyModel::OpenAI(client.completion_model(name).into()),
            ClientBackend::OpenAICompletions(client) => {
                AnyModel::OpenAICompletions(client.completion_model(name))
            }
            ClientBackend::ChatGPT(client) => AnyModel::ChatGPT(client.completion_model(name)),
            ClientBackend::Anthropic(client) => AnyModel::Anthropic(client.completion_model(name)),
            ClientBackend::Gemini(client) => AnyModel::Gemini(client.completion_model(name)),
            ClientBackend::DeepSeek(client) => AnyModel::DeepSeek(client.completion_model(name)),
            ClientBackend::Ollama(client) => AnyModel::Ollama(client.completion_model(name)),
        }
    }

    pub async fn discover_models(
        &self,
        catalog: &crate::models::ModelCatalog,
    ) -> anyhow::Result<crate::models::ModelDiscoveryResult> {
        if catalog.offline() {
            anyhow::bail!("model discovery disabled by PHI_OFFLINE");
        }
        let model_ids = self.list_model_ids().await?;
        catalog.record_discovery(&self.provider_id, model_ids)
    }

    async fn list_model_ids(&self) -> anyhow::Result<Vec<String>> {
        let models = match &self.backend {
            ClientBackend::Ollama(client) => client.list_models().await?,
            ClientBackend::OpenAI(client) if self.provider_id != "openai" => {
                client.list_models().await?
            }
            ClientBackend::OpenAICompletions(client) if self.provider_id != "openai" => {
                client.clone().responses_api().list_models().await?
            }
            _ => anyhow::bail!("model discovery is not supported for {}", self.provider_id),
        };
        Ok(models.iter().map(|model| model.id.clone()).collect())
    }

    pub async fn compress_conversation(
        &self,
        model_name: &str,
        conversation: &str,
        previous_summary: Option<&str>,
        instructions: Option<&str>,
    ) -> anyhow::Result<String> {
        let prompt = prompt::COMPACTION_PROMPT
            .replace("{conversation}", conversation)
            .replace("{previous_summary}", previous_summary.unwrap_or("(none)"))
            .replace("{instructions}", instructions.unwrap_or("(none)"));

        self.completion_model(model_name.to_string())
            .summarize(prompt)
            .await
    }
}

async fn run_summarizer<M>(model: M, prompt: String) -> anyhow::Result<String>
where
    M: CompletionModel + Clone + 'static,
{
    let request = generation::request_builder(
        &model,
        vec![Message::user(prompt)],
        ProviderHistoryPolicy::Native,
    )?
    .preamble("You are a conversation summarizer.".to_string())
    .build();
    let response = generation::PreparedGeneration::new(model, request)?
        .complete()
        .await
        .map_err(|error| anyhow::anyhow!("Compression failed: {error}"))?;
    anyhow::ensure!(
        !response
            .finish_reason()
            .is_some_and(|reason| reason.truncated_output()),
        "Compression returned truncated output"
    );
    let response = generation::text_blocks(&response.choice)
        .collect::<Vec<_>>()
        .join("\n");

    if response.trim().is_empty() {
        anyhow::bail!("Compression returned empty response");
    }

    Ok(response)
}

pub(crate) struct ResolvedModel {
    descriptor: ModelDescriptor,
    backend: AnyModel,
}

impl ResolvedModel {
    pub(super) fn prepare_message(
        self,
        request: &MessageRequest,
        settings: &GenerationSettings,
    ) -> Result<
        futures::future::BoxFuture<'static, Result<StreamingCompletionResponse, CompletionError>>,
        MessageError,
    > {
        let descriptor = &self.descriptor;
        match self.backend {
            AnyModel::OpenRouter(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::OpenAI(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::OpenAICompletions(model) => {
                prepare_message(model, descriptor, request, settings)
            }
            AnyModel::ChatGPT(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::Anthropic(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::Gemini(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::DeepSeek(model) => prepare_message(model, descriptor, request, settings),
            AnyModel::Ollama(model) => prepare_message(model, descriptor, request, settings),
        }
    }
}

fn prepare_message<M: CompletionModel + Clone + Send + 'static>(
    model: M,
    descriptor: &ModelDescriptor,
    request: &MessageRequest,
    settings: &GenerationSettings,
) -> Result<
    futures::future::BoxFuture<'static, Result<StreamingCompletionResponse, CompletionError>>,
    MessageError,
> {
    use crate::messages::{MessageOutput, MessageRole};
    let messages = request
        .messages()
        .iter()
        .map(|message| match message.role() {
            MessageRole::User => Message::user(message.content().to_string()),
            MessageRole::Assistant => Message::assistant(message.content().to_string()),
        })
        .collect();
    let mut builder = generation::request_builder(
        &model,
        messages,
        ProviderHistoryPolicy::for_model(descriptor.api, &descriptor.input),
    )
    .map_err(|error| {
        MessageError::new(
            MessageErrorKind::InvalidRequest,
            "generation request could not be prepared",
        )
        .with_source(error.into())
    })?
    .max_tokens(
        settings
            .max_output_tokens()
            .map(Into::into)
            .unwrap_or(descriptor.max_output_tokens),
    );
    if let Some(instructions) = request.instructions() {
        builder = builder.preamble(instructions.to_string());
    }
    if let Some(temperature) = settings.temperature() {
        builder = builder.temperature(temperature);
    }
    if let Some(parameters) = generation::model_thinking_params(
        descriptor.api,
        descriptor,
        settings.thinking_level().map(|level| level.as_str()),
        None,
    ) {
        builder = builder.additional_params(parameters);
    }
    if let MessageOutput::JsonSchema { schema } = request.output() {
        let schema = serde_json::from_value(schema.clone()).map_err(|error| {
            MessageError::new(
                MessageErrorKind::InvalidRequest,
                "invalid output schema document",
            )
            .with_source(error.into())
        })?;
        builder = builder.output_schema(schema);
    }
    let prepared =
        generation::PreparedGeneration::new(model, builder.build()).map_err(|error| {
            MessageError::new(
                MessageErrorKind::InvalidRequest,
                "generation request could not be prepared",
            )
            .with_source(error.into())
        })?;
    Ok(Box::pin(prepared.start_attempt()))
}

enum AnyModel {
    OpenRouter(openrouter::completion::CompletionModel<ProviderTransport>),
    OpenAI(ResponsesModel),
    OpenAICompletions(openai::completion::CompletionModel<ProviderTransport>),
    ChatGPT(auth::ChatGPTModel),
    Anthropic(anthropic::completion::CompletionModel<ProviderTransport>),
    Gemini(gemini::completion::CompletionModel<ProviderTransport>),
    DeepSeek(deepseek::CompletionModel<ProviderTransport>),
    Ollama(ollama::CompletionModel<ProviderTransport>),
}

impl AnyModel {
    fn api(&self) -> ProviderApi {
        match self {
            Self::OpenRouter(_) | Self::OpenAICompletions(_) => ProviderApi::OpenAiCompletions,
            Self::OpenAI(_) => ProviderApi::OpenAiResponses,
            Self::ChatGPT(_) => ProviderApi::OpenAiCodexResponses,
            Self::Anthropic(_) => ProviderApi::AnthropicMessages,
            Self::Gemini(_) => ProviderApi::GeminiGenerateContent,
            Self::DeepSeek(_) => ProviderApi::DeepSeekChat,
            Self::Ollama(_) => ProviderApi::OllamaChat,
        }
    }

    async fn summarize(self, prompt: String) -> anyhow::Result<String> {
        match self {
            Self::OpenRouter(model) => run_summarizer(model, prompt).await,
            Self::OpenAI(model) => run_summarizer(model, prompt).await,
            Self::OpenAICompletions(model) => run_summarizer(model, prompt).await,
            Self::ChatGPT(model) => run_summarizer(model, prompt).await,
            Self::Anthropic(model) => run_summarizer(model, prompt).await,
            Self::Gemini(model) => run_summarizer(model, prompt).await,
            Self::DeepSeek(model) => run_summarizer(model, prompt).await,
            Self::Ollama(model) => run_summarizer(model, prompt).await,
        }
    }
}

#[derive(Clone)]
pub(crate) enum AnyRuntime {
    OpenRouter(AgentRuntime<openrouter::completion::CompletionModel<ProviderTransport>>),
    OpenAI(AgentRuntime<ResponsesModel>),
    OpenAICompletions(AgentRuntime<openai::completion::CompletionModel<ProviderTransport>>),
    ChatGPT(AgentRuntime<auth::ChatGPTModel>),
    Anthropic(AgentRuntime<anthropic::completion::CompletionModel<ProviderTransport>>),
    Gemini(AgentRuntime<gemini::completion::CompletionModel<ProviderTransport>>),
    DeepSeek(AgentRuntime<deepseek::CompletionModel<ProviderTransport>>),
    Ollama(AgentRuntime<ollama::CompletionModel<ProviderTransport>>),
}

macro_rules! map_runtime_client {
    ($runtime:expr, $client:expr, |$inner:ident, $provider:ident| $body:expr) => {
        match ($runtime, &$client.backend) {
            (AnyRuntime::OpenRouter($inner), ClientBackend::OpenRouter($provider)) => {
                $body.map(AnyRuntime::OpenRouter)
            }
            (AnyRuntime::OpenAI($inner), ClientBackend::OpenAI($provider)) => {
                $body.map(AnyRuntime::OpenAI)
            }
            (
                AnyRuntime::OpenAICompletions($inner),
                ClientBackend::OpenAICompletions($provider),
            ) => $body.map(AnyRuntime::OpenAICompletions),
            (AnyRuntime::ChatGPT($inner), ClientBackend::ChatGPT($provider)) => {
                $body.map(AnyRuntime::ChatGPT)
            }
            (AnyRuntime::Anthropic($inner), ClientBackend::Anthropic($provider)) => {
                $body.map(AnyRuntime::Anthropic)
            }
            (AnyRuntime::Gemini($inner), ClientBackend::Gemini($provider)) => {
                $body.map(AnyRuntime::Gemini)
            }
            (AnyRuntime::DeepSeek($inner), ClientBackend::DeepSeek($provider)) => {
                $body.map(AnyRuntime::DeepSeek)
            }
            (AnyRuntime::Ollama($inner), ClientBackend::Ollama($provider)) => {
                $body.map(AnyRuntime::Ollama)
            }
            _ => anyhow::bail!("agent runtime requires the active provider client"),
        }
    };
}

impl AnyRuntime {
    pub(crate) fn with_model(
        self,
        client: &AnyClient,
        descriptor: Option<&ModelDescriptor>,
        options: &AgentOptions,
        cfg: &RuntimeConfig,
    ) -> anyhow::Result<Self> {
        if let Some(descriptor) = descriptor {
            client.resolve_model(descriptor)?;
        }
        map_runtime_client!(self, client, |runtime, provider| {
            let runtime = if let Some(descriptor) = descriptor {
                runtime
                    .model(provider.completion_model(descriptor.id.clone()).into())
                    .usage_model(&descriptor.provider, &descriptor.id)
                    .model_cost(descriptor.cost.clone())
                    .history_policy(
                        descriptor.api,
                        ProviderHistoryPolicy::for_model(descriptor.api, &descriptor.input),
                    )
                    .max_tokens(options.max_tokens.or(Some(descriptor.max_output_tokens)))
                    .additional_params(builder::model_thinking_params(
                        descriptor.api,
                        descriptor,
                        options,
                        cfg,
                    ))
            } else {
                runtime
            };
            Ok(runtime)
        })
    }
}

pub(crate) fn create_client(
    provider_name: &str,
    api_key: Option<&str>,
    custom_providers: &HashMap<String, CustomProviderConfig>,
) -> anyhow::Result<AnyClient> {
    let provider = resolve_provider(provider_name, custom_providers).ok_or_else(|| {
        anyhow::anyhow!(
            "Unknown provider: {}. Supported providers: openrouter, openai, chatgpt, openai-codex, anthropic, google, deepseek, ollama",
            provider_name
        )
    })?;

    let key = if provider.adapter == ProviderAdapter::ChatGPT {
        String::new()
    } else {
        resolve_api_key(provider.adapter, provider.api_key.as_deref(), api_key)?
    };

    let backend = match (provider.adapter, provider.api) {
        (ProviderAdapter::OpenAI, ProviderApi::OpenAiResponses) => {
            let mut builder = openai::Client::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::OpenAI(builder.build()?)
        }
        (ProviderAdapter::OpenAI, ProviderApi::OpenAiCompletions) => {
            let mut builder = openai::CompletionsClient::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::OpenAICompletions(builder.build()?)
        }
        (ProviderAdapter::ChatGPT, ProviderApi::OpenAiCodexResponses) => {
            ClientBackend::ChatGPT(create_chatgpt_client(provider.id())?)
        }
        (ProviderAdapter::Anthropic, ProviderApi::AnthropicMessages) => {
            let mut builder = anthropic::Client::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::Anthropic(builder.build()?)
        }
        (ProviderAdapter::Gemini, ProviderApi::GeminiGenerateContent) => {
            let mut builder = gemini::Client::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::Gemini(builder.build()?)
        }
        (ProviderAdapter::DeepSeek, ProviderApi::DeepSeekChat) => {
            let mut builder = deepseek::Client::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::DeepSeek(builder.build()?)
        }
        (ProviderAdapter::Ollama, ProviderApi::OllamaChat) => {
            let key: ollama::OllamaApiKey = key.as_str().into();
            let mut builder = ollama::Client::builder()
                .api_key(key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::Ollama(builder.build()?)
        }
        (ProviderAdapter::OpenRouter, ProviderApi::OpenAiCompletions) => {
            let mut builder = openrouter::Client::builder()
                .api_key(&key)
                .http_client(ProviderTransport::default());
            if let Some(base_url) = &provider.base_url {
                builder = builder.base_url(base_url);
            }
            ClientBackend::OpenRouter(builder.build()?)
        }
        _ => anyhow::bail!("provider adapter and API protocol are incompatible"),
    };
    Ok(AnyClient {
        provider_id: provider.id,
        backend,
    })
}

pub(crate) async fn build_runtime(
    model: ResolvedModel,
    options: &AgentOptions,
    cfg: &RuntimeConfig,
    context: &ContextFiles,
    working_dir: &std::path::Path,
    permission: Option<PermissionRuntime>,
    sandbox: Sandbox,
    extensions: Option<&std::sync::Arc<crate::extension::ExtensionRuntime>>,
    tool_declarations: &[crate::tools::ToolDeclaration],
    mcp_manager: Option<&McpClientManager>,
) -> anyhow::Result<AnyRuntime> {
    let ResolvedModel {
        descriptor,
        backend,
    } = model;
    let provider_api = backend.api();
    let build = builder::RuntimeBuildContext {
        descriptor,
        options,
        cfg,
        context,
        working_dir,
        permission,
        sandbox,
        extensions,
        tool_declarations,
        mcp_manager,
    };
    Ok(match backend {
        AnyModel::OpenRouter(model) => {
            AnyRuntime::OpenRouter(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::OpenAI(model) => {
            AnyRuntime::OpenAI(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::OpenAICompletions(model) => AnyRuntime::OpenAICompletions(
            builder::build_runtime_inner(model, provider_api, build).await?,
        ),
        AnyModel::ChatGPT(model) => {
            AnyRuntime::ChatGPT(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::Anthropic(model) => {
            AnyRuntime::Anthropic(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::Gemini(model) => {
            AnyRuntime::Gemini(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::DeepSeek(model) => {
            AnyRuntime::DeepSeek(builder::build_runtime_inner(model, provider_api, build).await?)
        }
        AnyModel::Ollama(model) => {
            AnyRuntime::Ollama(builder::build_runtime_inner(model, provider_api, build).await?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_policy_preserves_tool_result_groups_and_capabilities() {
        let image = rig::message::Image {
            data: rig::message::DocumentSourceKind::Base64("iVBORw0KGgo=".into()),
            media_type: Some(rig::message::ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        };
        let messages = ["first", "second"]
            .into_iter()
            .map(|id| Message::User {
                content: vec![UserContent::tool_result_for(
                    rig::message::ToolCallId::new_or_mint(id),
                    None,
                    "Codemode",
                    vec![ToolResultContent::Image(image.clone())],
                )],
            })
            .chain([Message::assistant("done")])
            .collect::<Vec<_>>();
        for api in [
            ProviderApi::OpenAiCompletions,
            ProviderApi::DeepSeekChat,
            ProviderApi::OllamaChat,
            ProviderApi::OpenAiResponses,
            ProviderApi::OpenAiCodexResponses,
            ProviderApi::AnthropicMessages,
            ProviderApi::GeminiGenerateContent,
        ] {
            for input in [
                vec![ModelInput::Text],
                vec![ModelInput::Text, ModelInput::Image],
            ] {
                let policy = ProviderHistoryPolicy::for_model(api, &input);
                let prepared = policy.prepare(messages.clone());
                let attached = matches!(policy, ProviderHistoryPolicy::Attached);
                assert_eq!(prepared.len(), if attached { 4 } else { 3 });
                for message in &prepared[..2] {
                    let Message::User { content } = message else {
                        panic!("lost result");
                    };
                    let [UserContent::ToolResult(result)] = content.as_slice() else {
                        panic!("lost result");
                    };
                    assert_eq!(
                        result
                            .content
                            .iter()
                            .any(|block| matches!(block, ToolResultContent::Image(_))),
                        matches!(policy, ProviderHistoryPolicy::Native)
                    );
                    assert!(!result.content.is_empty());
                }
                if attached {
                    let Message::User { content } = &prepared[2] else {
                        panic!("missing images");
                    };
                    assert_eq!(
                        content
                            .iter()
                            .filter(|block| matches!(block, UserContent::Image(_)))
                            .count(),
                        2
                    );
                }
                assert!(matches!(prepared.last(), Some(Message::Assistant { .. })));
            }
        }
    }

    fn test_descriptor(provider: &str, api: ProviderApi) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.to_string(),
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api,
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::models::ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: crate::models::ModelCost::default(),
        }
    }

    #[tokio::test]
    async fn responses_sends_explicit_agent_strict_false() {
        use rig::completion::{CompletionModel, ToolDefinition};
        use serde_json::{Value, json};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let providers = HashMap::from([(
            "openai".into(),
            CustomProviderConfig {
                api: ProviderApi::OpenAiResponses,
                base_url: Some(format!("http://{}/v1", listener.local_addr().unwrap())),
                api_key: None,
            },
        )]);
        let client = create_client("openai", Some("test-key"), &providers).unwrap();
        let ClientBackend::OpenAI(client) = client.backend else {
            panic!("expected Responses client");
        };
        let model = ResponsesModel::from(client.completion_model("test-model"));
        use rig::tool::Tool;
        let agent = crate::agent::tools::AgentTool::new(
            tokio::sync::mpsc::unbounded_channel().0,
            &crate::agent::custom::AgentCatalog::new(Vec::new()),
            None,
        );
        let parameters = agent.parameters();
        let request = model
            .completion_request("Inspect.")
            .tools(vec![
                ToolDefinition {
                    name: "Agent".into(),
                    description: agent.description(),
                    parameters: parameters.clone(),
                },
                ToolDefinition {
                    name: "Task".into(),
                    description: "Track work".into(),
                    parameters: json!({"type":"object", "properties":{}}),
                },
            ])
            .build();

        for streaming in [false, true] {
            let capture = async {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut headers = String::new();
                while !headers.ends_with("\r\n\r\n") {
                    assert_ne!(stream.read_line(&mut headers).await.unwrap(), 0);
                }
                let len = headers
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut bytes = vec![0; len];
                stream.read_exact(&mut bytes).await.unwrap();
                stream
                .get_mut()
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            };
            let (_, wire) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let run = async {
                    use futures::StreamExt;
                    if streaming {
                        model.stream(request.clone()).await.unwrap().next().await;
                    } else {
                        let _ = model.completion(request.clone()).await;
                    }
                };
                tokio::join!(run, capture)
            })
            .await
            .unwrap();
            let tools = wire["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 2);
            let tool = |name: &str| tools.iter().find(|tool| tool["name"] == name).unwrap();
            let agent = tool("Agent");
            assert_eq!(agent.get("strict"), Some(&json!(false)));
            let schema = &agent["parameters"];
            assert_eq!(schema, &parameters);
            assert_eq!(
                schema["properties"]["action"]["enum"],
                json!(["create", "list", "message"])
            );
            assert_eq!(schema["required"], json!(["action"]));
            assert!(schema["properties"].get("role").is_some());
            assert!(tool("Task").get("strict").is_none());
        }
    }

    #[test]
    fn model_resolution_rejects_provider_and_protocol_mismatches() {
        let anthropic = test_descriptor("anthropic", ProviderApi::AnthropicMessages);
        let openai = create_client("openai", Some("test-key"), &HashMap::new()).unwrap();
        assert!(openai.resolve_model(&anthropic).is_err());

        let providers = HashMap::from([(
            "openai".to_string(),
            CustomProviderConfig {
                api: ProviderApi::OpenAiCompletions,
                base_url: Some("http://localhost".to_string()),
                api_key: None,
            },
        )]);
        let completions = create_client("openai", Some("test-key"), &providers).unwrap();
        let gpt = test_descriptor("openai", ProviderApi::OpenAiResponses);
        assert!(completions.resolve_model(&gpt).is_err());
    }
}
