use super::{ProviderApi, generation};
use crate::models::{InputTokenAccounting, ModelCost};
use rig::streaming::StreamFinal;

#[derive(Debug, Clone, Default)]
pub(crate) struct ObservedUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) total_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_write_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
}

impl From<rig::completion::Usage> for ObservedUsage {
    fn from(usage: rig::completion::Usage) -> Self {
        // Zero-default adapter fields cannot establish that a zero was reported.
        let known = |value| (value != 0).then_some(value);
        Self {
            input_tokens: known(usage.input_tokens),
            output_tokens: known(usage.output_tokens),
            total_tokens: known(usage.total_tokens),
            cache_read_tokens: known(usage.cached_input_tokens),
            cache_write_tokens: known(usage.cache_creation_input_tokens),
            reasoning_tokens: known(usage.reasoning_tokens),
        }
    }
}

pub(crate) fn input_accounting(api: ProviderApi) -> InputTokenAccounting {
    match api {
        ProviderApi::AnthropicMessages => InputTokenAccounting::ExcludesCache,
        ProviderApi::OpenAiResponses
        | ProviderApi::OpenAiCodexResponses
        | ProviderApi::OpenAiCompletions
        | ProviderApi::GeminiGenerateContent
        | ProviderApi::DeepSeekChat
        | ProviderApi::OllamaChat => InputTokenAccounting::IncludesCache,
    }
}

impl ObservedUsage {
    pub(crate) fn output_for_cost(&self, api: ProviderApi, output: u64) -> u64 {
        match api {
            ProviderApi::GeminiGenerateContent => {
                output.saturating_add(self.reasoning_tokens.unwrap_or_default())
            }
            _ => output,
        }
    }

    pub(crate) fn estimated_cost(&self, api: ProviderApi, rates: &ModelCost) -> Option<f64> {
        let (input, output, cache_read, cache_write) = (
            self.input_tokens?,
            self.output_tokens?,
            self.cache_read_tokens?,
            self.cache_write_tokens?,
        );
        if api == ProviderApi::GeminiGenerateContent {
            self.reasoning_tokens?;
        }
        let output = self.output_for_cost(api, output);
        let counts = [input, output, cache_read, cache_write];
        let known_rates = std::iter::once([
            rates.input,
            rates.output,
            rates.cache_read,
            rates.cache_write,
        ])
        .chain(
            rates
                .tiers
                .iter()
                .map(|tier| [tier.input, tier.output, tier.cache_read, tier.cache_write]),
        )
        .all(|rates| {
            counts
                .iter()
                .zip(rates)
                .all(|(count, rate)| rate.is_finite() && rate >= 0.0 && (*count == 0 || rate > 0.0))
        });
        if !known_rates {
            return None;
        }
        let cost = crate::models::estimate_usage_cost(
            input_accounting(api),
            rates,
            input,
            output,
            cache_read,
            cache_write,
        );
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }
}

pub(crate) fn observe_usage(
    terminal: &StreamFinal,
    api: ProviderApi,
    raw_limit: usize,
) -> ObservedUsage {
    let usage = terminal.usage;
    let mut observed = ObservedUsage::from(usage);
    if generation::serialized_size(&terminal.raw, raw_limit).is_err() {
        return observed;
    }
    use serde::Deserialize;
    match api {
        ProviderApi::OpenAiResponses | ProviderApi::OpenAiCodexResponses => {
            if let Ok(native) = rig::providers::openai::responses_api::ResponsesUsage::deserialize(
                &terminal.raw["usage"],
            ) {
                // Rig synthesizes all-zero Responses usage when the provider omits it.
                if native.input_tokens != 0 || native.output_tokens != 0 || native.total_tokens != 0
                {
                    observed.input_tokens = Some(native.input_tokens);
                    observed.output_tokens = Some(native.output_tokens);
                    observed.total_tokens = Some(native.total_tokens);
                    observed.cache_read_tokens = native
                        .input_tokens_details
                        .map(|details| details.cached_tokens);
                    observed.reasoning_tokens = native
                        .output_tokens_details
                        .map(|details| details.reasoning_tokens);
                }
            }
        }
        ProviderApi::OpenAiCompletions => {
            if let Ok(native) =
                rig::providers::openai::completion::Usage::deserialize(&terminal.raw["usage"])
            {
                if native.completion_tokens.is_some()
                    || native.prompt_tokens != 0
                    || native.total_tokens != 0
                {
                    observed.input_tokens = u64::try_from(native.prompt_tokens).ok();
                    observed.output_tokens = native
                        .completion_tokens
                        .and_then(|tokens| u64::try_from(tokens).ok());
                    observed.total_tokens = u64::try_from(native.total_tokens).ok();
                    observed.cache_read_tokens = native
                        .prompt_tokens_details
                        .and_then(|details| u64::try_from(details.cached_tokens).ok())
                        .or(observed.cache_read_tokens);
                    observed.reasoning_tokens = native
                        .completion_tokens_details
                        .and_then(|details| u64::try_from(details.reasoning_tokens).ok())
                        .or(observed.reasoning_tokens);
                }
            }
        }
        ProviderApi::OllamaChat => {
            if let Ok(native) =
                rig::providers::ollama::StreamingCompletionResponse::deserialize(&terminal.raw)
            {
                observed.input_tokens = native.prompt_eval_count;
                observed.output_tokens = native.eval_count;
                observed.total_tokens = native
                    .prompt_eval_count
                    .zip(native.eval_count)
                    .and_then(|(input, output)| input.checked_add(output));
            }
        }
        ProviderApi::AnthropicMessages => {
            if let Ok(native) =
                rig::providers::anthropic::streaming::StreamingCompletionResponse::deserialize(
                    &terminal.raw,
                )
            {
                if native.usage.input_tokens.is_some() {
                    observed.input_tokens = Some(usage.input_tokens);
                }
                observed.output_tokens = u64::try_from(native.usage.output_tokens).ok();
                observed.cache_read_tokens = native.usage.cache_read_input_tokens;
                observed.cache_write_tokens = native.usage.cache_creation_input_tokens;
                observed.reasoning_tokens = native
                    .usage
                    .output_tokens_details
                    .map(|details| details.thinking_tokens);
            }
        }
        ProviderApi::GeminiGenerateContent => {
            if let Ok(native) =
                rig::providers::gemini::streaming::StreamingCompletionResponse::deserialize(
                    &terminal.raw,
                )
            {
                observed.input_tokens = u64::try_from(native.usage_metadata.prompt_token_count)
                    .ok()
                    .filter(|value| *value > 0);
                observed.total_tokens = u64::try_from(native.usage_metadata.total_token_count)
                    .ok()
                    .filter(|value| *value > 0);
                observed.output_tokens = native
                    .usage_metadata
                    .candidates_token_count
                    .and_then(|value| u64::try_from(value).ok());
                observed.cache_read_tokens = native
                    .usage_metadata
                    .cached_content_token_count
                    .and_then(|value| u64::try_from(value).ok());
                observed.reasoning_tokens = native
                    .usage_metadata
                    .thoughts_token_count
                    .and_then(|value| u64::try_from(value).ok());
            }
        }
        ProviderApi::DeepSeekChat => {
            if let Ok(native) = rig::providers::deepseek::Usage::deserialize(&terminal.raw["usage"])
            {
                // Rig defaults and reserializes scalar counts, erasing zero presence.
                observed.cache_read_tokens = native
                    .prompt_tokens_details
                    .and_then(|details| details.cached_tokens)
                    .map(u64::from)
                    .or(observed.cache_read_tokens);
                observed.reasoning_tokens = native
                    .completion_tokens_details
                    .and_then(|details| details.reasoning_tokens)
                    .map(u64::from)
                    .or(observed.reasoning_tokens);
            }
        }
    }
    observed
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn native_presence_recovers_zero_without_guessing_defaulted_fields() {
        use serde_json::json;
        for (api, raw, expected) in [
            (
                ProviderApi::AnthropicMessages,
                json!({"usage": {"output_tokens": 0}}),
                [None, Some(0), None, None, None],
            ),
            (
                ProviderApi::OpenAiResponses,
                json!({"usage": {
                    "input_tokens": 7, "output_tokens": 0, "total_tokens": 7,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens_details": {"reasoning_tokens": 0}
                }}),
                [Some(7), Some(0), Some(7), Some(0), Some(0)],
            ),
            (
                ProviderApi::OpenAiCodexResponses,
                json!({"usage": {
                    "input_tokens": 7, "output_tokens": 0, "total_tokens": 7
                }}),
                [Some(7), Some(0), Some(7), None, None],
            ),
            (
                ProviderApi::OpenAiCompletions,
                json!({"usage": {
                    "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0
                }}),
                [Some(0), Some(0), Some(0), None, None],
            ),
            (
                ProviderApi::OpenAiCompletions,
                json!({"usage": {
                    "prompt_tokens": 0, "total_tokens": 0
                }}),
                [None; 5],
            ),
            (
                ProviderApi::OpenAiResponses,
                json!({"usage": {
                    "input_tokens": 0, "output_tokens": 0, "total_tokens": 0,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens_details": {"reasoning_tokens": 0}
                }}),
                [None; 5],
            ),
            (
                ProviderApi::GeminiGenerateContent,
                json!({"usage_metadata": {
                    "promptTokenCount": -1, "candidatesTokenCount": 0, "totalTokenCount": -1,
                    "cachedContentTokenCount": 0, "thoughtsTokenCount": 0
                }}),
                [None, Some(0), None, Some(0), Some(0)],
            ),
        ] {
            let terminal =
                StreamFinal::new("alias", rig::completion::Usage::default()).with_raw(raw);
            let observed = observe_usage(&terminal, api, 4096);
            assert_eq!(
                [
                    observed.input_tokens,
                    observed.output_tokens,
                    observed.total_tokens,
                    observed.cache_read_tokens,
                    observed.reasoning_tokens
                ],
                expected,
                "{api:?}"
            );
            assert_eq!(observed.cache_write_tokens, None);
            assert_eq!(observe_usage(&terminal, api, 1).output_tokens, None);
        }
    }

    #[test]
    fn cost_requires_observed_categories_and_known_rates() {
        let mut observed = ObservedUsage {
            input_tokens: Some(7),
            output_tokens: Some(2),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            ..Default::default()
        };
        let mut rates = ModelCost {
            input: 1.0,
            output: 2.0,
            ..Default::default()
        };
        let api = ProviderApi::AnthropicMessages;
        assert_eq!(
            observed.estimated_cost(api, &rates),
            Some(11.0 / 1_000_000.0)
        );
        observed.cache_write_tokens = None;
        assert_eq!(observed.estimated_cost(api, &rates), None);
        observed.cache_write_tokens = Some(0);
        rates.output = 0.0;
        assert_eq!(observed.estimated_cost(api, &rates), None);
    }

    #[test]
    fn gemini_thinking_is_billed_as_output_without_double_counting_other_protocols() {
        let observed = ObservedUsage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            reasoning_tokens: Some(3),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            ..Default::default()
        };
        let rates = ModelCost {
            input: 1.0,
            output: 1.0,
            ..Default::default()
        };
        let gemini = observed
            .estimated_cost(ProviderApi::GeminiGenerateContent, &rates)
            .unwrap();
        assert!((gemini - 15.0 / 1_000_000.0).abs() < f64::EPSILON);
        let openai = observed
            .estimated_cost(ProviderApi::OpenAiResponses, &rates)
            .unwrap();
        assert!((openai - 12.0 / 1_000_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn deepseek_defaulted_zeros_are_unknown_but_optional_details_retain_evidence() {
        for output in [None, Some(0), Some(2)] {
            let mut wire = serde_json::json!({
                "prompt_tokens": 10, "total_tokens": 10 + output.unwrap_or(0),
                "completion_tokens_details": {"reasoning_tokens": 0}
            });
            if let Some(output) = output {
                wire["completion_tokens"] = serde_json::json!(output);
            }
            let native: rig::providers::deepseek::Usage = serde_json::from_value(wire).unwrap();
            let terminal = StreamFinal::new("alias", (&native).into())
                .with_raw(serde_json::json!({"usage": native}));
            let observed = observe_usage(&terminal, ProviderApi::DeepSeekChat, 4096);
            assert_eq!(observed.input_tokens, Some(10));
            assert_eq!(observed.output_tokens, output.filter(|count| *count > 0));
            assert_eq!(observed.cache_read_tokens, None);
            assert_eq!(observed.reasoning_tokens, Some(0));
            assert_eq!(
                observe_usage(&terminal, ProviderApi::DeepSeekChat, 1).reasoning_tokens,
                None
            );
        }
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn cache_inclusive_protocols_charge_each_input_token_once(
            input in 1u64..10_000,
            output in 0u64..10_000,
            cache in 0u64..10_000,
        ) {
            let cache = cache.min(input);
            let observed = ObservedUsage {
                input_tokens: Some(input), output_tokens: Some(output),
                cache_read_tokens: Some(cache), cache_write_tokens: Some(0), reasoning_tokens: Some(0),
                ..Default::default()
            };
            let rates = ModelCost { input: 1.0, output: 1.0, cache_read: 1.0, ..Default::default() };
            for api in [ProviderApi::OpenAiResponses, ProviderApi::OpenAiCodexResponses,
                ProviderApi::OpenAiCompletions, ProviderApi::GeminiGenerateContent,
                ProviderApi::DeepSeekChat, ProviderApi::OllamaChat] {
                let cost = observed.estimated_cost(api, &rates).unwrap();
                prop_assert!((cost - (input + output) as f64 / 1_000_000.0).abs() < f64::EPSILON);
            }
            let cost = observed.estimated_cost(ProviderApi::AnthropicMessages, &rates).unwrap();
            prop_assert!((cost - (input + output + cache) as f64 / 1_000_000.0).abs() < f64::EPSILON);
        }

        #[test]
        fn missing_native_usage_never_fabricates_reported_zeros(
            counts in prop::array::uniform6(0u64..1000),
        ) {
            let usage = rig::completion::Usage {
                input_tokens: counts[0],
                output_tokens: counts[1],
                total_tokens: counts[2],
                cached_input_tokens: counts[3],
                cache_creation_input_tokens: counts[4],
                reasoning_tokens: counts[5],
                ..Default::default()
            };
            let terminal = StreamFinal::new("test", usage);
            for api in [
                ProviderApi::OpenAiResponses,
                ProviderApi::OpenAiCodexResponses,
                ProviderApi::OpenAiCompletions,
                ProviderApi::AnthropicMessages,
                ProviderApi::GeminiGenerateContent,
                ProviderApi::OllamaChat,
                ProviderApi::DeepSeekChat,
            ] {
                let observed = observe_usage(&terminal, api, 4096);
                let fields = [
                    observed.input_tokens,
                    observed.output_tokens,
                    observed.total_tokens,
                    observed.cache_read_tokens,
                    observed.cache_write_tokens,
                    observed.reasoning_tokens,
                ];
                for (reported, count) in fields.into_iter().zip(counts) {
                    prop_assert_eq!(reported, (count != 0).then_some(count));
                }
            }
        }

        #[test]
        fn native_zero_and_missing_output_remain_distinct(input in 0u64..1000) {
            let terminal = StreamFinal::new("ollama", rig::completion::Usage::default())
                .with_raw(serde_json::json!({"model": "test", "prompt_eval_count": input, "eval_count": 0}));
            let observed = observe_usage(&terminal, ProviderApi::OllamaChat, 4096);
            prop_assert_eq!(observed.input_tokens, Some(input));
            prop_assert_eq!(observed.output_tokens, Some(0));
            prop_assert_eq!(observed.total_tokens, Some(input));
            let missing = StreamFinal::new("ollama", rig::completion::Usage::default())
                .with_raw(serde_json::json!({"model": "test", "prompt_eval_count": input}));
            let observed = observe_usage(&missing, ProviderApi::OllamaChat, 4096);
            prop_assert_eq!(observed.input_tokens, Some(input));
            prop_assert_eq!(observed.output_tokens, None);
            prop_assert_eq!(observed.total_tokens, None);
            prop_assert_eq!(observed.estimated_cost(ProviderApi::OllamaChat, &ModelCost::default()), None);
        }
    }
}
