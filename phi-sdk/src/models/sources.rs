use std::collections::BTreeMap;

use serde::Deserialize;

use super::schema::{
    ModelCost, ModelCostTier, ModelDescriptor, ModelInput, ProviderModels, ThinkingLevel,
    ThinkingLevelMap,
};
use crate::provider::ProviderApi;

pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";
pub const OPENAI_CODEX_MODELS_URL: &str =
    "https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json";
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

pub type NormalizedCatalog = BTreeMap<String, ProviderModels>;

const MIN_CODEX_VERSION: (u64, u64) = (5, 5);

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: BTreeMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    name: Option<String>,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<ReasoningOption>,
    modalities: Option<Modalities>,
    limit: Option<ModelLimits>,
    cost: Option<ModelsDevCost>,
}

#[derive(Debug, Deserialize)]
struct ReasoningOption {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    values: Vec<Option<String>>,
}

#[derive(Debug, Deserialize)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelLimits {
    context: Option<u64>,
    output: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    #[serde(default)]
    tiers: Vec<ModelsDevCostTier>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCostTier {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tier: Option<ModelsDevTierCondition>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevTierCondition {
    #[serde(rename = "type")]
    kind: String,
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CodexResponse {
    #[serde(default)]
    models: Vec<CodexModel>,
}

#[derive(Debug, Deserialize)]
struct CodexModel {
    slug: String,
    display_name: String,
    visibility: String,
    context_window: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexReasoningLevel>,
}

#[derive(Debug, Deserialize)]
struct CodexReasoningLevel {
    effort: String,
}

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    #[serde(default)]
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    name: Option<String>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    architecture: Option<OpenRouterArchitecture>,
    pricing: Option<OpenRouterPricing>,
    top_provider: Option<OpenRouterTopProvider>,
    context_length: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    modality: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    prompt: Option<String>,
    completion: Option<String>,
    input_cache_read: Option<String>,
    input_cache_write: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    context_length: Option<u64>,
    max_completion_tokens: Option<u64>,
}

pub(crate) fn normalize_models_dev_bytes(bytes: &[u8]) -> anyhow::Result<NormalizedCatalog> {
    normalize_models_dev(serde_json::from_slice(bytes)?)
}

pub(crate) fn normalize_openrouter_bytes(bytes: &[u8]) -> anyhow::Result<ProviderModels> {
    normalize_openrouter(serde_json::from_slice(bytes)?)
}

pub(crate) fn normalize_codex_bytes(
    bytes: &[u8],
    openai_models: &ProviderModels,
) -> anyhow::Result<ProviderModels> {
    normalize_codex(serde_json::from_slice(bytes)?, openai_models)
}

fn normalize_models_dev(
    response: BTreeMap<String, ModelsDevProvider>,
) -> anyhow::Result<NormalizedCatalog> {
    let mut catalog = BTreeMap::new();
    for (source, provider, api) in [
        ("openai", "openai", ProviderApi::OpenAiResponses),
        ("anthropic", "anthropic", ProviderApi::AnthropicMessages),
        ("google", "google", ProviderApi::GeminiGenerateContent),
        ("deepseek", "deepseek", ProviderApi::DeepSeekChat),
    ] {
        let source_models = response
            .get(source)
            .map(|entry| &entry.models)
            .ok_or_else(|| anyhow::anyhow!("models.dev response is missing provider {source}"))?;
        let mut models = ProviderModels::new();
        for (id, model) in source_models {
            if !model.tool_call {
                continue;
            }
            let mut descriptor = ModelDescriptor {
                provider: provider.to_string(),
                id: id.clone(),
                name: model.name.clone().unwrap_or_else(|| id.clone()),
                api,
                reasoning: model.reasoning,
                thinking_level_map: thinking_level_map(model),
                input: model_inputs(
                    model
                        .modalities
                        .as_ref()
                        .is_some_and(|value| value.input.iter().any(|item| item == "image")),
                ),
                context_window: model
                    .limit
                    .as_ref()
                    .and_then(|limit| limit.context)
                    .unwrap_or(4096),
                max_output_tokens: model
                    .limit
                    .as_ref()
                    .and_then(|limit| limit.output)
                    .unwrap_or(4096),
                cost: models_dev_cost(model.cost.as_ref()),
            };
            apply_corrections(&mut descriptor);
            models.insert(id.clone(), descriptor);
        }
        catalog.insert(provider.to_string(), models);
    }
    Ok(catalog)
}

fn normalize_openrouter(response: OpenRouterResponse) -> anyhow::Result<ProviderModels> {
    let mut models = ProviderModels::new();
    for model in response.data {
        if !has_parameter(&model, "tools") {
            continue;
        }
        let context_window = model
            .top_provider
            .as_ref()
            .and_then(|provider| provider.context_length)
            .or(model.context_length)
            .unwrap_or(4096);
        let max_output_tokens = model
            .top_provider
            .as_ref()
            .and_then(|provider| provider.max_completion_tokens)
            .unwrap_or(4096);
        let reasoning = has_parameter(&model, "reasoning");
        let cost = openrouter_cost(model.pricing.as_ref())?;
        if !valid_rates(&cost) {
            continue;
        }
        let mut descriptor = ModelDescriptor {
            provider: "openrouter".to_string(),
            id: model.id.clone(),
            name: model.name.unwrap_or_else(|| model.id.clone()),
            api: ProviderApi::OpenAiCompletions,
            reasoning,
            thinking_level_map: None,
            input: model_inputs(
                model
                    .architecture
                    .as_ref()
                    .and_then(|value| value.modality.as_deref())
                    .is_some_and(|value| value.contains("image")),
            ),
            context_window,
            max_output_tokens,
            cost,
        };
        apply_corrections(&mut descriptor);
        models.insert(model.id, descriptor);
    }
    Ok(models)
}

fn normalize_codex(
    response: CodexResponse,
    openai_models: &ProviderModels,
) -> anyhow::Result<ProviderModels> {
    let mut models = ProviderModels::new();
    for model in response.models {
        if model.visibility != "list" || !is_supported_codex_version(&model.slug) {
            continue;
        }
        let source = openai_models
            .get(&model.slug)
            .ok_or_else(|| anyhow::anyhow!("models.dev is missing Codex model {}", model.slug))?;
        let descriptor = ModelDescriptor {
            provider: "openai-codex".to_string(),
            id: model.slug.clone(),
            name: model.display_name,
            api: ProviderApi::OpenAiCodexResponses,
            reasoning: !model.supported_reasoning_levels.is_empty(),
            thinking_level_map: codex_thinking_level_map(&model.supported_reasoning_levels),
            input: model_inputs(
                model
                    .input_modalities
                    .iter()
                    .any(|modality| modality == "image"),
            ),
            context_window: model.context_window.unwrap_or(source.context_window),
            max_output_tokens: source.max_output_tokens,
            cost: source.cost.clone(),
        };
        models.insert(model.slug, descriptor);
    }
    Ok(models)
}

fn is_supported_codex_version(id: &str) -> bool {
    let Some(version) = id.strip_prefix("gpt-") else {
        return false;
    };
    let mut parts = version.split(['.', '-']);
    let Some(major) = parts.next().and_then(|part| part.parse().ok()) else {
        return false;
    };
    let minor = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    (major, minor) >= MIN_CODEX_VERSION
}

fn codex_thinking_level_map(levels: &[CodexReasoningLevel]) -> Option<ThinkingLevelMap> {
    let levels = levels
        .iter()
        .filter_map(|level| {
            parse_thinking_level(&level.effort).map(|parsed| (parsed, Some(level.effort.clone())))
        })
        .collect::<ThinkingLevelMap>();
    (!levels.is_empty()).then_some(levels)
}

fn model_inputs(has_image: bool) -> Vec<ModelInput> {
    if has_image {
        vec![ModelInput::Text, ModelInput::Image]
    } else {
        vec![ModelInput::Text]
    }
}

fn models_dev_cost(cost: Option<&ModelsDevCost>) -> ModelCost {
    let Some(cost) = cost else {
        return ModelCost::default();
    };
    let tiers = cost
        .tiers
        .iter()
        .filter_map(|tier| {
            let condition = tier.tier.as_ref()?;
            (condition.kind == "context").then_some(ModelCostTier {
                input_tokens_above: condition.size?,
                input: tier.input.unwrap_or_default(),
                output: tier.output.unwrap_or_default(),
                cache_read: tier.cache_read.unwrap_or_default(),
                cache_write: tier.cache_write.unwrap_or_default(),
            })
        })
        .collect();
    ModelCost {
        input: cost.input.unwrap_or_default(),
        output: cost.output.unwrap_or_default(),
        cache_read: cost.cache_read.unwrap_or_default(),
        cache_write: cost.cache_write.unwrap_or_default(),
        tiers,
    }
}

fn openrouter_cost(pricing: Option<&OpenRouterPricing>) -> anyhow::Result<ModelCost> {
    let rate = |value: Option<&String>| -> anyhow::Result<f64> {
        value
            .map(|value| value.parse::<f64>())
            .transpose()
            .map(|value| value.unwrap_or_default() * 1_000_000.0)
            .map_err(Into::into)
    };
    Ok(ModelCost {
        input: rate(pricing.and_then(|value| value.prompt.as_ref()))?,
        output: rate(pricing.and_then(|value| value.completion.as_ref()))?,
        cache_read: rate(pricing.and_then(|value| value.input_cache_read.as_ref()))?,
        cache_write: rate(pricing.and_then(|value| value.input_cache_write.as_ref()))?,
        tiers: Vec::new(),
    })
}

fn valid_rates(cost: &ModelCost) -> bool {
    [cost.input, cost.output, cost.cache_read, cost.cache_write]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
}

fn thinking_level_map(model: &ModelsDevModel) -> Option<ThinkingLevelMap> {
    if !model.reasoning {
        return None;
    }
    let values = model
        .reasoning_options
        .iter()
        .find(|option| option.kind == "effort")?
        .values
        .as_slice();
    let mut levels = ThinkingLevelMap::new();
    for level in [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ] {
        levels.insert(level, None);
    }
    for value in values.iter().flatten() {
        if let Some(level) = parse_thinking_level(value) {
            levels.insert(level, Some(value.clone()));
        }
    }
    Some(levels)
}

fn parse_thinking_level(value: &str) -> Option<ThinkingLevel> {
    match value.to_ascii_lowercase().as_str() {
        "none" | "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

fn has_parameter(model: &OpenRouterModel, parameter: &str) -> bool {
    model
        .supported_parameters
        .iter()
        .any(|item| item == parameter)
}

fn apply_corrections(model: &mut ModelDescriptor) {
    if model.provider == "openai" && model.id == "gpt-5-pro" {
        model.max_output_tokens = 128_000;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_models_dev_and_filters_models_without_tools() {
        let response = serde_json::from_value(serde_json::json!({
            "openai": { "models": {
                "gpt-5-pro": {
                    "name": "GPT-5 Pro", "tool_call": true, "reasoning": true,
                    "reasoning_options": [{ "type": "effort", "values": ["none", "high"] }],
                    "modalities": { "input": ["text", "image"] },
                    "limit": { "context": 100000, "output": 10000 },
                    "cost": { "input": 1.0, "output": 2.0, "tiers": [
                        { "input": 2.0, "output": 3.0, "tier": { "type": "context", "size": 50000 } }
                    ] }
                },
                "gpt-5.6-sol": {
                    "name": "GPT-5.6 Sol", "tool_call": true, "reasoning": true,
                    "reasoning_options": [{ "type": "effort", "values": ["low", "max"] }],
                    "modalities": { "input": ["text", "image"] },
                    "limit": { "context": 1050000, "output": 128000 },
                    "cost": { "input": 4.0, "output": 20.0 }
                },
                "no-tools": { "tool_call": false }
            }},
            "anthropic": { "models": {} },
            "google": { "models": {} },
            "deepseek": { "models": {} }
        }))
        .unwrap();

        let catalog = normalize_models_dev(response).unwrap();
        let model = &catalog["openai"]["gpt-5-pro"];

        assert_eq!(model.input, vec![ModelInput::Text, ModelInput::Image]);
        assert_eq!(model.cost.tiers[0].input_tokens_above, 50_000);
        assert_eq!(model.max_output_tokens, 128_000);
        assert!(!catalog["openai"].contains_key("no-tools"));
        assert_eq!(catalog["google"].len(), 0);
    }

    #[test]
    fn builds_codex_catalog_from_official_models_and_models_dev_metadata() {
        let models_dev = serde_json::from_value(serde_json::json!({
            "gpt-5.6-sol": {
                "provider": "openai", "id": "gpt-5.6-sol", "name": "GPT-5.6 Sol",
                "api": "openai-responses", "reasoning": true,
                "input": ["text", "image"], "contextWindow": 1050000,
                "maxOutputTokens": 128000, "cost": { "input": 4.0, "output": 20.0 }
            }
        }))
        .unwrap();
        let response = serde_json::from_value(serde_json::json!({ "models": [
            {
                "slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol",
                "visibility": "list", "context_window": 272000,
                "input_modalities": ["text", "image"],
                "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "max" }]
            },
            { "slug": "gpt-5.4", "display_name": "GPT-5.4", "visibility": "list" },
            { "slug": "gpt-6-hidden", "display_name": "Hidden", "visibility": "hide" }
        ] }))
        .unwrap();

        let models = normalize_codex(response, &models_dev).unwrap();
        let sol = &models["gpt-5.6-sol"];

        assert_eq!(models.len(), 1);
        assert_eq!(sol.provider, "openai-codex");
        assert_eq!(sol.api, ProviderApi::OpenAiCodexResponses);
        assert_eq!(sol.context_window, 272_000);
        assert_eq!(sol.max_output_tokens, 128_000);
        assert_eq!(sol.cost.input, 4.0);
        assert_eq!(
            sol.thinking_level_map.as_ref().unwrap()[&ThinkingLevel::Max],
            Some("max".to_string())
        );
        assert!(is_supported_codex_version("gpt-6-astra"));
        assert!(!is_supported_codex_version("gpt-5.4-mini"));
    }

    #[test]
    fn normalizes_openrouter_prices_per_million_tokens() {
        let response = serde_json::from_value(serde_json::json!({ "data": [{
            "id": "vendor/model", "name": "Model",
            "supported_parameters": ["tools", "reasoning"],
            "architecture": { "modality": "text+image->text" },
            "pricing": { "prompt": "0.000001", "completion": "0.000002" },
            "context_length": 64000,
            "top_provider": { "max_completion_tokens": 8000 }
        }] }))
        .unwrap();

        let models = normalize_openrouter(response).unwrap();
        let model = &models["vendor/model"];

        assert_eq!(model.cost.input, 1.0);
        assert_eq!(model.cost.output, 2.0);
        assert!(model.reasoning);
        assert!(model.input.contains(&ModelInput::Image));
    }
}
