//! Explicit-context model generation contracts and trusted capability preparation.

use crate::models::{ModelDescriptor, ThinkingLevel};
pub use crate::provider::generation::GenerationFinish;
use crate::provider::generation::{self, GenerationCore};
use crate::transcript::{ContentBlock, SharedText};
use futures::StreamExt;
use rig::completion::{CompletionError, FinishReason};
use rig::streaming::{StreamedAssistantContent, StreamingCompletionResponse};
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelSelection {
    provider: String,
    model: String,
}

impl ModelSelection {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, MessageError> {
        let selection = Self {
            provider: provider.into(),
            model: model.into(),
        };
        if selection.provider.trim().is_empty() || selection.model.trim().is_empty() {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "provider and model must be nonempty",
            ));
        }
        Ok(selection)
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    role: MessageRole,
    content: SharedText,
}

impl InputMessage {
    pub fn new(role: MessageRole, content: impl Into<SharedText>) -> Result<Self, MessageError> {
        let content = content.into();
        if content.is_empty() {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "message content must be nonempty",
            ));
        }
        Ok(Self { role, content })
    }

    pub fn role(&self) -> MessageRole {
        self.role
    }

    pub fn content(&self) -> &SharedText {
        &self.content
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerationSettings {
    max_output_tokens: Option<NonZeroU64>,
    temperature: Option<f64>,
    thinking_level: Option<ThinkingLevel>,
}

impl GenerationSettings {
    pub fn with_max_output_tokens(mut self, limit: NonZeroU64) -> Self {
        self.max_output_tokens = Some(limit);
        self
    }

    pub fn with_temperature(mut self, temperature: f64) -> Result<Self, MessageError> {
        if !temperature.is_finite() {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "temperature must be finite",
            ));
        }
        self.temperature = Some(temperature);
        Ok(self)
    }

    pub fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = Some(level);
        self
    }

    pub fn max_output_tokens(&self) -> Option<NonZeroU64> {
        self.max_output_tokens
    }

    pub fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.thinking_level
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageOutput {
    Text,
    JsonSchema { schema: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageRequest {
    model: Option<ModelSelection>,
    instructions: Option<SharedText>,
    messages: Vec<InputMessage>,
    settings: GenerationSettings,
    output: MessageOutput,
}

impl MessageRequest {
    pub fn new(messages: Vec<InputMessage>) -> Result<Self, MessageError> {
        if messages.is_empty() {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "messages must be nonempty",
            ));
        }
        Ok(Self {
            model: None,
            instructions: None,
            messages,
            settings: GenerationSettings::default(),
            output: MessageOutput::Text,
        })
    }

    pub fn with_model(mut self, model: ModelSelection) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_instructions(mut self, instructions: impl Into<SharedText>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    pub fn with_settings(mut self, settings: GenerationSettings) -> Self {
        self.settings = settings;
        self
    }

    pub fn with_output(mut self, output: MessageOutput) -> Self {
        self.output = output;
        self
    }

    pub fn model(&self) -> Option<&ModelSelection> {
        self.model.as_ref()
    }

    pub fn instructions(&self) -> Option<&SharedText> {
        self.instructions.as_ref()
    }

    pub fn messages(&self) -> &[InputMessage] {
        &self.messages
    }

    pub fn settings(&self) -> &GenerationSettings {
        &self.settings
    }

    pub fn output(&self) -> &MessageOutput {
        &self.output
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLimits {
    max_input_bytes: NonZeroUsize,
    max_output_bytes: NonZeroUsize,
    max_response_bytes: NonZeroUsize,
    max_schema_bytes: NonZeroUsize,
    max_schema_nodes: NonZeroUsize,
    max_schema_depth: NonZeroUsize,
    max_concurrent_generations: NonZeroUsize,
    max_duration: Duration,
}

impl MessageLimits {
    pub fn new(
        max_input_bytes: NonZeroUsize,
        max_output_bytes: NonZeroUsize,
        max_response_bytes: NonZeroUsize,
        max_schema_bytes: NonZeroUsize,
        max_schema_nodes: NonZeroUsize,
        max_schema_depth: NonZeroUsize,
        max_concurrent_generations: NonZeroUsize,
        max_duration: Duration,
    ) -> Result<Self, MessageError> {
        if max_duration.is_zero()
            || std::time::Instant::now()
                .checked_add(max_duration)
                .is_none()
            || max_concurrent_generations.get() > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "generation limits exceed supported duration or concurrency bounds",
            ));
        }
        Ok(Self {
            max_input_bytes,
            max_output_bytes,
            max_response_bytes,
            max_schema_bytes,
            max_schema_nodes,
            max_schema_depth,
            max_concurrent_generations,
            max_duration,
        })
    }

    pub fn max_input_bytes(&self) -> NonZeroUsize {
        self.max_input_bytes
    }

    pub fn max_output_bytes(&self) -> NonZeroUsize {
        self.max_output_bytes
    }

    /// Maximum response entity bytes, including stream framing and error bodies.
    pub fn max_response_bytes(&self) -> NonZeroUsize {
        self.max_response_bytes
    }

    pub fn max_schema_bytes(&self) -> NonZeroUsize {
        self.max_schema_bytes
    }

    pub fn max_schema_nodes(&self) -> NonZeroUsize {
        self.max_schema_nodes
    }

    pub fn max_schema_depth(&self) -> NonZeroUsize {
        self.max_schema_depth
    }

    pub fn max_concurrent_generations(&self) -> NonZeroUsize {
        self.max_concurrent_generations
    }

    pub fn max_duration(&self) -> Duration {
        self.max_duration
    }
}

#[derive(Debug, Clone)]
pub struct MessagesOptions {
    allowed_models: Vec<ModelSelection>,
    default_model: Option<ModelSelection>,
    defaults: GenerationSettings,
    limits: MessageLimits,
}

impl MessagesOptions {
    pub fn new(
        allowed_models: Vec<ModelSelection>,
        limits: MessageLimits,
    ) -> Result<Self, MessageError> {
        if allowed_models.is_empty() {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "allowed models must be nonempty",
            ));
        }
        Ok(Self {
            allowed_models,
            default_model: None,
            defaults: GenerationSettings::default(),
            limits,
        })
    }

    pub fn with_default_model(mut self, model: ModelSelection) -> Self {
        self.default_model = Some(model);
        self
    }

    pub fn with_defaults(mut self, defaults: GenerationSettings) -> Self {
        self.defaults = defaults;
        self
    }
}

#[derive(Debug)]
struct MessagePolicy {
    allowed_models: Vec<ModelSelection>,
    default_model: Option<ModelSelection>,
    defaults: GenerationSettings,
    limits: MessageLimits,
}

/// A prepared model capability with no Agent identity or conversation history.
#[derive(Clone)]
pub struct Messages {
    core: Arc<GenerationCore>,
    permits: Arc<tokio::sync::Semaphore>,
    policy: Arc<MessagePolicy>,
}

impl std::fmt::Debug for Messages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Messages")
            .field("core", &self.core)
            .field("policy", &self.policy)
            .finish()
    }
}

impl Messages {
    pub(crate) fn prepare(
        phi: &crate::Phi,
        options: MessagesOptions,
    ) -> Result<Self, MessageError> {
        let MessagesOptions {
            mut allowed_models,
            default_model,
            defaults,
            limits,
        } = options;
        allowed_models.sort();
        allowed_models.dedup();
        if default_model
            .as_ref()
            .is_some_and(|model| !allowed_models.contains(model))
        {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "default model must belong to allowed models",
            ));
        }
        let core = GenerationCore::capture(phi, &allowed_models)?;
        for model in &allowed_models {
            resolve_settings(
                &GenerationSettings::default(),
                &defaults,
                core.descriptor(model)?,
            )?;
        }
        Ok(Self {
            core: Arc::new(core),
            permits: Arc::new(tokio::sync::Semaphore::new(
                limits.max_concurrent_generations.get(),
            )),
            policy: Arc::new(MessagePolicy {
                allowed_models,
                default_model,
                defaults,
                limits,
            }),
        })
    }

    pub async fn create(&self, request: MessageRequest) -> Result<MessageResponse, MessageError> {
        self.create_controlled(request, CancellationToken::new())
            .await
    }

    async fn create_controlled(
        &self,
        request: MessageRequest,
        cancellation: CancellationToken,
    ) -> Result<MessageResponse, MessageError> {
        let generation_id = GenerationId::new();
        let deadline = Instant::now() + self.limits().max_duration;
        let scope =
            crate::provider::transport::ResponseScope::new(self.limits().max_response_bytes.get());
        let result = scope
            .run(execute(
                self,
                request,
                cancellation,
                deadline,
                generation_id,
            ))
            .await;
        let result = match (result, scope.exceeded_status()) {
            (Err(mut error), Some(status))
                if !matches!(error.kind, MessageErrorKind::Cancelled) =>
            {
                error.kind = MessageErrorKind::LimitExceeded;
                error.message = "provider response exceeds its byte limit";
                error.provider_status = status;
                Err(error)
            }
            (Ok(response), Some(status)) => {
                let mut error = with_report(
                    MessageError::new(
                        MessageErrorKind::LimitExceeded,
                        "provider response exceeds its byte limit",
                    ),
                    response.report,
                );
                error.provider_status = status;
                Err(error)
            }
            (result, _) => result,
        };
        result.map_err(|mut error| {
            error.generation_id = Some(generation_id);
            error
        })
    }

    pub fn allowed_models(&self) -> &[ModelSelection] {
        &self.policy.allowed_models
    }

    pub fn default_model(&self) -> Option<&ModelSelection> {
        self.policy.default_model.as_ref()
    }

    pub fn defaults(&self) -> &GenerationSettings {
        &self.policy.defaults
    }

    pub fn limits(&self) -> &MessageLimits {
        &self.policy.limits
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationId(uuid::Uuid);

impl GenerationId {
    fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenerationAttemptId(uuid::Uuid);

impl std::fmt::Display for GenerationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::fmt::Display for GenerationAttemptId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationCompletion {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct GenerationIdentity {
    generation_id: GenerationId,
    attempt_id: GenerationAttemptId,
    provider_message_id: Option<String>,
    provider_response_id: Option<String>,
    provider_request_id: Option<String>,
}

impl GenerationIdentity {
    pub fn generation_id(&self) -> GenerationId {
        self.generation_id
    }

    pub fn attempt_id(&self) -> GenerationAttemptId {
        self.attempt_id
    }

    pub fn provider_message_id(&self) -> Option<&str> {
        self.provider_message_id.as_deref()
    }

    pub fn provider_response_id(&self) -> Option<&str> {
        self.provider_response_id.as_deref()
    }

    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
}

#[derive(Debug, Clone)]
pub struct GenerationModel {
    requested: Option<ModelSelection>,
    selected: ModelSelection,
    reported_model: Option<String>,
}

impl GenerationModel {
    pub fn requested(&self) -> Option<&ModelSelection> {
        self.requested.as_ref()
    }

    pub fn selected(&self) -> &ModelSelection {
        &self.selected
    }

    pub fn reported_model(&self) -> Option<&str> {
        self.reported_model.as_deref()
    }
}

#[derive(Debug, Clone, Default)]
pub struct GenerationUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    estimated_cost_usd: Option<f64>,
}

impl GenerationUsage {
    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub fn total_tokens(&self) -> Option<u64> {
        self.total_tokens
    }

    pub fn cache_read_tokens(&self) -> Option<u64> {
        self.cache_read_tokens
    }

    pub fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    pub fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    pub fn estimated_cost_usd(&self) -> Option<f64> {
        self.estimated_cost_usd
    }
}

#[derive(Debug, Clone)]
pub struct GenerationReport {
    identity: GenerationIdentity,
    model: GenerationModel,
    completion: GenerationCompletion,
    content: Vec<ContentBlock>,
    finish_reason: GenerationFinish,
    usage: GenerationUsage,
}

impl GenerationReport {
    pub fn identity(&self) -> &GenerationIdentity {
        &self.identity
    }

    pub fn model(&self) -> &GenerationModel {
        &self.model
    }

    pub fn completion(&self) -> GenerationCompletion {
        self.completion
    }

    pub fn content(&self) -> &[ContentBlock] {
        &self.content
    }

    pub fn finish_reason(&self) -> &GenerationFinish {
        &self.finish_reason
    }

    pub fn usage(&self) -> &GenerationUsage {
        &self.usage
    }
}

#[derive(Debug, Clone)]
pub struct MessageResponse {
    report: GenerationReport,
    parsed_json: Option<serde_json::Value>,
}

impl MessageResponse {
    pub fn report(&self) -> &GenerationReport {
        &self.report
    }

    pub fn parsed_json(&self) -> Option<&serde_json::Value> {
        self.parsed_json.as_ref()
    }

    pub fn text(&self) -> String {
        self.report
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_ref()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageErrorKind {
    InvalidRequest,
    UnsupportedCapability,
    Unauthorized,
    Provider,
    UnsupportedOutput,
    OutputValidation,
    LimitExceeded,
    Cancelled,
    Coordination,
}

#[derive(thiserror::Error)]
#[error("{message}")]
pub struct MessageError {
    generation_id: Option<GenerationId>,
    provider_status: Option<u16>,
    report: Option<Box<GenerationReport>>,
    kind: MessageErrorKind,
    message: &'static str,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl std::fmt::Debug for MessageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MessageError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

impl MessageError {
    pub(crate) fn new(kind: MessageErrorKind, message: &'static str) -> Self {
        Self {
            generation_id: None,
            provider_status: None,
            report: None,
            kind,
            message,
            source: None,
        }
    }

    pub(crate) fn with_source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source.into_boxed_dyn_error());
        self
    }

    pub fn generation_id(&self) -> Option<GenerationId> {
        self.generation_id
    }

    pub fn provider_status(&self) -> Option<u16> {
        self.provider_status
    }

    pub fn report(&self) -> Option<&GenerationReport> {
        self.report.as_deref()
    }

    pub fn kind(&self) -> MessageErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        self.message
    }
}

async fn execute(
    capability: &Messages,
    request: MessageRequest,
    cancellation: CancellationToken,
    deadline: Instant,
    generation_id: GenerationId,
) -> Result<MessageResponse, MessageError> {
    check_control(&cancellation, deadline)?;
    let selection = request
        .model
        .as_ref()
        .or(capability.default_model())
        .ok_or_else(|| {
            MessageError::new(
                MessageErrorKind::InvalidRequest,
                "a model selection is required",
            )
        })?;
    if !capability.allowed_models().contains(selection) {
        return Err(MessageError::new(
            MessageErrorKind::InvalidRequest,
            "model selection is not allowed",
        ));
    }
    let descriptor = capability.core.descriptor(selection)?;
    let settings = resolve_settings(&request.settings, capability.defaults(), descriptor)?;
    let input_bytes = request.messages.iter().try_fold(
        request.instructions.as_ref().map_or(0, |text| text.len()),
        |total, message| total.checked_add(message.content.len()),
    );
    if input_bytes.is_none_or(|bytes| bytes > capability.limits().max_input_bytes.get()) {
        return Err(MessageError::new(
            MessageErrorKind::LimitExceeded,
            "generation input exceeds its byte limit",
        ));
    }
    if request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Assistant)
    {
        return Err(MessageError::new(
            MessageErrorKind::UnsupportedCapability,
            "assistant prefill is not supported",
        ));
    }
    let schema = match &request.output {
        MessageOutput::Text => None,
        MessageOutput::JsonSchema { schema } => {
            Some(compile_schema(schema, descriptor, capability.limits())?)
        }
    };
    let attempt = capability
        .core
        .prepare_message(selection, &request, &settings)?;
    check_control(&cancellation, deadline)?;
    let _permit = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(cancelled()),
        _ = tokio::time::sleep_until(deadline) => return Err(timed_out()),
        permit = capability.permits.acquire() => permit.map_err(|_| MessageError::new(MessageErrorKind::Coordination, "generation admission is closed"))?,
    };
    check_control(&cancellation, deadline)?;
    let mut attempt_id = None;
    let opened = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(cancelled()),
        _ = tokio::time::sleep_until(deadline) => Err(timed_out()),
        stream = async {
            attempt_id = Some(GenerationAttemptId(uuid::Uuid::new_v4()));
            attempt.await
        } => stream.map_err(provider_error),
    };
    let Some(attempt_id) = attempt_id else {
        return Err(opened.err().unwrap_or_else(|| {
            MessageError::new(
                MessageErrorKind::Coordination,
                "generation opened without an attempt",
            )
        }));
    };
    let report = GenerationReport {
        identity: GenerationIdentity {
            generation_id,
            attempt_id,
            provider_message_id: None,
            provider_response_id: None,
            provider_request_id: None,
        },
        model: GenerationModel {
            requested: request.model.clone(),
            selected: selection.clone(),
            reported_model: None,
        },
        completion: GenerationCompletion::Incomplete,
        content: Vec::new(),
        finish_reason: GenerationFinish::Unknown,
        usage: GenerationUsage::default(),
    };
    let stream = opened.map_err(|error| with_report(error, report.clone()))?;
    let report = collect_generation(
        stream,
        report,
        capability.limits(),
        descriptor,
        &cancellation,
        deadline,
    )
    .await?;
    check_control(&cancellation, deadline).map_err(|error| with_report(error, report.clone()))?;
    let response = validate_output(report, schema)?;
    check_control(&cancellation, deadline)
        .map_err(|error| with_report(error, response.report.clone()))?;
    Ok(response)
}

async fn collect_generation(
    mut stream: StreamingCompletionResponse,
    mut report: GenerationReport,
    limits: &MessageLimits,
    descriptor: &ModelDescriptor,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<GenerationReport, MessageError> {
    let collection = collect(
        &mut stream,
        &mut report,
        limits,
        descriptor,
        cancellation,
        deadline,
    )
    .await;
    // Aborting Rig finalizes its accumulator without polling the provider again.
    stream.cancel();
    while stream.next().await.is_some() {}
    let mut failure = collection.err();
    let limit = limits.max_output_bytes.get();
    let mut retained_bytes = 2usize.saturating_add(
        metadata_bytes(
            [
                report.identity.provider_message_id.as_deref(),
                report.identity.provider_response_id.as_deref(),
                report.identity.provider_request_id.as_deref(),
                report.model.reported_model.as_deref(),
                match &report.finish_reason {
                    GenerationFinish::Other(reason) => Some(reason.as_str()),
                    _ => None,
                },
            ],
            limit,
        )
        .unwrap_or(limit),
    );
    if report.identity.provider_message_id.is_none() {
        if let Some(message_id) = stream.message_id.take() {
            if let Ok(bytes) =
                generation::serialized_size(&message_id, limit.saturating_sub(retained_bytes))
            {
                retained_bytes += bytes;
                report.identity.provider_message_id = Some(message_id);
            } else {
                failure.get_or_insert_with(output_exceeded);
            }
        }
    }
    for content in &stream.choice {
        let remaining = limit.saturating_sub(retained_bytes.saturating_add(1));
        let block = match content {
            rig::completion::AssistantContent::Text(text) => {
                if text.additional_params.is_some() {
                    failure.get_or_insert_with(|| {
                        MessageError::new(
                            MessageErrorKind::UnsupportedOutput,
                            "provider produced unsupported text metadata",
                        )
                    });
                }
                let empty_bytes =
                    generation::serialized_size(&ContentBlock::Text { text: "".into() }, remaining)
                        .unwrap_or(remaining.saturating_add(1));
                if empty_bytes > remaining {
                    failure.get_or_insert_with(output_exceeded);
                    break;
                }
                let text_budget = remaining - empty_bytes + 2;
                let mut end = text.text.len().min(text_budget);
                while !text.text.is_char_boundary(end) {
                    end -= 1;
                }
                while generation::serialized_size(&&text.text[..end], text_budget).is_err() {
                    end /= 2;
                    while !text.text.is_char_boundary(end) {
                        end -= 1;
                    }
                }
                if end < text.text.len() {
                    failure.get_or_insert_with(output_exceeded);
                }
                ContentBlock::Text {
                    text: text.text[..end].into(),
                }
            }
            _ => {
                if generation::serialized_size(content, remaining).is_err() {
                    failure.get_or_insert_with(output_exceeded);
                    break;
                }
                match generation::content_block(content) {
                    Ok(block) => block,
                    Err(_) => {
                        failure.get_or_insert_with(|| {
                            MessageError::new(
                                MessageErrorKind::UnsupportedOutput,
                                "provider output cannot be represented",
                            )
                        });
                        break;
                    }
                }
            }
        };
        let Ok(bytes) = generation::serialized_size(&block, remaining) else {
            failure.get_or_insert_with(output_exceeded);
            break;
        };
        retained_bytes = retained_bytes.saturating_add(bytes + 1);
        report.content.push(block);
        if retained_bytes >= limit {
            break;
        }
    }
    if let Some(error) = failure {
        return Err(with_report(error, report));
    }
    check_control(cancellation, deadline).map_err(|error| with_report(error, report.clone()))?;
    Ok(report)
}

fn validate_output(
    report: GenerationReport,
    schema: Option<OutputSchema>,
) -> Result<MessageResponse, MessageError> {
    if report.completion != GenerationCompletion::Complete {
        return Err(with_report(
            MessageError::new(
                MessageErrorKind::Provider,
                "provider stream ended without verified completion",
            ),
            report,
        ));
    }
    if report.content.iter().any(|block| {
        !matches!(
            block,
            ContentBlock::Text { .. } | ContentBlock::Thinking { .. }
        )
    }) {
        return Err(with_report(
            MessageError::new(
                MessageErrorKind::UnsupportedOutput,
                "provider produced unsupported content",
            ),
            report,
        ));
    }
    let mut response = MessageResponse {
        report,
        parsed_json: None,
    };
    if let Some(schema) = schema {
        let parsed = serde_json::from_str::<serde_json::Value>(&response.text());
        response.parsed_json = Some(match parsed {
            Ok(value)
                if response.report.finish_reason == GenerationFinish::Stop
                    && schema.accepts(&value) =>
            {
                value
            }
            _ => {
                return Err(with_report(
                    MessageError::new(
                        MessageErrorKind::OutputValidation,
                        "output did not satisfy the requested schema and termination contract",
                    ),
                    response.report,
                ));
            }
        });
    }
    Ok(response)
}

fn resolve_settings(
    requested: &GenerationSettings,
    defaults: &GenerationSettings,
    descriptor: &ModelDescriptor,
) -> Result<GenerationSettings, MessageError> {
    let settings = GenerationSettings {
        max_output_tokens: requested.max_output_tokens.or(defaults.max_output_tokens),
        temperature: requested.temperature.or(defaults.temperature),
        thinking_level: requested.thinking_level.or(defaults.thinking_level),
    };
    if settings
        .max_output_tokens
        .is_some_and(|limit| limit.get() > descriptor.max_output_tokens)
    {
        return Err(MessageError::new(
            MessageErrorKind::InvalidRequest,
            "output token limit exceeds the selected model ceiling",
        ));
    }
    if let Some(temperature) = settings.temperature {
        let maximum = if descriptor.api == crate::provider::ProviderApi::AnthropicMessages {
            1.0
        } else {
            2.0
        };
        if !(0.0..=maximum).contains(&temperature) {
            return Err(MessageError::new(
                MessageErrorKind::InvalidRequest,
                "temperature is outside the supported range",
            ));
        }
        if descriptor.reasoning {
            return Err(MessageError::new(
                MessageErrorKind::UnsupportedCapability,
                "temperature support is not established for this reasoning model",
            ));
        }
    }
    Ok(settings)
}

struct OutputBudget {
    limit: usize,
    used: usize,
    reasoning: BTreeMap<String, usize>,
    text_open: bool,
}

impl OutputBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: 2,
            reasoning: BTreeMap::new(),
            text_open: false,
        }
    }

    fn observe(&mut self, item: &StreamedAssistantContent) -> Result<(), MessageError> {
        let additional = match item {
            StreamedAssistantContent::Text(text) => {
                let text_bytes = generation::serialized_size(&text.text, self.limit)
                    .map_err(|_| output_exceeded())?
                    .saturating_sub(2);
                let overhead = if self.text_open {
                    0
                } else {
                    generation::serialized_size(&ContentBlock::Text { text: "".into() }, self.limit)
                        .map_err(|_| output_exceeded())?
                        + 1
                };
                self.text_open = true;
                text_bytes.saturating_add(overhead)
            }
            StreamedAssistantContent::ReasoningDelta {
                id,
                provider_id,
                reasoning,
            } => {
                self.text_open = false;
                let previous = self.reasoning.get(id).copied().unwrap_or(0);
                let text_bytes = generation::serialized_size(reasoning, self.limit)
                    .map_err(|_| output_exceeded())?
                    .saturating_sub(2);
                let overhead = if previous == 0 {
                    let empty = rig::message::Reasoning {
                        id: provider_id.clone(),
                        content: vec![rig::message::ReasoningContent::Text {
                            text: String::new(),
                            signature: None,
                        }],
                    };
                    let block = generation::content_block(
                        &rig::completion::AssistantContent::Reasoning(empty),
                    )
                    .map_err(|_| output_exceeded())?;
                    generation::serialized_size(&block, self.limit)
                        .map_err(|_| output_exceeded())?
                        + 1
                } else {
                    0
                };
                let replacement = previous
                    .saturating_add(text_bytes.saturating_mul(2))
                    .saturating_add(overhead);
                self.replace_reasoning(id, previous, replacement)?;
                return Ok(());
            }
            StreamedAssistantContent::Reasoning { id, reasoning } => {
                self.text_open = false;
                generation::serialized_size(reasoning, self.limit)
                    .map_err(|_| output_exceeded())?;
                let block = generation::content_block(
                    &rig::completion::AssistantContent::Reasoning(reasoning.clone()),
                )
                .map_err(|_| output_exceeded())?;
                let replacement = generation::serialized_size(&block, self.limit)
                    .map_err(|_| output_exceeded())?
                    + 1;
                let previous = self.reasoning.get(id).copied().unwrap_or(0);
                self.replace_reasoning(id, previous, replacement)?;
                return Ok(());
            }
            StreamedAssistantContent::Final(terminal) => metadata_bytes(
                [
                    terminal.message_id.as_deref(),
                    terminal.response_id.as_deref(),
                    terminal.provider_request_id.as_deref(),
                    terminal.model.as_deref(),
                    match &terminal.finish_reason {
                        Some(FinishReason::Other(reason)) => Some(reason.as_str()),
                        _ => None,
                    },
                ],
                self.limit,
            )?,
            StreamedAssistantContent::ToolCall { .. } => {
                self.text_open = false;
                generation::serialized_size(item, self.limit).map_err(|_| output_exceeded())?
            }
            StreamedAssistantContent::ToolCallDelta { .. } => {
                return Err(MessageError::new(
                    MessageErrorKind::UnsupportedOutput,
                    "provider attempted an unsupported tool call",
                ));
            }
            StreamedAssistantContent::Unknown(_) => {
                return Err(MessageError::new(
                    MessageErrorKind::UnsupportedOutput,
                    "provider produced an unsupported output item",
                ));
            }
        };
        let used = self.used.saturating_add(additional);
        if used > self.limit {
            return Err(output_exceeded());
        }
        self.used = used;
        Ok(())
    }

    fn replace_reasoning(
        &mut self,
        id: &str,
        previous: usize,
        replacement: usize,
    ) -> Result<(), MessageError> {
        let used = self
            .used
            .saturating_sub(previous)
            .saturating_add(replacement);
        if used > self.limit {
            return Err(output_exceeded());
        }
        self.reasoning.insert(id.to_owned(), replacement);
        self.used = used;
        Ok(())
    }
}

fn metadata_bytes(values: [Option<&str>; 5], limit: usize) -> Result<usize, MessageError> {
    values
        .into_iter()
        .flatten()
        .try_fold(0usize, |bytes, value| {
            generation::serialized_size(&value, limit.saturating_sub(bytes))
                .map(|additional| bytes.saturating_add(additional))
                .map_err(|_| output_exceeded())
        })
}

async fn collect(
    stream: &mut StreamingCompletionResponse,
    report: &mut GenerationReport,
    limits: &MessageLimits,
    descriptor: &ModelDescriptor,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), MessageError> {
    let mut budget = OutputBudget::new(limits.max_output_bytes.get());
    loop {
        check_control(cancellation, deadline)?;
        let item = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled()),
            _ = tokio::time::sleep_until(deadline) => return Err(timed_out()),
            item = stream.next() => item,
        };
        let Some(item) = item else { break };
        let item = item.map_err(|error| {
            report.completion = GenerationCompletion::Incomplete;
            provider_error(error)
        })?;
        if let StreamedAssistantContent::Final(terminal) = &item {
            report.usage = observed_usage(terminal, descriptor, limits.max_output_bytes.get());
        }
        budget.observe(&item)?;
        if let StreamedAssistantContent::Final(terminal) = item {
            report.completion = GenerationCompletion::Complete;
            report.finish_reason = GenerationFinish::from_reason(
                terminal.finish_reason.as_ref(),
                Some(descriptor.api),
            );
            let identity = terminal.identity();
            report.identity.provider_message_id = identity.message_id;
            report.identity.provider_response_id = identity.response_id;
            report.identity.provider_request_id = identity.provider_request_id;
            report.model.reported_model = terminal.model;
        }
        tokio::task::consume_budget().await;
    }
    Ok(())
}

fn output_exceeded() -> MessageError {
    MessageError::new(
        MessageErrorKind::LimitExceeded,
        "generation output exceeds its byte limit",
    )
}

fn observed_usage(
    terminal: &rig::streaming::StreamFinal,
    descriptor: &ModelDescriptor,
    raw_limit: usize,
) -> GenerationUsage {
    let observed = crate::provider::usage::observe_usage(terminal, descriptor.api, raw_limit);
    GenerationUsage {
        input_tokens: observed.input_tokens,
        output_tokens: observed.output_tokens,
        total_tokens: observed.total_tokens,
        cache_read_tokens: observed.cache_read_tokens,
        cache_write_tokens: observed.cache_write_tokens,
        reasoning_tokens: observed.reasoning_tokens,
        estimated_cost_usd: observed.estimated_cost(descriptor.api, &descriptor.cost),
    }
}

fn check_control(cancellation: &CancellationToken, deadline: Instant) -> Result<(), MessageError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    Ok(())
}

fn cancelled() -> MessageError {
    MessageError::new(MessageErrorKind::Cancelled, "generation was cancelled")
}

fn timed_out() -> MessageError {
    MessageError::new(
        MessageErrorKind::LimitExceeded,
        "generation exceeded its deadline",
    )
}

fn provider_error(error: CompletionError) -> MessageError {
    let facts = crate::provider::error_facts(&error);
    let kind = if matches!(
        facts.kind,
        crate::provider::ProviderErrorKind::Authentication
            | crate::provider::ProviderErrorKind::AccessDenied
    ) {
        MessageErrorKind::Unauthorized
    } else {
        MessageErrorKind::Provider
    };
    let mut failure =
        MessageError::new(kind, "provider generation failed").with_source(error.into());
    failure.provider_status = facts.status;
    failure
}

fn with_report(mut error: MessageError, mut report: GenerationReport) -> MessageError {
    if matches!(
        error.kind,
        MessageErrorKind::Cancelled | MessageErrorKind::LimitExceeded
    ) {
        report.completion = GenerationCompletion::Incomplete;
    }
    error.generation_id = Some(report.identity.generation_id);
    error.report = Some(Box::new(report));
    error
}

struct OutputSchema {
    kind: SchemaKind,
    enumeration: Option<Vec<Value>>,
}

enum SchemaKind {
    Object(BTreeMap<String, OutputSchema>),
    Array(Box<OutputSchema>),
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

fn compile_schema(
    document: &Value,
    descriptor: &ModelDescriptor,
    limits: &MessageLimits,
) -> Result<OutputSchema, MessageError> {
    generation::serialized_size(document, limits.max_schema_bytes.get()).map_err(|_| exceeded())?;
    let mut pending = vec![(document, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > limits.max_schema_nodes.get() || depth > limits.max_schema_depth.get() {
            return Err(exceeded());
        }
        match value {
            Value::Object(object) => {
                pending.extend(object.values().map(|value| (value, depth + 1)))
            }
            Value::Array(array) => pending.extend(array.iter().map(|value| (value, depth + 1))),
            _ => {}
        }
    }
    if !generation::supports_output_schema(descriptor) {
        return Err(unsupported());
    }
    let schema = compile_node(document)?;
    if !matches!(schema.kind, SchemaKind::Object(_)) {
        return Err(unsupported());
    }
    Ok(schema)
}

fn compile_node(document: &Value) -> Result<OutputSchema, MessageError> {
    let object = document.as_object().ok_or_else(invalid)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let allowed: &[&str] = match kind {
        "object" => &[
            "type",
            "properties",
            "required",
            "additionalProperties",
            "description",
            "title",
            "enum",
        ],
        "array" => &["type", "items", "description", "title", "enum"],
        "string" | "number" | "integer" | "boolean" | "null" => {
            &["type", "description", "title", "enum"]
        }
        _ => return Err(unsupported()),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(unsupported());
    }
    for annotation in ["description", "title"] {
        if object
            .get(annotation)
            .is_some_and(|value| !value.is_string())
        {
            return Err(invalid());
        }
    }
    let kind = match kind {
        "object" => {
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            let required = object
                .get("required")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            if object.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Err(unsupported());
            }
            if required.len() != properties.len()
                || required
                    .iter()
                    .any(|key| key.as_str().is_none_or(|key| !properties.contains_key(key)))
                || properties.keys().any(|key| {
                    required
                        .iter()
                        .filter(|value| value.as_str() == Some(key))
                        .count()
                        != 1
                })
            {
                return Err(invalid());
            }
            SchemaKind::Object(
                properties
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), compile_node(value)?)))
                    .collect::<Result<_, MessageError>>()?,
            )
        }
        "array" => SchemaKind::Array(Box::new(compile_node(
            object.get("items").ok_or_else(invalid)?,
        )?)),
        "string" => SchemaKind::String,
        "number" => SchemaKind::Number,
        "integer" => SchemaKind::Integer,
        "boolean" => SchemaKind::Boolean,
        "null" => SchemaKind::Null,
        _ => unreachable!(),
    };
    let mut schema = OutputSchema {
        kind,
        enumeration: None,
    };
    if let Some(values) = object.get("enum") {
        if !matches!(schema.kind, SchemaKind::String) {
            return Err(unsupported());
        }
        let values = values
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(invalid)?;
        if values.iter().any(|value| !schema.accepts(value))
            || values
                .iter()
                .filter_map(Value::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != values.len()
        {
            return Err(invalid());
        }
        schema.enumeration = Some(values.clone());
    }
    Ok(schema)
}

impl OutputSchema {
    fn accepts(&self, value: &Value) -> bool {
        if self
            .enumeration
            .as_ref()
            .is_some_and(|values| !values.contains(value))
        {
            return false;
        }
        match &self.kind {
            SchemaKind::Object(properties) => value.as_object().is_some_and(|object| {
                object.len() == properties.len()
                    && properties.iter().all(|(key, schema)| {
                        object.get(key).is_some_and(|value| schema.accepts(value))
                    })
            }),
            SchemaKind::Array(items) => value
                .as_array()
                .is_some_and(|array| array.iter().all(|value| items.accepts(value))),
            SchemaKind::String => value.is_string(),
            SchemaKind::Number => value.is_number(),
            SchemaKind::Integer => value.as_number().is_some_and(|number| {
                number.is_i64()
                    || number.is_u64()
                    || number.as_f64().is_some_and(|value| value.fract() == 0.0)
            }),
            SchemaKind::Boolean => value.is_boolean(),
            SchemaKind::Null => value.is_null(),
        }
    }
}

fn invalid() -> MessageError {
    MessageError::new(
        MessageErrorKind::InvalidRequest,
        "invalid output schema document",
    )
}

fn unsupported() -> MessageError {
    MessageError::new(
        MessageErrorKind::UnsupportedCapability,
        "native output schema or schema feature is not supported",
    )
}

fn exceeded() -> MessageError {
    MessageError::new(
        MessageErrorKind::LimitExceeded,
        "output schema exceeds its complexity or byte limit",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelCatalog, ModelCost, ModelDescriptor, ModelInput};
    use crate::providers::ProviderApi;
    use proptest::prelude::*;

    #[test]
    fn provider_failures_preserve_message_policy_and_source() {
        use http::StatusCode;
        use std::error::Error;

        for (status, expected) in [
            (StatusCode::UNAUTHORIZED, MessageErrorKind::Unauthorized),
            (StatusCode::FORBIDDEN, MessageErrorKind::Unauthorized),
            (StatusCode::TOO_MANY_REQUESTS, MessageErrorKind::Provider),
            (StatusCode::BAD_REQUEST, MessageErrorKind::Provider),
            (StatusCode::SERVICE_UNAVAILABLE, MessageErrorKind::Provider),
        ] {
            let original = CompletionError::from_http_response(status, "context_length_exceeded");
            let diagnostic = original.to_string();
            let source_diagnostic = original.source().map(ToString::to_string);
            let failure = provider_error(original);
            assert_eq!(failure.kind(), expected);
            assert_eq!(failure.provider_status(), Some(status.as_u16()));
            let source = failure.source().unwrap();
            assert_eq!(source.to_string(), diagnostic);
            assert_eq!(source.source().map(ToString::to_string), source_diagnostic);
        }
    }

    fn limits() -> MessageLimits {
        let bound = NonZeroUsize::new(1024).unwrap();
        MessageLimits::new(
            bound,
            bound,
            NonZeroUsize::new(64 * 1024).unwrap(),
            bound,
            bound,
            NonZeroUsize::new(16).unwrap(),
            NonZeroUsize::new(2).unwrap(),
            Duration::from_secs(30),
        )
        .unwrap()
    }

    fn phi() -> crate::Phi {
        let descriptor = ModelDescriptor {
            provider: "ollama".into(),
            id: "local-model".into(),
            name: "Local model".into(),
            api: ProviderApi::OllamaChat,
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            context_window: 4096,
            max_output_tokens: 1024,
            cost: ModelCost::default(),
        };
        crate::Phi::builder()
            .provider("unused-agent-provider")
            .model("unused-agent-model")
            .max_tokens(0)
            .temperature(f64::NAN)
            .no_context_files(true)
            .runtime_services(
                crate::PhiRuntimeServices::new()
                    .model_catalog(Arc::new(ModelCatalog::for_test_model(descriptor).unwrap())),
            )
            .build()
            .unwrap()
    }

    fn selection() -> ModelSelection {
        ModelSelection::new("ollama", "local-model").unwrap()
    }

    #[test]
    fn prepares_only_explicit_models_and_defaults_without_an_agent_runtime() {
        let selection = selection();
        let capability = phi()
            .messages(
                MessagesOptions::new(vec![selection.clone(), selection.clone()], limits()).unwrap(),
            )
            .unwrap();
        assert_eq!(capability.allowed_models(), &[selection]);
        assert_eq!(capability.default_model(), None);
        assert_eq!(capability.defaults(), &GenerationSettings::default());
        assert_eq!(capability.clone().limits(), &limits());
    }

    #[test]
    fn rejects_invalid_policy_and_all_unresolved_selections() {
        let valid = selection();
        let missing = ModelSelection::new("ollama", "missing").unwrap();
        assert!(MessagesOptions::new(Vec::new(), limits()).is_err());
        for options in [
            MessagesOptions::new(vec![valid.clone()], limits())
                .unwrap()
                .with_default_model(missing.clone()),
            MessagesOptions::new(vec![valid.clone(), missing], limits()).unwrap(),
            MessagesOptions::new(vec![valid.clone()], limits())
                .unwrap()
                .with_defaults(
                    GenerationSettings::default()
                        .with_temperature(-1.0)
                        .unwrap(),
                ),
            MessagesOptions::new(vec![valid], limits())
                .unwrap()
                .with_defaults(
                    GenerationSettings::default()
                        .with_max_output_tokens(NonZeroU64::new(2048).unwrap()),
                ),
        ] {
            assert_eq!(
                phi().messages(options).unwrap_err().kind(),
                MessageErrorKind::InvalidRequest
            );
        }
        let capability = phi()
            .messages(
                MessagesOptions::new(vec![selection()], limits())
                    .unwrap()
                    .with_default_model(selection()),
            )
            .unwrap();
        assert_eq!(capability.default_model(), Some(&selection()));
    }

    #[test]
    fn requires_an_injected_catalog_and_rejects_protocol_mismatch() {
        let factory = phi().with_runtime_services(crate::PhiRuntimeServices::new());
        let options = MessagesOptions::new(vec![selection()], limits()).unwrap();
        assert_eq!(
            factory.messages(options.clone()).unwrap_err().kind(),
            MessageErrorKind::InvalidRequest
        );
        let mut factory = phi();
        factory.cfg.custom_providers.insert(
            "ollama".into(),
            crate::config::CustomProviderConfig {
                api: ProviderApi::OpenAiResponses,
                base_url: None,
                api_key: None,
            },
        );
        assert_eq!(
            factory.messages(options).unwrap_err().kind(),
            MessageErrorKind::UnsupportedCapability
        );
    }

    #[test]
    fn preparation_errors_keep_sources_out_of_public_diagnostics() {
        let mut factory = phi();
        factory.cfg.custom_providers.insert(
            "ollama".into(),
            crate::config::CustomProviderConfig {
                api: ProviderApi::OllamaChat,
                base_url: None,
                api_key: Some("${".into()),
            },
        );
        let error = factory
            .messages(MessagesOptions::new(vec![selection()], limits()).unwrap())
            .unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::Provider);
        assert!(std::error::Error::source(&error).is_some());
        assert!(!error.to_string().contains("apiKey"));
        assert!(!format!("{error:?}").contains("apiKey"));
    }

    #[test]
    fn rejects_empty_input_and_unusable_limits() {
        assert!(InputMessage::new(MessageRole::User, "").is_err());
        assert!(MessageRequest::new(Vec::new()).is_err());
        for temperature in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                GenerationSettings::default()
                    .with_temperature(temperature)
                    .is_err()
            );
        }
        let bound = NonZeroUsize::new(1).unwrap();
        for duration in [Duration::ZERO, Duration::MAX] {
            assert!(
                MessageLimits::new(bound, bound, bound, bound, bound, bound, bound, duration)
                    .is_err()
            );
        }
        assert!(
            MessageLimits::new(
                bound,
                bound,
                bound,
                bound,
                bound,
                bound,
                NonZeroUsize::new(tokio::sync::Semaphore::MAX_PERMITS + 1).unwrap(),
                Duration::from_secs(1),
            )
            .is_err()
        );
    }

    fn descriptor() -> ModelDescriptor {
        phi()
            .runtime_services
            .resolve_model_catalog("ollama")
            .unwrap()
            .get("ollama", "local-model")
            .unwrap()
    }

    fn empty_report() -> GenerationReport {
        GenerationReport {
            identity: GenerationIdentity {
                generation_id: GenerationId(uuid::Uuid::new_v4()),
                attempt_id: GenerationAttemptId(uuid::Uuid::new_v4()),
                provider_message_id: None,
                provider_response_id: None,
                provider_request_id: None,
            },
            model: GenerationModel {
                requested: None,
                selected: selection(),
                reported_model: None,
            },
            completion: GenerationCompletion::Incomplete,
            content: Vec::new(),
            finish_reason: GenerationFinish::Unknown,
            usage: GenerationUsage::default(),
        }
    }

    fn terminal(reason: Option<FinishReason>) -> rig::streaming::RawStreamingChoice {
        rig::streaming::RawStreamingChoice::FinalResponse(
            rig::streaming::StreamFinal::new("ollama", rig::completion::Usage::default())
                .with_optional_finish_reason(reason),
        )
    }

    async fn collect_events(
        events: Vec<Result<rig::streaming::RawStreamingChoice, CompletionError>>,
        limits: MessageLimits,
    ) -> Result<GenerationReport, MessageError> {
        collect_generation(
            StreamingCompletionResponse::stream("ollama", Box::pin(futures::stream::iter(events))),
            empty_report(),
            &limits,
            &descriptor(),
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(5),
        )
        .await
    }

    fn report_text(report: &GenerationReport) -> String {
        report
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn completion_requires_a_terminal_and_retains_finish_facts() {
        use rig::streaming::RawStreamingChoice as Event;
        for (reason, expected) in [
            (Some(FinishReason::Stop), GenerationFinish::Stop),
            (Some(FinishReason::Length), GenerationFinish::Length),
            (
                Some(FinishReason::ContentFilter),
                GenerationFinish::ContentFilter,
            ),
            (
                Some(FinishReason::Other("future_reason".into())),
                GenerationFinish::Other("future_reason".into()),
            ),
            (None, GenerationFinish::Unknown),
        ] {
            let report = collect_events(
                vec![
                    Ok(Event::Message("first".into())),
                    Ok(Event::Message("second".into())),
                    Ok(terminal(reason)),
                ],
                limits(),
            )
            .await
            .unwrap();
            assert_eq!(report.completion(), GenerationCompletion::Complete);
            assert_eq!(report.finish_reason(), &expected);
            assert_eq!(validate_output(report, None).unwrap().text(), "firstsecond");
        }
        let report = collect_events(vec![Ok(Event::Message("partial".into()))], limits())
            .await
            .unwrap();
        let error = validate_output(report, None).unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::Provider);
        assert_eq!(
            error.report().unwrap().completion(),
            GenerationCompletion::Incomplete
        );
        assert_eq!(report_text(error.report().unwrap()), "partial");
    }

    #[tokio::test]
    async fn output_limit_keeps_a_bounded_utf8_prefix() {
        use rig::streaming::RawStreamingChoice as Event;
        let mut bounds = limits();
        bounds.max_output_bytes = NonZeroUsize::new(96).unwrap();
        let error = collect_events(
            vec![
                Ok(Event::Message("prefix中".into())),
                Ok(Event::Message("界".repeat(100))),
            ],
            bounds.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
        let report = error.report().unwrap();
        assert_eq!(report.completion(), GenerationCompletion::Incomplete);
        assert!(report_text(report).starts_with("prefix中"));
        assert!(
            serde_json::to_vec(report.content()).unwrap().len() <= bounds.max_output_bytes.get()
        );
    }

    #[tokio::test]
    async fn reasoning_restatements_do_not_consume_the_output_budget_twice() {
        use rig::message::{Reasoning, ReasoningContent};
        use rig::streaming::{RawStreamingChoice as Event, StreamPartId};
        let text = "thought".repeat(20);
        let reasoning = Reasoning {
            id: None,
            content: vec![ReasoningContent::Text {
                text: text.clone(),
                signature: Some("signed".into()),
            }],
        };
        let block =
            generation::content_block(&rig::completion::AssistantContent::Reasoning(reasoning))
                .unwrap();
        let mut bounds = limits();
        bounds.max_output_bytes =
            NonZeroUsize::new(serde_json::to_vec(&block).unwrap().len() + 16).unwrap();
        let key = StreamPartId::from("reasoning-key");
        let report = collect_events(
            vec![
                Ok(Event::ReasoningDelta {
                    id: key.clone(),
                    provider_id: None,
                    reasoning: text.clone(),
                }),
                Ok(Event::ReasoningEnd {
                    id: key.clone(),
                    reasoning: Some(Reasoning {
                        id: None,
                        content: vec![ReasoningContent::Text {
                            text,
                            signature: None,
                        }],
                    }),
                    signature: None,
                    wire_sent: true,
                }),
                Ok(Event::ReasoningEnd {
                    id: key,
                    reasoning: None,
                    signature: Some("signed".into()),
                    wire_sent: true,
                }),
                Ok(terminal(Some(FinishReason::Stop))),
            ],
            bounds,
        )
        .await
        .unwrap();
        assert_eq!(report.content().len(), 1);
        assert_eq!(
            serde_json::to_value(report.content()).unwrap(),
            serde_json::to_value([block]).unwrap()
        );
    }

    #[tokio::test]
    async fn terminal_metadata_is_bounded_before_it_enters_error_evidence() {
        use rig::streaming::{RawStreamingChoice as Event, StreamFinal};
        let mut usage = rig::completion::Usage::default();
        usage.input_tokens = 5;
        let event = StreamFinal::new("ollama", usage).with_message_id("sensitive-id".repeat(1024));
        let error = collect_events(
            vec![
                Ok(Event::Message("kept".into())),
                Ok(Event::FinalResponse(event)),
            ],
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
        let report = error.report().unwrap();
        assert!(report.identity().provider_message_id().is_none());
        assert_eq!(report.usage().input_tokens(), Some(5));
        assert_eq!(report_text(report), "kept");
    }

    #[tokio::test]
    async fn provider_failures_and_unsupported_outputs_keep_partial_evidence() {
        use rig::streaming::{RawStreamingChoice as Event, RawStreamingToolCall};
        let error = collect_events(
            vec![
                Ok(Event::Message("before error".into())),
                Err(CompletionError::ProviderError(
                    "untrusted provider body".into(),
                )),
            ],
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::Provider);
        assert_eq!(report_text(error.report().unwrap()), "before error");
        assert!(!error.to_string().contains("untrusted"));
        assert!(!format!("{error:?}").contains("untrusted"));
        for terminal_first in [false, true] {
            let failure = Err(CompletionError::ProviderError("stream interrupted".into()));
            let terminal = Ok(terminal(Some(FinishReason::Stop)));
            let events = if terminal_first {
                vec![terminal, failure]
            } else {
                vec![failure, terminal]
            };
            let error = collect_events(
                std::iter::once(Ok(Event::Message("before error".into())))
                    .chain(events)
                    .collect(),
                limits(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), MessageErrorKind::Provider);
            assert_eq!(
                error.report().unwrap().completion(),
                GenerationCompletion::Incomplete
            );
            assert_eq!(report_text(error.report().unwrap()), "before error");
        }
        let report = collect_events(
            vec![
                Ok(Event::ToolCall(RawStreamingToolCall::new(
                    "call-id",
                    "not-authorized".into(),
                    serde_json::json!({}),
                ))),
                Ok(terminal(Some(FinishReason::Stop))),
            ],
            limits(),
        )
        .await
        .unwrap();
        let error = validate_output(report, None).unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::UnsupportedOutput);
        assert!(matches!(
            error.report().unwrap().content()[0],
            ContentBlock::ToolCall { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_deadline_keep_observed_partial_content() {
        use rig::streaming::RawStreamingChoice as Event;
        for expected in [MessageErrorKind::Cancelled, MessageErrorKind::LimitExceeded] {
            let cancellation = CancellationToken::new();
            let signal = cancellation.clone();
            let events = futures::stream::once(async move {
                if expected == MessageErrorKind::Cancelled {
                    signal.cancel();
                }
                Ok(Event::Message("partial".into()))
            })
            .chain(futures::stream::pending());
            let error = collect_generation(
                StreamingCompletionResponse::stream("ollama", Box::pin(events)),
                empty_report(),
                &limits(),
                &descriptor(),
                &cancellation,
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), expected);
            assert_eq!(
                error.report().unwrap().completion(),
                GenerationCompletion::Incomplete
            );
            assert_eq!(report_text(error.report().unwrap()), "partial");
        }
    }

    fn schema_model() -> ModelDescriptor {
        let mut model = descriptor();
        model.provider = "openai".into();
        model.id = "gpt-4o-2024-08-06".into();
        model.api = ProviderApi::OpenAiResponses;
        model
    }

    fn output_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "label": { "type": "string", "enum": ["accept", "reject"] } },
            "required": ["label"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn schema_acceptance_requires_supported_shape_and_normal_termination() {
        let document = output_schema();
        for (text, finish) in [
            (r#"{}"#, GenerationFinish::Stop),
            (r#"{"label":1}"#, GenerationFinish::Stop),
            (r#"{"label":"accept","extra":true}"#, GenerationFinish::Stop),
            (r#"{"label":"accept"}"#, GenerationFinish::Unknown),
            (
                r#"{"label":"accept"}"#,
                GenerationFinish::Other("future".into()),
            ),
            ("not json", GenerationFinish::Stop),
        ] {
            let schema = compile_schema(&document, &schema_model(), &limits()).unwrap();
            let mut report = empty_report();
            report.completion = GenerationCompletion::Complete;
            report.finish_reason = finish;
            report
                .content
                .push(ContentBlock::Text { text: text.into() });
            assert_eq!(
                validate_output(report, Some(schema)).unwrap_err().kind(),
                MessageErrorKind::OutputValidation
            );
        }
        assert!(compile_schema(&document, &descriptor(), &limits()).is_err());
        for keyword in ["$ref", "$defs", "pattern", "oneOf", "format"] {
            let mut document = output_schema();
            document
                .as_object_mut()
                .unwrap()
                .insert(keyword.into(), Value::String("not supported".into()));
            assert_eq!(
                compile_schema(&document, &schema_model(), &limits())
                    .err()
                    .unwrap()
                    .kind(),
                MessageErrorKind::UnsupportedCapability
            );
        }
        let mut duplicate_enum = output_schema();
        duplicate_enum["properties"]["label"]["enum"] = serde_json::json!(["accept", "accept"]);
        assert_eq!(
            compile_schema(&duplicate_enum, &schema_model(), &limits())
                .err()
                .unwrap()
                .kind(),
            MessageErrorKind::InvalidRequest
        );
        for field in ["bytes", "depth", "nodes"] {
            let mut bounds = limits();
            match field {
                "bytes" => bounds.max_schema_bytes = NonZeroUsize::new(1).unwrap(),
                "depth" => bounds.max_schema_depth = NonZeroUsize::new(1).unwrap(),
                _ => bounds.max_schema_nodes = NonZeroUsize::new(1).unwrap(),
            }
            assert_eq!(
                compile_schema(&document, &schema_model(), &bounds)
                    .err()
                    .unwrap()
                    .kind(),
                MessageErrorKind::LimitExceeded
            );
        }
    }

    struct TestProvider {
        url: String,
        requests: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
        release: tokio::sync::watch::Sender<bool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestProvider {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl TestProvider {
        async fn new(body: String, hold_after: Option<usize>, status: http::StatusCode) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (requests, receiver) = tokio::sync::mpsc::unbounded_channel();
            let (release, released) = tokio::sync::watch::channel(false);
            let task = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        connection = listener.accept() => {
                            let (mut socket, _) = connection.unwrap();
                            let body = body.clone();
                            let requests = requests.clone();
                            let mut released = released.clone();
                            connections.spawn(async move {
                                let mut request = Vec::new();
                                let mut chunk = [0u8; 4096];
                                let header_end = loop {
                                    let count = socket.read(&mut chunk).await.unwrap();
                                    if count == 0 { return; }
                                    request.extend_from_slice(&chunk[..count]);
                                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") { break end + 4; }
                                };
                                let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                                let length: usize = headers.lines().find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                                }).unwrap();
                                while request.len() < header_end + length {
                                    let count = socket.read(&mut chunk).await.unwrap();
                                    if count == 0 { return; }
                                    request.extend_from_slice(&chunk[..count]);
                                }
                                let request = serde_json::from_slice(&request[header_end..header_end + length]).unwrap();
                                if requests.send((headers, request)).is_err() { return; }
                                let headers = format!("HTTP/1.1 {} {}\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nx-request-id: transport-request\r\n\r\n", status.as_u16(), status.canonical_reason().unwrap());
                                if socket.write_all(headers.as_bytes()).await.is_err() { return; }
                                let split = hold_after.unwrap_or(body.len());
                                let prefix = format!("{:x}\r\n{}\r\n", split, &body[..split]);
                                if split != 0 && socket.write_all(prefix.as_bytes()).await.is_err() { return; }
                                if hold_after.is_some() && released.wait_for(|released| *released).await.is_err() { return; }
                                if split < body.len() {
                                    let tail = format!("{:x}\r\n{}\r\n", body.len() - split, &body[split..]);
                                    if socket.write_all(tail.as_bytes()).await.is_err() { return; }
                                }
                                let _ = socket.write_all(b"0\r\n\r\n").await;
                            });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            });
            Self {
                url,
                requests: receiver,
                release,
                task,
            }
        }

        fn messages(&self, descriptor: ModelDescriptor, limits: MessageLimits) -> Messages {
            let selection = ModelSelection::new(&descriptor.provider, &descriptor.id).unwrap();
            let mut factory =
                phi().with_runtime_services(crate::PhiRuntimeServices::new().model_catalog(
                    Arc::new(ModelCatalog::for_test_model(descriptor.clone()).unwrap()),
                ));
            factory.api_key = Some("primary-override-must-not-leak".into());
            factory.cfg.custom_providers.insert(
                descriptor.provider.clone(),
                crate::config::CustomProviderConfig {
                    api: descriptor.api,
                    base_url: Some(self.url.clone()),
                    api_key: Some(String::new()),
                },
            );
            factory
                .messages(
                    MessagesOptions::new(vec![selection.clone()], limits)
                        .unwrap()
                        .with_default_model(selection),
                )
                .unwrap()
        }

        async fn next_request(&mut self) -> (String, Value) {
            tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
                .await
                .unwrap()
                .unwrap()
        }
    }

    fn request() -> MessageRequest {
        MessageRequest::new(vec![
            InputMessage::new(MessageRole::User, "explicit input").unwrap(),
        ])
        .unwrap()
    }

    fn ollama_body() -> String {
        [
            serde_json::json!({"model":"reported-model", "created_at":"2026-01-01T00:00:00Z", "message":{"role":"assistant", "content":"first"}, "done":false}),
            serde_json::json!({"model":"reported-model", "created_at":"2026-01-01T00:00:00Z", "message":{"role":"assistant", "content":"second"}, "done":false}),
            serde_json::json!({"model":"reported-model", "created_at":"2026-01-01T00:00:00Z", "message":{"role":"assistant", "content":""}, "done":true, "done_reason":"stop", "prompt_eval_count":0, "eval_count":2}),
        ].iter().map(|value| format!("{value}\n")).collect()
    }

    fn openai_body(content: &str, reason: &str) -> String {
        let text = serde_json::json!({"type":"response.output_text.delta", "item_id":"message-id", "output_index":0, "content_index":0, "sequence_number":1, "delta":content});
        let status = if reason == "length" {
            "incomplete"
        } else {
            "completed"
        };
        let incomplete = if reason == "length" {
            serde_json::json!({"reason":"max_output_tokens"})
        } else {
            Value::Null
        };
        let terminal = serde_json::json!({"type":"response.completed", "sequence_number":2, "response":{
            "id":"response-id", "object":"response", "created_at":1, "status":status, "incomplete_details":incomplete,
            "model":"reported-openai", "usage":{"input_tokens":3, "output_tokens":2, "total_tokens":5},
            "output":[{"type":"message", "id":"message-id", "role":"assistant", "status":"completed", "content":[{"type":"output_text", "text":content, "annotations":[]}]}],
        }});
        format!("data: {text}\n\ndata: {terminal}\n\n")
    }

    #[tokio::test]
    async fn create_sends_only_explicit_context_and_preserves_model_and_usage_facts() {
        let mut server = TestProvider::new(ollama_body(), None, http::StatusCode::OK).await;
        let capability = server.messages(descriptor(), limits());
        let response = capability
            .create(
                request()
                    .with_instructions("only these instructions")
                    .with_settings(
                        GenerationSettings::default()
                            .with_max_output_tokens(NonZeroU64::new(42).unwrap()),
                    ),
            )
            .await
            .unwrap();
        let (headers, request) = server.next_request().await;
        assert!(!headers.contains("primary-override-must-not-leak"));
        assert_eq!(request["model"], "local-model");
        assert_eq!(request["options"]["num_predict"], 42);
        assert_eq!(
            request["messages"],
            serde_json::json!([
                {"role":"system", "content":"only these instructions"},
                {"role":"user", "content":"explicit input"},
            ])
        );
        assert_eq!(response.text(), "firstsecond");
        assert_eq!(
            response.report().completion(),
            GenerationCompletion::Complete
        );
        assert_eq!(response.report().model().requested(), None);
        assert_eq!(response.report().model().selected(), &selection());
        assert_eq!(
            response.report().model().reported_model(),
            Some("reported-model")
        );
        assert_eq!(response.report().usage().input_tokens(), Some(0));
        assert_eq!(response.report().usage().output_tokens(), Some(2));
        assert_eq!(response.report().usage().estimated_cost_usd(), None);
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn native_schema_dispatch_and_rejection_keep_provider_evidence() {
        for (text, finish, expected) in [
            (r#"{"label":"accept"}"#, "stop", None),
            (
                r#"{"label":"unknown"}"#,
                "stop",
                Some(MessageErrorKind::OutputValidation),
            ),
            (
                r#"{"label":"accept"}"#,
                "length",
                Some(MessageErrorKind::OutputValidation),
            ),
        ] {
            let mut server =
                TestProvider::new(openai_body(text, finish), None, http::StatusCode::OK).await;
            let capability = server.messages(schema_model(), limits());
            let result = capability
                .create(request().with_output(MessageOutput::JsonSchema {
                    schema: output_schema(),
                }))
                .await;
            let (_, request) = server.next_request().await;
            assert_eq!(request["text"]["format"]["type"], "json_schema");
            let report = match expected {
                None => {
                    let response = result.unwrap();
                    assert_eq!(
                        response.parsed_json(),
                        Some(&serde_json::json!({"label":"accept"}))
                    );
                    response.report
                }
                Some(kind) => {
                    let error = result.unwrap_err();
                    assert_eq!(error.kind(), kind);
                    assert_eq!(
                        error.generation_id(),
                        Some(error.report().unwrap().identity().generation_id())
                    );
                    *error.report.unwrap()
                }
            };
            assert_eq!(report.completion(), GenerationCompletion::Complete);
            assert_eq!(
                report.identity().provider_response_id(),
                Some("response-id")
            );
            assert_eq!(
                report.identity().provider_request_id(),
                Some("transport-request")
            );
            assert_eq!(report.identity().provider_message_id(), Some("message-id"));
            assert_eq!(report.usage().input_tokens(), Some(3));
            assert!(server.requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn failures_before_dispatch_have_no_attempt_and_provider_errors_are_not_retried() {
        let mut server = TestProvider::new(
            "untrusted-provider-diagnostic".into(),
            None,
            http::StatusCode::UNAUTHORIZED,
        )
        .await;
        let capability = server.messages(descriptor(), limits());
        let mut oversized = request();
        oversized.instructions = Some("x".repeat(limits().max_input_bytes.get()).into());
        for (request, expected) in [
            (oversized, MessageErrorKind::LimitExceeded),
            (
                request().with_model(ModelSelection::new("ollama", "not-allowed").unwrap()),
                MessageErrorKind::InvalidRequest,
            ),
            (
                request().with_output(MessageOutput::JsonSchema {
                    schema: output_schema(),
                }),
                MessageErrorKind::UnsupportedCapability,
            ),
            (
                MessageRequest::new(vec![
                    InputMessage::new(MessageRole::Assistant, "prefill").unwrap(),
                ])
                .unwrap(),
                MessageErrorKind::UnsupportedCapability,
            ),
        ] {
            let error = capability.create(request).await.unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(error.generation_id().is_some());
            assert!(error.report().is_none());
        }
        assert!(server.requests.try_recv().is_err());
        let error = capability.create(request()).await.unwrap_err();
        server.next_request().await;
        assert_eq!(error.kind(), MessageErrorKind::Unauthorized);
        assert_eq!(
            error.provider_status(),
            Some(http::StatusCode::UNAUTHORIZED.as_u16())
        );
        assert!(error.report().is_some());
        assert!(!error.to_string().contains("untrusted-provider"));
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn response_bytes_are_bounded_before_parsing_and_error_buffering() {
        for status in [http::StatusCode::OK, http::StatusCode::UNAUTHORIZED] {
            let body = if status.is_success() {
                format!(
                    "{}{}",
                    ": ignored framing\n\n".repeat(128),
                    openai_body("ok", "stop")
                )
            } else {
                "untrusted-error-body".repeat(256)
            };
            let mut server = TestProvider::new(body, None, status).await;
            let mut bounds = limits();
            bounds.max_response_bytes = NonZeroUsize::new(512).unwrap();
            let capability = server.messages(schema_model(), bounds);
            let error = tokio::time::timeout(Duration::from_secs(5), capability.create(request()))
                .await
                .unwrap()
                .unwrap_err();
            server.next_request().await;
            assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
            assert_eq!(error.provider_status(), Some(status.as_u16()));
            assert_eq!(
                error.report().unwrap().completion(),
                GenerationCompletion::Incomplete
            );
            assert!(error.report().unwrap().content().is_empty());
            assert!(!format!("{error:?}").contains("untrusted-error-body"));
            assert!(server.requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn response_limit_preserves_partial_content_and_releases_capacity() {
        let body = ollama_body();
        let split = body.find('\n').unwrap() + 1;
        let mut server = TestProvider::new(body, Some(split), http::StatusCode::OK).await;
        let mut bounds = limits();
        bounds.max_response_bytes = NonZeroUsize::new(split).unwrap();
        bounds.max_concurrent_generations = NonZeroUsize::new(1).unwrap();
        let capability = server.messages(descriptor(), bounds);
        let mut active = Box::pin(capability.create(request()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), active.as_mut())
                .await
                .is_err()
        );
        server.next_request().await;
        server.release.send(true).unwrap();
        let error = active.await.unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
        let report = error.report().unwrap();
        assert_eq!(report.completion(), GenerationCompletion::Incomplete);
        assert_eq!(report.finish_reason(), &GenerationFinish::Unknown);
        assert!(
            matches!(report.content(), [ContentBlock::Text { text }] if text.as_str() == "first")
        );
        let second = capability.create(request()).await.unwrap_err();
        server.next_request().await;
        assert_eq!(second.kind(), MessageErrorKind::LimitExceeded);
        assert_ne!(error.generation_id(), second.generation_id());
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn cloned_calls_have_independent_response_byte_budgets() {
        let body = ollama_body();
        let mut server = TestProvider::new(body.clone(), None, http::StatusCode::OK).await;
        let mut bounds = limits();
        bounds.max_response_bytes = NonZeroUsize::new(body.len()).unwrap();
        let capability = server.messages(descriptor(), bounds);
        let cloned = capability.clone();
        let (first, second) = tokio::join!(capability.create(request()), cloned.create(request()));
        assert_eq!(first.unwrap().text(), "firstsecond");
        assert_eq!(second.unwrap().text(), "firstsecond");
        server.next_request().await;
        server.next_request().await;
        assert_eq!(
            capability.create(request()).await.unwrap().text(),
            "firstsecond"
        );
        server.next_request().await;
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn admission_deadline_expires_without_provider_dispatch() {
        let mut server = TestProvider::new(ollama_body(), None, http::StatusCode::OK).await;
        let mut bounds = limits();
        bounds.max_concurrent_generations = NonZeroUsize::new(1).unwrap();
        bounds.max_duration = Duration::from_millis(50);
        let capability = server.messages(descriptor(), bounds);
        let permit = capability.permits.acquire().await.unwrap();
        let error = capability.clone().create(request()).await.unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
        assert!(error.generation_id().is_some());
        assert!(error.report().is_none());
        assert!(server.requests.try_recv().is_err());
        drop(permit);
        assert_eq!(
            capability.create(request()).await.unwrap().text(),
            "firstsecond"
        );
        server.next_request().await;
        assert!(server.requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn dropped_and_cancelled_calls_release_capacity_and_transport() {
        let body = ollama_body();
        let mut server = TestProvider::new(body, Some(0), http::StatusCode::OK).await;
        let mut bounds = limits();
        bounds.max_concurrent_generations = NonZeroUsize::new(1).unwrap();
        let capability = server.messages(descriptor(), bounds);
        let first = capability.clone();
        let first = tokio::spawn(async move { first.create(request()).await });
        server.next_request().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        server.release.send(true).unwrap();
        let response = capability.create(request()).await.unwrap();
        server.next_request().await;
        assert_eq!(response.text(), "firstsecond");
        server.release.send(false).unwrap();
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let active = capability.clone();
        let active = tokio::spawn(async move { active.create_controlled(request(), signal).await });
        server.next_request().await;
        cancellation.cancel();
        server.release.send(true).unwrap();
        let error = active.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::Cancelled);
        assert_eq!(
            error.report().unwrap().completion(),
            GenerationCompletion::Incomplete
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = capability
            .create_controlled(request(), cancellation)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), MessageErrorKind::Cancelled);
        assert!(error.report().is_none());
        assert!(server.requests.try_recv().is_err());
    }

    #[test]
    fn settings_validate_effective_defaults_and_explicit_overrides() {
        let mut model = descriptor();
        model.api = ProviderApi::AnthropicMessages;
        let defaults = GenerationSettings::default()
            .with_max_output_tokens(NonZeroU64::new(model.max_output_tokens).unwrap())
            .with_temperature(1.5)
            .unwrap();
        let empty = GenerationSettings::default();
        assert_eq!(
            resolve_settings(&empty, &defaults, &model)
                .unwrap_err()
                .kind(),
            MessageErrorKind::InvalidRequest
        );
        let requested = GenerationSettings::default().with_temperature(1.0).unwrap();
        let resolved = resolve_settings(&requested, &defaults, &model).unwrap();
        assert_eq!(resolved.max_output_tokens(), defaults.max_output_tokens());
        assert_eq!(resolved.temperature(), Some(1.0));
        let excessive = requested
            .clone()
            .with_max_output_tokens(NonZeroU64::new(model.max_output_tokens + 1).unwrap());
        assert_eq!(
            resolve_settings(&excessive, &defaults, &model)
                .unwrap_err()
                .kind(),
            MessageErrorKind::InvalidRequest
        );
        model.api = ProviderApi::OpenAiResponses;
        assert!(resolve_settings(&empty, &defaults, &model).is_ok());
        model.reasoning = true;
        assert_eq!(
            resolve_settings(&empty, &defaults, &model)
                .unwrap_err()
                .kind(),
            MessageErrorKind::UnsupportedCapability
        );
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn retained_output_is_always_bounded_utf8(
            text in ".{0,512}",
            maximum in 32usize..256,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let mut bounds = limits();
            bounds.max_output_bytes = NonZeroUsize::new(maximum).unwrap();
            let result = runtime.block_on(collect_events(vec![Ok(rig::streaming::RawStreamingChoice::Message(text.clone()))], bounds));
            let report = match result {
                Ok(report) => report,
                Err(error) => {
                    prop_assert_eq!(error.kind(), MessageErrorKind::LimitExceeded);
                    *error.report.unwrap()
                }
            };
            prop_assert!(serde_json::to_vec(report.content()).unwrap().len() <= maximum);
            prop_assert!(text.starts_with(&report_text(&report)));
        }

    }
}
