mod catalog;
mod schema;
mod sources;
mod update;

pub(crate) use catalog::canonical_provider;
pub use catalog::{
    CatalogDiagnostic, CatalogSnapshot, ModelCatalog, ModelCatalogOptions, ModelDiscoveryResult,
};
pub use schema::{ModelCost, ModelCostTier, ModelDescriptor, ModelInput, ThinkingLevel};
pub use update::{CatalogUpdateError, CatalogUpdateResult};

use crate::config::RuntimeConfig;

pub fn provider_model_ids(catalog: &ModelCatalog, provider: &str) -> Vec<String> {
    catalog.model_ids(provider)
}

pub fn estimate_cost(
    catalog: &ModelCatalog,
    provider: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    let Some(descriptor) = catalog.get(provider, model) else {
        return estimate_model_cost(
            provider,
            &ModelCost::default(),
            input_tokens,
            output_tokens,
            cache_read,
            cache_write,
        );
    };
    estimate_usage_cost(
        crate::provider::usage::input_accounting(descriptor.api),
        &descriptor.cost,
        input_tokens,
        output_tokens,
        cache_read,
        cache_write,
    )
}

pub fn estimate_model_cost(
    provider: &str,
    cost: &ModelCost,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    let accounting = match provider {
        "openai" | "openai-codex" | "chatgpt" => InputTokenAccounting::IncludesCache,
        _ => InputTokenAccounting::ExcludesCache,
    };
    estimate_usage_cost(
        accounting,
        cost,
        input_tokens,
        output_tokens,
        cache_read,
        cache_write,
    )
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum InputTokenAccounting {
    IncludesCache,
    ExcludesCache,
}

impl InputTokenAccounting {
    pub(crate) fn total_input(self, input: u64, cache_read: u64, cache_write: u64) -> u64 {
        match self {
            Self::IncludesCache => input,
            Self::ExcludesCache => input.saturating_add(cache_read).saturating_add(cache_write),
        }
    }
}

pub(crate) fn estimate_usage_cost(
    accounting: InputTokenAccounting,
    cost: &ModelCost,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    let billable_input = match accounting {
        InputTokenAccounting::IncludesCache => input_tokens
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        InputTokenAccounting::ExcludesCache => input_tokens,
    };
    let request_input = billable_input
        .saturating_add(cache_read)
        .saturating_add(cache_write);
    let rates = rates_for_tokens(&cost, request_input);
    cost_for_tokens(billable_input, rates.input)
        + cost_for_tokens(output_tokens, rates.output)
        + cost_for_tokens(cache_read, rates.cache_read)
        + cost_for_tokens(cache_write, rates.cache_write)
}

pub fn available_provider_model_ids(
    catalog: &ModelCatalog,
    cfg: &RuntimeConfig,
    current_provider: &str,
) -> Vec<(String, String)> {
    available_providers(catalog, cfg, current_provider)
        .into_iter()
        .flat_map(|provider| {
            catalog
                .model_ids(&provider)
                .into_iter()
                .map(move |model| (provider.clone(), model))
        })
        .collect()
}

pub fn available_providers(
    catalog: &ModelCatalog,
    cfg: &RuntimeConfig,
    current_provider: &str,
) -> Vec<String> {
    let mut providers = Vec::new();
    push_if_available(&mut providers, current_provider);
    for provider in [
        "openrouter",
        "openai",
        "openai-codex",
        "anthropic",
        "google",
        "deepseek",
        "ollama",
    ] {
        if (catalog.provider_ids().iter().any(|known| known == provider)
            || matches!(provider, "openai-codex" | "ollama"))
            && crate::provider::api_key_available(provider, cfg)
        {
            push_if_available(&mut providers, provider);
        }
    }
    for name in cfg.custom_providers.keys() {
        if crate::provider::api_key_available(name, cfg) {
            push_if_available(&mut providers, name);
        }
    }
    providers
}

#[derive(Clone, Copy)]
struct CostRates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn rates_for_tokens(cost: &ModelCost, input_tokens: u64) -> CostRates {
    match cost
        .tiers
        .iter()
        .rev()
        .find(|tier| input_tokens > tier.input_tokens_above)
    {
        Some(tier) => tier.into(),
        None => cost.into(),
    }
}

impl From<&ModelCost> for CostRates {
    fn from(cost: &ModelCost) -> Self {
        Self {
            input: cost.input,
            output: cost.output,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        }
    }
}

impl From<&ModelCostTier> for CostRates {
    fn from(cost: &ModelCostTier) -> Self {
        Self {
            input: cost.input,
            output: cost.output,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        }
    }
}

fn cost_for_tokens(tokens: u64, price_per_million: f64) -> f64 {
    (tokens as f64 * price_per_million) / 1_000_000.0
}

fn push_if_available(providers: &mut Vec<String>, provider: &str) {
    if !provider.is_empty() && !providers.iter().any(|known| known == provider) {
        providers.push(provider.to_string());
    }
}
