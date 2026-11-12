use crate::core::event::AgentEvent;
use crate::core::lifecycle::MessageDisplayEvent;
use crate::hooks::{HookRegistry, emit_message_display};
use crate::provider::generation;
pub(super) use crate::provider::generation::tool_item_id;
use crate::transcript::{ContentBlock, ImageSource, Usage};
use compact_str::CompactString;
use rig::completion::AssistantContent;
#[cfg(test)]
use rig::completion::Message;
use rig::message::ToolCall;
#[cfg(test)]
use rig::message::{DocumentSourceKind, ReasoningContent};
use tokio::sync::mpsc;

pub(super) async fn flush_display_delta(
    hooks: &HookRegistry,
    event_tx: &mpsc::Sender<AgentEvent>,
    turn_id: &str,
    message_id: &str,
    index: &mut usize,
    pending: &mut String,
    delta: &str,
    final_batch: bool,
) {
    pending.push_str(delta);
    let original = take_display_delta(pending, final_batch);
    if original.is_empty() {
        return;
    }

    let result = emit_message_display(
        hooks,
        MessageDisplayEvent {
            turn_id: turn_id.to_string(),
            message_id: message_id.to_string(),
            index: *index,
            final_batch,
            delta: original.clone(),
        },
    )
    .await;
    *index += 1;

    let display = result.display_content.unwrap_or_else(|| original.clone());
    let _ = event_tx
        .send(AgentEvent::AssistantDelta {
            original: CompactString::new(""),
            display: CompactString::from(display),
        })
        .await;
}

fn take_display_delta(pending: &mut String, final_batch: bool) -> String {
    if final_batch {
        return std::mem::take(pending);
    }
    let Some(index) = pending.rfind('\n') else {
        return String::new();
    };
    pending.drain(..=index).collect()
}

pub(super) fn transcript_content(choice: &[AssistantContent]) -> Vec<ContentBlock> {
    choice
        .iter()
        .filter_map(|content| generation::content_block(content).ok())
        .collect()
}

pub(super) fn assistant_image_source(image: &rig::message::Image) -> Option<ImageSource> {
    generation::image_source(image).ok()
}

pub(super) fn collect_tool_calls(choice: &[AssistantContent]) -> Vec<ToolCall> {
    choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn tool_call_event(call: &ToolCall) -> AgentEvent {
    let ContentBlock::ToolCall {
        id,
        provider_call_id,
        provider_item_id,
        name,
        args,
    } = generation::tool_call_content(call)
    else {
        unreachable!("tool call conversion must produce tool call content")
    };
    AgentEvent::ToolCall {
        id,
        provider_call_id,
        provider_item_id,
        name,
        args,
    }
}

#[cfg(test)]
fn tool_result_message(call: &ToolCall, output: &str) -> Message {
    use rig::message::{ToolResultContent, UserContent};

    Message::User {
        content: vec![UserContent::tool_result_for(
            call.id.clone(),
            call.provider.clone(),
            call.function.name.clone(),
            vec![ToolResultContent::text(output)],
        )],
    }
}

#[derive(Default)]
pub(super) struct AccountedUsage {
    usage: Usage,
    estimated_output_tokens: Option<u64>,
}

impl AccountedUsage {
    pub(super) fn record(&mut self, other: &Self) {
        self.usage.input = self.usage.input.saturating_add(other.usage.input);
        self.usage.output = self.usage.output.saturating_add(other.usage.output);
        self.usage.total_tokens = self
            .usage
            .total_tokens
            .saturating_add(other.usage.total_tokens);
        self.usage.cache_read = self.usage.cache_read.saturating_add(other.usage.cache_read);
        self.usage.cache_write = self
            .usage
            .cache_write
            .saturating_add(other.usage.cache_write);
        self.usage.cost += other.usage.cost;
        if let Some(estimate) = other.estimated_output_tokens {
            self.estimated_output_tokens = Some(
                self.estimated_output_tokens
                    .unwrap_or_default()
                    .saturating_add(estimate),
            );
        }
    }

    pub(super) fn into_usage(self) -> Usage {
        self.usage
    }
}

pub(super) fn transcript_usage(
    provider_id: &str,
    api: Option<crate::provider::ProviderApi>,
    model_cost: &crate::models::ModelCost,
    observed: &crate::provider::usage::ObservedUsage,
    fallback_text: &str,
) -> AccountedUsage {
    let input = observed.input_tokens.unwrap_or_default();
    let estimated_output_tokens = observed
        .output_tokens
        .is_none()
        .then(|| crate::transcript::Transcript::estimate_tokens(fallback_text));
    let output = observed
        .output_tokens
        .or(estimated_output_tokens)
        .unwrap_or_default();
    let cache_read = observed.cache_read_tokens.unwrap_or_default();
    let cache_write = observed.cache_write_tokens.unwrap_or_default();
    let billable_output = api.map_or(output, |api| observed.output_for_cost(api, output));
    let cost = match api {
        Some(api) => crate::models::estimate_usage_cost(
            crate::provider::usage::input_accounting(api),
            model_cost,
            input,
            billable_output,
            cache_read,
            cache_write,
        ),
        None => crate::models::estimate_model_cost(
            provider_id,
            model_cost,
            input,
            output,
            cache_read,
            cache_write,
        ),
    };
    let total_input = match api {
        Some(api) => crate::provider::usage::input_accounting(api).total_input(
            input,
            cache_read,
            cache_write,
        ),
        None => input.saturating_add(cache_read).saturating_add(cache_write),
    };
    let total_tokens = if estimated_output_tokens.is_none() {
        observed
            .total_tokens
            .unwrap_or_else(|| total_input.saturating_add(billable_output))
    } else {
        total_input.saturating_add(billable_output)
    };

    AccountedUsage {
        usage: Usage {
            input,
            output,
            total_tokens,
            cost,
            cache_read,
            cache_write,
        },
        estimated_output_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use proptest::prelude::*;
    use rig::tool::ToolOutput;

    #[test]
    fn run_totals_keep_estimates_and_per_request_cost_tiers() {
        use crate::provider::{ProviderApi, usage::ObservedUsage};

        let rates = crate::models::ModelCost {
            input: 1.0,
            output: 1.0,
            tiers: vec![crate::models::ModelCostTier {
                input_tokens_above: 10,
                input: 100.0,
                output: 100.0,
                cache_read: 0.0,
                cache_write: 0.0,
            }],
            ..Default::default()
        };
        let missing = ObservedUsage {
            input_tokens: Some(6),
            ..Default::default()
        };
        let reported = ObservedUsage {
            input_tokens: Some(6),
            output_tokens: Some(0),
            total_tokens: Some(6),
            ..Default::default()
        };
        let first = transcript_usage(
            "alias",
            Some(ProviderApi::OpenAiCompletions),
            &rates,
            &missing,
            "estimate",
        );
        let second = transcript_usage(
            "alias",
            Some(ProviderApi::OpenAiCompletions),
            &rates,
            &reported,
            "not counted",
        );
        let estimate = Transcript::estimate_tokens("estimate");
        assert_eq!(first.estimated_output_tokens, Some(estimate));
        assert_eq!(second.estimated_output_tokens, None);
        assert_eq!(missing.output_tokens, None);
        let mut total = AccountedUsage::default();
        total.record(&first);
        total.record(&second);
        assert_eq!(total.estimated_output_tokens, Some(estimate));
        let usage = total.into_usage();
        assert_eq!(usage.output, estimate);
        assert_eq!(usage.total_tokens, 12 + estimate);
        assert!((usage.cost - (12 + estimate) as f64 / 1_000_000.0).abs() < f64::EPSILON);
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn transcript_accounting_estimates_only_unknown_output_and_uses_protocol(
            input in 0u64..1_000_000,
            output in 0u64..1_000_000,
            cache_read in 0u64..1_000_000,
            text in ".{0,128}",
        ) {
            use crate::provider::{ProviderApi, usage::ObservedUsage};

            let cache_read = cache_read.min(input);
            let rates = crate::models::ModelCost {
                input: 1.0, output: 2.0, cache_read: 0.5, ..Default::default()
            };
            for api in [ProviderApi::OpenAiCompletions, ProviderApi::AnthropicMessages] {
                for reported_output in [None, Some(0), Some(output)] {
                    let observed = ObservedUsage {
                        input_tokens: Some(input), output_tokens: reported_output,
                        cache_read_tokens: Some(cache_read), ..Default::default()
                    };
                    let accounted = transcript_usage("alias", Some(api), &rates, &observed, &text);
                    let expected_output = reported_output.unwrap_or_else(|| Transcript::estimate_tokens(&text));
                    prop_assert_eq!(accounted.estimated_output_tokens,
                        reported_output.is_none().then_some(expected_output));
                    let usage = accounted.into_usage();
                    let cache_excluded = api == ProviderApi::AnthropicMessages;
                    let billable_input = if cache_excluded { input } else { input - cache_read };
                    let total_input = if cache_excluded { input + cache_read } else { input };
                    let expected_cost = (billable_input as f64 + expected_output as f64 * 2.0
                        + cache_read as f64 * 0.5) / 1_000_000.0;
                    prop_assert_eq!(usage.input, input);
                    prop_assert_eq!(usage.output, expected_output);
                    prop_assert_eq!(usage.cache_read, cache_read);
                    prop_assert_eq!(usage.total_tokens, total_input + expected_output);
                    prop_assert!((usage.cost - expected_cost).abs() < 1e-12);
                }
            }
        }
    }

    #[test]
    fn canonical_reasoning_round_trips_through_transcript() {
        let mut reasoning =
            rig::message::Reasoning::new_with_signature("private", Some("sig_1".to_string()))
                .with_id("rs_1".to_string());
        reasoning
            .content
            .push(ReasoningContent::Encrypted("encrypted".to_string()));
        let mut transcript = Transcript::new();
        transcript.append_assistant_message(
            None,
            None,
            transcript_content(&[AssistantContent::Reasoning(reasoning)]),
            "openai".into(),
            crate::provider::ProviderApi::OpenAiResponses,
            "test".into(),
            Usage::default(),
            "stop".into(),
        );

        let history = crate::context::convert_history(&transcript).unwrap();
        let Message::Assistant { content, .. } = &history[0] else {
            panic!("expected assistant history");
        };
        let Some(AssistantContent::Reasoning(reasoning)) = content.iter().next() else {
            panic!("expected reasoning content");
        };

        assert_eq!(reasoning.id.as_deref(), Some("rs_1"));
        assert!(matches!(
            reasoning.content.as_slice(),
            [
                ReasoningContent::Text { text, signature },
                ReasoningContent::Encrypted(data)
            ] if text == "private"
                && signature.as_deref() == Some("sig_1")
                && data == "encrypted"
        ));
    }

    #[test]
    fn canonical_assistant_image_round_trips_through_transcript() {
        let image = rig::message::Image {
            data: DocumentSourceKind::Base64("aW1hZ2U=".to_string()),
            media_type: Some(rig::message::ImageMediaType::PNG),
            detail: None,
            additional_params: None,
        };
        let mut transcript = Transcript::new();
        transcript.append_assistant_message(
            None,
            None,
            transcript_content(&[AssistantContent::Image(image)]),
            "openai".into(),
            crate::provider::ProviderApi::OpenAiResponses,
            "test".into(),
            Usage::default(),
            "stop".into(),
        );

        let history = crate::context::convert_history(&transcript).unwrap();
        let Message::Assistant { content, .. } = &history[0] else {
            panic!("expected assistant history");
        };

        assert!(matches!(
            content.iter().next(),
            Some(AssistantContent::Image(image))
                if matches!(&image.data, DocumentSourceKind::Base64(data) if data == "aW1hZ2U=")
                    && image.media_type == Some(rig::message::ImageMediaType::PNG)
        ));
        assert!(matches!(
            assistant_image_source(&rig::message::Image {
                data: DocumentSourceKind::String("data:image/png;base64,aW1hZ2U=".into()),
                media_type: None,
                detail: None,
                additional_params: None,
            }),
            Some(ImageSource::Base64 { data, mime_type })
                if data == "aW1hZ2U=" && mime_type == "image/png"
        ));
    }

    #[test]
    fn tool_call_ids_and_results_survive_transcript_replay() {
        use rig::message::{ToolCallId, ToolFunction, ToolResultContent, UserContent};

        let output = ToolOutput::json(serde_json::json!({"result": 42})).render();
        let function = || ToolFunction {
            name: "bash".into(),
            arguments: serde_json::json!({"command": "pwd"}),
        };
        for call in [
            ToolCall::new(ToolCallId::mint(), function()),
            ToolCall::from_wire("tool_1", function()),
            ToolCall::from_dual_wire("fc_1", "call_1", function()),
        ] {
            let AgentEvent::ToolCall { id, .. } = tool_call_event(&call) else {
                panic!("expected tool call event");
            };
            assert_eq!(id.as_str(), tool_item_id(&call));

            let mut transcript = Transcript::new();
            transcript.append_assistant_message(
                None,
                None,
                transcript_content(&[AssistantContent::ToolCall(call.clone())]),
                "openai".into(),
                crate::provider::ProviderApi::OpenAiResponses,
                "test".into(),
                Usage::default(),
                "tool_use".into(),
            );
            transcript.append_tool_result(&id, &output, false, None);
            let restored: Transcript =
                serde_json::from_str(&serde_json::to_string(&transcript).unwrap()).unwrap();
            let history = crate::context::convert_history(&restored).unwrap();
            let [
                Message::Assistant { content, .. },
                Message::User { content: results },
            ] = history.as_slice()
            else {
                panic!("expected tool call and result");
            };
            let [AssistantContent::ToolCall(replayed)] = content.as_slice() else {
                panic!("expected tool call");
            };
            let [UserContent::ToolResult(result)] = results.as_slice() else {
                panic!("expected tool result");
            };
            let Message::User { content: live } = tool_result_message(&call, &output) else {
                panic!("expected live tool result");
            };
            assert_eq!(replayed.id, result.call);
            assert_eq!(replayed.provider, call.provider);
            assert_eq!(result.provider, call.provider);
            assert_eq!(results, &live);
            assert!(
                matches!(result.content.as_slice(), [ToolResultContent::Text(text)] if text.text == output)
            );
        }
    }
}
