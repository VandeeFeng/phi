use super::{ProviderApi, ProviderHistoryPolicy};
use crate::config::OpenAiReasoningConfig;
use crate::models::ModelDescriptor;
use crate::transcript::{ContentBlock, ImageSource, ReasoningMetadata, ReasoningMetadataContent};
use base64::Engine;
use compact_str::CompactString;
use rig::completion::{
    AssistantContent, CompletionError, CompletionModel, CompletionRequest,
    CompletionRequestBuilder, CompletionResponse, Message,
};
use rig::message::{DocumentSourceKind, MimeType, ReasoningContent, ToolCall};
use rig::streaming::StreamingCompletionResponse;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::messages::{GenerationSettings, MessageError, MessageErrorKind, ModelSelection};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationFinish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Refusal,
    Other(String),
    Unknown,
}

impl GenerationFinish {
    pub(crate) fn from_reason(
        reason: Option<&rig::completion::FinishReason>,
        api: Option<ProviderApi>,
    ) -> Self {
        use rig::completion::FinishReason;

        match reason {
            Some(FinishReason::Stop) => Self::Stop,
            Some(FinishReason::Length) => Self::Length,
            Some(FinishReason::ToolCalls) => Self::ToolCalls,
            Some(FinishReason::ContentFilter) => Self::ContentFilter,
            Some(FinishReason::Other(reason))
                if api == Some(ProviderApi::AnthropicMessages) && reason == "refusal" =>
            {
                Self::Refusal
            }
            Some(FinishReason::Other(reason)) => Self::Other(reason.clone()),
            None => Self::Unknown,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_use",
            Self::Length => "length",
            Self::ContentFilter => "content_filter",
            Self::Refusal => "refusal",
            Self::Other(reason) => reason,
            Self::Unknown => "unknown",
        }
    }
}

pub(crate) fn supports_output_schema(descriptor: &ModelDescriptor) -> bool {
    // Only established native adapter/model combinations are admitted initially.
    descriptor.provider == "openai"
        && matches!(
            descriptor.api,
            ProviderApi::OpenAiResponses | ProviderApi::OpenAiCompletions
        )
        && matches!(
            descriptor.id.as_str(),
            "gpt-4o-2024-08-06"
                | "gpt-4o-2024-11-20"
                | "gpt-4o-mini-2024-07-18"
                | "gpt-4.1"
                | "gpt-4.1-2025-04-14"
                | "gpt-4.1-mini"
                | "gpt-4.1-mini-2025-04-14"
                | "gpt-4.1-nano"
                | "gpt-4.1-nano-2025-04-14"
        )
}

pub(crate) fn normalize_tool_call_id(id: &str, provider: &str, api: Option<ProviderApi>) -> String {
    use sha2::{Digest, Sha256};

    const MAX_TOOL_ID_LENGTH: usize = 64;
    const MAX_OPENAI_CHAT_ID_LENGTH: usize = 40;
    const HASH_BYTES: usize = 8;

    let max_length = match api {
        Some(
            ProviderApi::AnthropicMessages
            | ProviderApi::OpenAiResponses
            | ProviderApi::OpenAiCodexResponses,
        ) => MAX_TOOL_ID_LENGTH,
        Some(ProviderApi::OpenAiCompletions) if provider == "openai" || id.contains('|') => {
            MAX_OPENAI_CHAT_ID_LENGTH
        }
        _ => return id.to_owned(),
    };
    let normalized = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized == id && !id.is_empty() && id.len() <= max_length {
        return normalized;
    }
    let hash = Sha256::digest(id.as_bytes())[..HASH_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prefix_length = normalized.len().min(max_length - hash.len() - 1);
    format!("{}_{hash}", &normalized[..prefix_length])
}

pub(crate) struct GenerationCore {
    catalog: Arc<crate::models::CatalogSnapshot>,
    clients: BTreeMap<String, super::AnyClient>,
}

impl std::fmt::Debug for GenerationCore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenerationCore")
            .field("catalog_providers", &self.catalog.providers().len())
            .field("prepared_providers", &self.clients.len())
            .finish_non_exhaustive()
    }
}

impl GenerationCore {
    pub(crate) fn capture(
        phi: &crate::Phi,
        selections: &[ModelSelection],
    ) -> Result<Self, MessageError> {
        let catalog = phi
            .runtime_services
            .resolve_model_catalog(&phi.provider)
            .map_err(|error| {
                MessageError::new(
                    MessageErrorKind::InvalidRequest,
                    "messages preparation requires an injected model catalog",
                )
                .with_source(error)
            })?
            .snapshot();
        // Validate the complete selection set before resolving any credentials.
        for selection in selections {
            let descriptor = catalog
                .get(selection.provider(), selection.model())
                .ok_or_else(|| {
                    MessageError::new(
                        MessageErrorKind::InvalidRequest,
                        "allowed model is absent from the prepared catalog",
                    )
                })?;
            if !descriptor.input.contains(&crate::models::ModelInput::Text) {
                return Err(MessageError::new(
                    MessageErrorKind::UnsupportedCapability,
                    "allowed model does not support text input",
                ));
            }
            let provider = super::resolve_provider(selection.provider(), &phi.cfg.custom_providers)
                .ok_or_else(|| {
                    MessageError::new(
                        MessageErrorKind::UnsupportedCapability,
                        "allowed provider is not configured",
                    )
                })?;
            if provider.api != descriptor.api {
                return Err(MessageError::new(
                    MessageErrorKind::UnsupportedCapability,
                    "allowed model is incompatible with its configured provider",
                ));
            }
        }
        let mut clients = BTreeMap::new();
        for selection in selections {
            if !clients.contains_key(selection.provider()) {
                // A primary-provider override must never leak to another provider.
                let api_key = (selection.provider() == phi.provider)
                    .then_some(phi.api_key.as_deref())
                    .flatten();
                let client =
                    super::create_client(selection.provider(), api_key, &phi.cfg.custom_providers)
                        .map_err(|error| {
                            MessageError::new(
                                MessageErrorKind::Provider,
                                "configured provider client could not be prepared",
                            )
                            .with_source(error)
                        })?;
                clients.insert(selection.provider().to_owned(), client);
            }
            let descriptor = catalog
                .get(selection.provider(), selection.model())
                .expect("all selections were validated against this immutable snapshot");
            clients[selection.provider()]
                .resolve_model(descriptor)
                .map_err(|error| {
                    MessageError::new(
                        MessageErrorKind::UnsupportedCapability,
                        "allowed model is incompatible with its configured provider",
                    )
                    .with_source(error)
                })?;
        }
        Ok(Self { catalog, clients })
    }

    pub(crate) fn descriptor(
        &self,
        selection: &ModelSelection,
    ) -> Result<&ModelDescriptor, MessageError> {
        self.catalog
            .get(selection.provider(), selection.model())
            .ok_or_else(|| {
                MessageError::new(
                    MessageErrorKind::InvalidRequest,
                    "selected model is absent from the prepared catalog",
                )
            })
    }

    pub(crate) fn prepare_message(
        &self,
        selection: &ModelSelection,
        request: &crate::messages::MessageRequest,
        settings: &GenerationSettings,
    ) -> Result<
        futures::future::BoxFuture<'static, Result<StreamingCompletionResponse, CompletionError>>,
        MessageError,
    > {
        let descriptor = self.descriptor(selection)?;
        let client = self.clients.get(selection.provider()).ok_or_else(|| {
            MessageError::new(
                MessageErrorKind::InvalidRequest,
                "selected provider was not prepared",
            )
        })?;
        let resolved = client.resolve_model(descriptor).map_err(|error| {
            MessageError::new(
                MessageErrorKind::UnsupportedCapability,
                "selected model cannot be prepared",
            )
            .with_source(error)
        })?;
        resolved.prepare_message(request, settings)
    }
}

pub(crate) fn serialized_size(value: &impl serde::Serialize, limit: usize) -> Result<usize, ()> {
    struct ByteCounter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other(
                    "serialized value exceeds its byte limit",
                ));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = ByteCounter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|_| ())?;
    Ok(counter.bytes)
}

pub(crate) fn request_builder<M: CompletionModel + Clone>(
    model: &M,
    messages: Vec<Message>,
    history_policy: ProviderHistoryPolicy,
) -> Result<CompletionRequestBuilder<M>, CompletionError> {
    let mut messages = history_policy.prepare(messages);
    let prompt = messages
        .pop()
        .ok_or_else(|| CompletionError::RequestError("generation messages are empty".into()))?;
    Ok(model.completion_request(prompt).messages(messages))
}

pub(crate) struct PreparedGeneration<M: CompletionModel> {
    model: M,
    request: CompletionRequest,
}

impl<M: CompletionModel> PreparedGeneration<M> {
    pub(crate) fn new(model: M, request: CompletionRequest) -> Result<Self, CompletionError> {
        request.validate_message_content()?;
        Ok(Self { model, request })
    }

    pub(crate) async fn complete(self) -> Result<CompletionResponse, CompletionError> {
        self.model.completion(self.request).await
    }

    pub(crate) async fn start_attempt(
        self,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        self.model.stream(self.request).await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("provider image content cannot be represented")]
pub(crate) struct UnsupportedImage;

pub(crate) fn content_block(content: &AssistantContent) -> Result<ContentBlock, UnsupportedImage> {
    Ok(match content {
        AssistantContent::Text(text) => ContentBlock::Text {
            text: CompactString::new(&text.text).into(),
        },
        AssistantContent::Reasoning(reasoning) => ContentBlock::Thinking {
            text: CompactString::new(reasoning.display_text()).into(),
            metadata: Some(ReasoningMetadata {
                id: reasoning.id.as_deref().map(CompactString::new),
                content: reasoning
                    .content
                    .iter()
                    .map(reasoning_metadata_content)
                    .collect(),
            }),
        },
        AssistantContent::ToolCall(call) => tool_call_content(call),
        AssistantContent::Image(image) => ContentBlock::Image {
            source: image_source(image)?,
        },
    })
}

pub(crate) fn tool_item_id(call: &ToolCall) -> &str {
    call.provider
        .as_ref()
        .and_then(|provider| provider.item_id.as_deref())
        .unwrap_or(call.id.as_str())
}

pub(crate) fn tool_call_content(call: &ToolCall) -> ContentBlock {
    let provider = call.provider.as_ref();
    ContentBlock::ToolCall {
        id: CompactString::new(tool_item_id(call)),
        provider_call_id: provider.map(|provider| CompactString::new(&provider.call_id)),
        provider_item_id: provider
            .and_then(|provider| provider.item_id.as_deref())
            .map(CompactString::new),
        name: CompactString::new(&call.function.name),
        args: call.function.arguments.clone(),
    }
}

pub(crate) fn image_source(image: &rig::message::Image) -> Result<ImageSource, UnsupportedImage> {
    let mime_type = image
        .media_type
        .as_ref()
        .map(|media_type| CompactString::new(media_type.to_mime_type()));
    match &image.data {
        DocumentSourceKind::Url(url) => {
            Ok(
                data_url_image_source(url).unwrap_or_else(|| ImageSource::Url {
                    url: CompactString::new(url),
                    mime_type,
                }),
            )
        }
        DocumentSourceKind::Base64(data) => Ok(ImageSource::Base64 {
            data: CompactString::new(data),
            mime_type: mime_type.ok_or(UnsupportedImage)?,
        }),
        DocumentSourceKind::Raw(data) => Ok(ImageSource::Base64 {
            data: CompactString::new(base64::engine::general_purpose::STANDARD.encode(data)),
            mime_type: mime_type.ok_or(UnsupportedImage)?,
        }),
        DocumentSourceKind::String(value) => data_url_image_source(value).ok_or(UnsupportedImage),
        _ => Err(UnsupportedImage),
    }
}

fn data_url_image_source(url: &str) -> Option<ImageSource> {
    let encoded = url.strip_prefix("data:")?;
    let (metadata, data) = encoded.split_once(',')?;
    let (mime_type, encoding) = metadata.split_once(';')?;
    if encoding != "base64" || !mime_type.starts_with("image/") {
        return None;
    }
    Some(ImageSource::Base64 {
        data: CompactString::new(data),
        mime_type: CompactString::new(mime_type),
    })
}

fn reasoning_metadata_content(content: &ReasoningContent) -> ReasoningMetadataContent {
    match content {
        ReasoningContent::Text { text, signature } => ReasoningMetadataContent::Text {
            text: CompactString::new(text),
            signature: signature.as_deref().map(CompactString::new),
        },
        ReasoningContent::Encrypted(data) => ReasoningMetadataContent::Encrypted {
            data: CompactString::new(data),
        },
        ReasoningContent::Redacted { data } => ReasoningMetadataContent::Redacted {
            data: CompactString::new(data),
        },
        ReasoningContent::Summary(text) => ReasoningMetadataContent::Summary {
            text: CompactString::new(text),
        },
    }
}

pub(crate) fn text_blocks(choice: &[AssistantContent]) -> impl Iterator<Item = &str> {
    choice.iter().filter_map(|content| match content {
        AssistantContent::Text(text) => Some(text.text.as_str()),
        _ => None,
    })
}

pub(crate) fn model_thinking_params(
    api: ProviderApi,
    descriptor: &ModelDescriptor,
    requested_level: Option<&str>,
    openai: Option<&OpenAiReasoningConfig>,
) -> Option<serde_json::Value> {
    let level = requested_level.map(|level| descriptor.resolve_thinking_level(level));
    let openai = api.supports_openai_reasoning().then_some(openai).flatten();
    thinking_params(api, level.as_deref(), openai)
}

fn thinking_params(
    api: ProviderApi,
    level: Option<&str>,
    openai: Option<&OpenAiReasoningConfig>,
) -> Option<serde_json::Value> {
    match api {
        ProviderApi::OpenAiResponses | ProviderApi::OpenAiCodexResponses => {
            openai_thinking_params(level, openai)
        }
        ProviderApi::OpenAiCompletions | ProviderApi::OllamaChat => {
            openai_thinking_params(level, None)
        }
        ProviderApi::AnthropicMessages => level.and_then(anthropic_thinking_params),
        ProviderApi::GeminiGenerateContent => level.and_then(gemini_thinking_params),
        ProviderApi::DeepSeekChat => level.map(deepseek_thinking_params),
    }
}

fn openai_thinking_params(
    level: Option<&str>,
    config: Option<&OpenAiReasoningConfig>,
) -> Option<serde_json::Value> {
    let mut reasoning = config
        .and_then(|config| serde_json::to_value(config).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(level) = level {
        let effort = if level == "off" { "none" } else { level };
        reasoning.insert("effort".to_string(), effort.into());
    }
    (!reasoning.is_empty()).then(|| serde_json::json!({ "reasoning": reasoning }))
}

fn anthropic_thinking_params(level: &str) -> Option<serde_json::Value> {
    if level == "off" {
        return Some(serde_json::json!({ "thinking": { "type": "disabled" } }));
    }
    let effort = if level == "minimal" { "low" } else { level };
    Some(serde_json::json!({
        "thinking": { "type": "adaptive" },
        "output_config": { "effort": effort }
    }))
}

fn gemini_thinking_params(level: &str) -> Option<serde_json::Value> {
    let thinking_config = if level == "off" {
        serde_json::json!({ "thinkingBudget": 0 })
    } else {
        serde_json::json!({ "thinkingLevel": level, "includeThoughts": true })
    };
    Some(serde_json::json!({
        "generationConfig": { "thinkingConfig": thinking_config }
    }))
}

fn deepseek_thinking_params(level: &str) -> serde_json::Value {
    if level.eq_ignore_ascii_case("off") || level.eq_ignore_ascii_case("none") {
        return serde_json::json!({ "thinking": { "type": "disabled" } });
    }
    serde_json::json!({
        "thinking": { "type": "enabled" },
        "reasoning_effort": level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn finish_mapping_preserves_termination_and_wire_spelling() {
        use rig::completion::FinishReason;

        for (reason, expected, spelling) in [
            (Some(FinishReason::Stop), GenerationFinish::Stop, "stop"),
            (
                Some(FinishReason::ToolCalls),
                GenerationFinish::ToolCalls,
                "tool_use",
            ),
            (
                Some(FinishReason::Length),
                GenerationFinish::Length,
                "length",
            ),
            (
                Some(FinishReason::ContentFilter),
                GenerationFinish::ContentFilter,
                "content_filter",
            ),
            (
                Some(FinishReason::Other("stop".into())),
                GenerationFinish::Other("stop".into()),
                "stop",
            ),
            (None, GenerationFinish::Unknown, "unknown"),
        ] {
            let normalized = GenerationFinish::from_reason(reason.as_ref(), None);
            assert_eq!(normalized, expected);
            assert_eq!(normalized.as_str(), spelling);
        }
        let refusal = rig::completion::FinishReason::Other("refusal".into());
        assert_eq!(
            GenerationFinish::from_reason(Some(&refusal), Some(ProviderApi::AnthropicMessages)),
            GenerationFinish::Refusal,
        );
        assert_eq!(
            GenerationFinish::from_reason(Some(&refusal), Some(ProviderApi::OpenAiResponses)),
            GenerationFinish::Other("refusal".into()),
        );
    }

    #[test]
    fn tool_id_rules_preserve_unconstrained_protocols() {
        let id = format!("read:file/{}", "x".repeat(100));
        for (provider, api) in [
            ("custom", Some(ProviderApi::OpenAiCompletions)),
            ("gemini", Some(ProviderApi::GeminiGenerateContent)),
            ("deepseek", Some(ProviderApi::DeepSeekChat)),
            ("ollama", Some(ProviderApi::OllamaChat)),
            ("unknown", None),
        ] {
            assert_eq!(normalize_tool_call_id(&id, provider, api), id);
        }
        let composite = format!("call|{id}");
        let normalized =
            normalize_tool_call_id(&composite, "custom", Some(ProviderApi::OpenAiCompletions));
        assert!(normalized.len() <= 40);
        assert!(
            normalized
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
    }

    #[derive(Clone, Default)]
    struct AttemptProbe {
        attempts: Arc<AtomicUsize>,
    }

    impl CompletionModel for AttemptProbe {
        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, CompletionError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(CompletionError::ProviderError("probe failure".into()))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse, CompletionError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(CompletionError::ProviderError("probe failure".into()))
        }
    }

    #[tokio::test]
    async fn preparation_is_pure_and_dispatch_never_retries() {
        let model = AttemptProbe::default();
        let prepare = || {
            let request = request_builder(
                &model,
                vec![Message::user("explicit input")],
                ProviderHistoryPolicy::Native,
            )
            .unwrap()
            .build();
            PreparedGeneration::new(model.clone(), request).unwrap()
        };
        let completion = prepare();
        let stream = prepare();
        assert_eq!(model.attempts.load(Ordering::SeqCst), 0);
        assert!(matches!(
            completion.complete().await,
            Err(CompletionError::ProviderError(_))
        ));
        assert_eq!(model.attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            stream.start_attempt().await,
            Err(CompletionError::ProviderError(_))
        ));
        assert_eq!(model.attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn preparation_rejects_empty_context_before_dispatch() {
        let model = AttemptProbe::default();
        assert!(matches!(
            request_builder(&model, Vec::new(), ProviderHistoryPolicy::Native),
            Err(CompletionError::RequestError(_))
        ));
        for message in [
            Message::User {
                content: Vec::new(),
            },
            Message::Assistant {
                id: None,
                content: Vec::new(),
            },
        ] {
            let request = request_builder(&model, vec![message], ProviderHistoryPolicy::Native)
                .unwrap()
                .build();
            assert!(matches!(
                PreparedGeneration::new(model.clone(), request),
                Err(CompletionError::RequestError(_))
            ));
        }
        assert_eq!(model.attempts.load(Ordering::SeqCst), 0);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn preparation_preserves_explicit_text(
            history in prop::collection::vec((any::<bool>(), ".{1,128}"), 1..16),
        ) {
            let model = AttemptProbe::default();
            let messages = history.into_iter().map(|(assistant, text)| {
                if assistant { Message::assistant(text) } else { Message::user(text) }
            }).collect::<Vec<_>>();
            for policy in [
                ProviderHistoryPolicy::Native,
                ProviderHistoryPolicy::Attached,
                ProviderHistoryPolicy::TextOnly,
            ] {
                let request = request_builder(&model, messages.clone(), policy).unwrap().build();
                let prepared = PreparedGeneration::new(model.clone(), request).unwrap();
                prop_assert_eq!(&prepared.request.chat_history, &messages);
            }
            prop_assert_eq!(model.attempts.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn unsupported_images_remain_conversion_failures() {
        for source in [
            DocumentSourceKind::String("not an image".into()),
            DocumentSourceKind::Base64("aW1hZ2U=".into()),
            DocumentSourceKind::Raw(vec![1, 2, 3]),
        ] {
            let image = rig::message::Image {
                data: source,
                media_type: None,
                detail: None,
                additional_params: None,
            };
            assert!(content_block(&AssistantContent::Image(image)).is_err());
        }
    }

    #[test]
    fn builds_provider_specific_thinking_params() {
        assert_eq!(
            thinking_params(ProviderApi::OpenAiResponses, Some("xhigh"), None),
            Some(serde_json::json!({ "reasoning": { "effort": "xhigh" } }))
        );
        assert_eq!(
            thinking_params(ProviderApi::OpenAiResponses, Some("off"), None),
            Some(serde_json::json!({ "reasoning": { "effort": "none" } }))
        );
        assert_eq!(
            thinking_params(ProviderApi::AnthropicMessages, Some("minimal"), None),
            Some(serde_json::json!({
                "thinking": { "type": "adaptive" },
                "output_config": { "effort": "low" }
            }))
        );
        assert_eq!(
            thinking_params(ProviderApi::GeminiGenerateContent, Some("off"), None),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingBudget": 0 } }
            }))
        );
    }

    #[test]
    fn composes_openai_reasoning_params() {
        let config = OpenAiReasoningConfig {
            effort: Some(crate::config::OpenAiReasoningEffort::Max),
            mode: Some(crate::config::OpenAiReasoningMode::Pro),
            context: Some(crate::config::OpenAiReasoningContext::AllTurns),
            summary: Some(crate::config::OpenAiReasoningSummary::Detailed),
        };

        assert_eq!(
            thinking_params(ProviderApi::OpenAiResponses, None, Some(&config)),
            Some(serde_json::json!({
                "reasoning": {
                    "effort": "max",
                    "mode": "pro",
                    "context": "all_turns",
                    "summary": "detailed"
                }
            }))
        );
        assert_eq!(
            thinking_params(ProviderApi::OpenAiResponses, Some("low"), Some(&config)),
            Some(serde_json::json!({
                "reasoning": {
                    "effort": "low",
                    "mode": "pro",
                    "context": "all_turns",
                    "summary": "detailed"
                }
            }))
        );
    }
}
