use crate::config::{CustomProviderConfig, RuntimeConfig};
use std::collections::HashMap;

mod auth;
mod backend;
mod error;
pub(crate) mod generation;
pub(crate) mod transport;
pub(crate) mod usage;

pub(crate) use backend::{
    AnyClient, AnyRuntime, ProviderHistoryPolicy, build_runtime, create_client,
};
pub(crate) use error::{ProviderErrorKind, error_facts};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAdapter {
    OpenRouter,
    OpenAI,
    ChatGPT,
    Anthropic,
    Gemini,
    DeepSeek,
    Ollama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub enum ProviderApi {
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-codex-responses")]
    OpenAiCodexResponses,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "gemini-generate-content")]
    GeminiGenerateContent,
    #[serde(rename = "deepseek-chat")]
    DeepSeekChat,
    #[serde(rename = "ollama-chat")]
    OllamaChat,
}

impl ProviderApi {
    pub(crate) fn supports_openai_reasoning(self) -> bool {
        matches!(self, Self::OpenAiResponses | Self::OpenAiCodexResponses)
    }
}

pub fn parse_provider_adapter(name: &str) -> Option<ProviderAdapter> {
    match name.to_lowercase().as_str() {
        "openrouter" => Some(ProviderAdapter::OpenRouter),
        "openai" => Some(ProviderAdapter::OpenAI),
        "chatgpt" | "openai-codex" => Some(ProviderAdapter::ChatGPT),
        "anthropic" => Some(ProviderAdapter::Anthropic),
        "google" => Some(ProviderAdapter::Gemini),
        "deepseek" => Some(ProviderAdapter::DeepSeek),
        "ollama" => Some(ProviderAdapter::Ollama),
        _ => None,
    }
}

pub struct Provider {
    id: String,
    adapter: ProviderAdapter,
    api: ProviderApi,
    base_url: Option<String>,
    api_key: Option<String>,
}

impl Provider {
    pub fn id(&self) -> &str {
        &self.id
    }

    #[cfg(test)]
    fn adapter(&self) -> ProviderAdapter {
        self.adapter
    }

    #[cfg(test)]
    fn api(&self) -> ProviderApi {
        self.api
    }
}

pub fn resolve_provider(
    name: &str,
    custom_providers: &HashMap<String, CustomProviderConfig>,
) -> Option<Provider> {
    let custom = custom_providers.get(name);
    let (adapter, api) = match custom {
        Some(provider) => {
            let adapter = parse_provider_adapter(name)
                .filter(|adapter| default_api(*adapter) == provider.api)
                .unwrap_or_else(|| adapter_for_api(provider.api));
            (adapter, provider.api)
        }
        None => {
            let adapter = parse_provider_adapter(name)?;
            (adapter, default_api(adapter))
        }
    };
    Some(Provider {
        id: name.to_string(),
        adapter,
        api,
        base_url: custom.and_then(|provider| provider.base_url.clone()),
        api_key: custom.and_then(|provider| provider.api_key.clone()),
    })
}

fn adapter_for_api(api: ProviderApi) -> ProviderAdapter {
    match api {
        ProviderApi::OpenAiResponses | ProviderApi::OpenAiCompletions => ProviderAdapter::OpenAI,
        ProviderApi::OpenAiCodexResponses => ProviderAdapter::ChatGPT,
        ProviderApi::AnthropicMessages => ProviderAdapter::Anthropic,
        ProviderApi::GeminiGenerateContent => ProviderAdapter::Gemini,
        ProviderApi::DeepSeekChat => ProviderAdapter::DeepSeek,
        ProviderApi::OllamaChat => ProviderAdapter::Ollama,
    }
}

fn default_api(adapter: ProviderAdapter) -> ProviderApi {
    match adapter {
        ProviderAdapter::OpenRouter => ProviderApi::OpenAiCompletions,
        ProviderAdapter::OpenAI => ProviderApi::OpenAiResponses,
        ProviderAdapter::ChatGPT => ProviderApi::OpenAiCodexResponses,
        ProviderAdapter::Anthropic => ProviderApi::AnthropicMessages,
        ProviderAdapter::Gemini => ProviderApi::GeminiGenerateContent,
        ProviderAdapter::DeepSeek => ProviderApi::DeepSeekChat,
        ProviderAdapter::Ollama => ProviderApi::OllamaChat,
    }
}

fn provider_env_var(adapter: ProviderAdapter) -> &'static str {
    match adapter {
        ProviderAdapter::OpenAI => "OPENAI_API_KEY",
        ProviderAdapter::ChatGPT => "",
        ProviderAdapter::Anthropic => "ANTHROPIC_API_KEY",
        ProviderAdapter::Gemini => "GEMINI_API_KEY",
        ProviderAdapter::DeepSeek => "DEEPSEEK_API_KEY",
        ProviderAdapter::Ollama => "OLLAMA_API_KEY",
        ProviderAdapter::OpenRouter => "OPENROUTER_API_KEY",
    }
}

fn resolve_api_key(
    adapter: ProviderAdapter,
    configured: Option<&str>,
    cli_key: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(key) = cli_key.filter(|key| !key.is_empty()) {
        tracing::warn!(
            "API key provided via --api-key is visible in process listings (/proc/*/cmdline). Use the {} environment variable instead.",
            provider_env_var(adapter)
        );
        return Ok(key.to_string());
    }
    if let Some(value) = configured {
        return resolve_config_value(value);
    }
    Ok(std::env::var(provider_env_var(adapter)).unwrap_or_default())
}

pub fn api_key_available(provider: &str, cfg: &RuntimeConfig) -> bool {
    if matches!(provider, "chatgpt" | "openai-codex") {
        return true;
    }
    if let Some(custom) = cfg.custom_providers.get(provider) {
        if custom.api == ProviderApi::OpenAiCodexResponses {
            return true;
        }
        return custom
            .api_key
            .as_deref()
            .map(config_value_available)
            .unwrap_or_else(|| api_env_var(custom.api).is_some_and(has_env_key));
    }
    builtin_api_env_var(provider).is_some_and(has_env_key)
}

fn builtin_api_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "ollama" => Some("OLLAMA_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        _ => None,
    }
}

fn api_env_var(api: ProviderApi) -> Option<&'static str> {
    match api {
        ProviderApi::OpenAiResponses | ProviderApi::OpenAiCompletions => Some("OPENAI_API_KEY"),
        ProviderApi::OpenAiCodexResponses => None,
        ProviderApi::AnthropicMessages => Some("ANTHROPIC_API_KEY"),
        ProviderApi::GeminiGenerateContent => Some("GEMINI_API_KEY"),
        ProviderApi::DeepSeekChat => Some("DEEPSEEK_API_KEY"),
        ProviderApi::OllamaChat => Some("OLLAMA_API_KEY"),
    }
}

fn has_env_key(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty())
}

fn config_value_available(value: &str) -> bool {
    resolve_config_value(value).is_ok_and(|value| !value.is_empty())
}

fn resolve_config_value(value: &str) -> anyhow::Result<String> {
    let mut resolved = String::new();
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '$' {
            resolved.push(character);
            continue;
        }
        if chars.next_if_eq(&'$').is_some() {
            resolved.push('$');
            continue;
        }
        let name = if chars.next_if_eq(&'{').is_some() {
            let mut name = String::new();
            let mut closed = false;
            for character in chars.by_ref() {
                if character == '}' {
                    closed = true;
                    break;
                }
                name.push(character);
            }
            if !closed || name.is_empty() {
                anyhow::bail!("apiKey contains an invalid environment variable reference");
            }
            name
        } else {
            let mut name = String::new();
            while chars
                .peek()
                .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
            {
                name.push(chars.next().unwrap());
            }
            if name.is_empty() {
                resolved.push('$');
                continue;
            }
            name
        };
        let variable = std::env::var(&name)
            .map_err(|_| anyhow::anyhow!("apiKey environment variable {name} is not set"))?;
        resolved.push_str(&variable);
    }
    Ok(resolved)
}

// Rig stores a flat auth record and overwrites the whole file on login/refresh.
// Separate auth/<provider>.json files prevent providers from overwriting each other;
// Rig's auth_file API cannot manage a shared, provider-keyed auth.json.
// Rig 0.42.0 AuthRecord (L29-36):
// https://docs.rs/rig-core/0.42.0/src/rig_core/providers/chatgpt/auth/native.rs.html#29-36
// Whole-file writes (L64-74):
// https://docs.rs/rig-core/0.42.0/src/rig_core/providers/internal/device_auth.rs.html#64-74
fn provider_auth_file(provider_name: &str) -> anyhow::Result<std::path::PathBuf> {
    anyhow::ensure!(
        !matches!(provider_name, "" | "." | "..") && !provider_name.contains(['/', '\\']),
        "invalid OAuth provider name"
    );
    let auth_file = std::env::var_os("PHI_AUTH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::utils::path::phi_config_dir().join("auth"))
        .join(format!("{provider_name}.json"));
    if let Some(parent) = auth_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(auth_file)
}

// Rig's ChatGPT client uses fixed-client Codex device OAuth, refreshes cached tokens,
// and sends subscription requests to https://chatgpt.com/backend-api/codex.
// This is not the newer unified OpenAI/ChatGPT OAuth flow targeting api.openai.com.
// Rig 0.42.0 device OAuth endpoints/client ID (L12-17) and token refresh (L220-237):
// https://docs.rs/rig-core/0.42.0/src/rig_core/providers/chatgpt/auth/native.rs.html#12-17
// https://docs.rs/rig-core/0.42.0/src/rig_core/providers/chatgpt/auth/native.rs.html#220-237
// Codex backend URL (L33):
// https://docs.rs/rig-core/0.42.0/src/rig_core/providers/chatgpt/mod.rs.html#33
fn create_chatgpt_client(provider_name: &str) -> anyhow::Result<auth::ChatGPTClient> {
    Ok(auth::ChatGPTClient::new(provider_auth_file(provider_name)?))
}

pub async fn login_chatgpt<F>(provider_name: &str, on_device_code: F) -> anyhow::Result<()>
where
    F: Fn(String, String) + Send + Sync + 'static,
{
    auth::AuthStore::new(provider_auth_file(provider_name)?)
        .login(on_device_code)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_provider_adapter_and_api() {
        let providers = HashMap::from([
            (
                "compatible".to_string(),
                CustomProviderConfig {
                    api: ProviderApi::OpenAiCompletions,
                    base_url: Some("http://localhost".to_string()),
                    api_key: None,
                },
            ),
            (
                "responses-compatible".to_string(),
                CustomProviderConfig {
                    api: ProviderApi::OpenAiResponses,
                    base_url: Some("http://localhost".to_string()),
                    api_key: None,
                },
            ),
            (
                "openrouter".to_string(),
                CustomProviderConfig {
                    api: ProviderApi::OpenAiCompletions,
                    base_url: None,
                    api_key: Some("$OPENROUTER_API_KEY".to_string()),
                },
            ),
        ]);

        for (name, adapter, api) in [
            (
                "openrouter",
                ProviderAdapter::OpenRouter,
                ProviderApi::OpenAiCompletions,
            ),
            (
                "openai",
                ProviderAdapter::OpenAI,
                ProviderApi::OpenAiResponses,
            ),
            (
                "compatible",
                ProviderAdapter::OpenAI,
                ProviderApi::OpenAiCompletions,
            ),
            (
                "responses-compatible",
                ProviderAdapter::OpenAI,
                ProviderApi::OpenAiResponses,
            ),
            (
                "chatgpt",
                ProviderAdapter::ChatGPT,
                ProviderApi::OpenAiCodexResponses,
            ),
            (
                "anthropic",
                ProviderAdapter::Anthropic,
                ProviderApi::AnthropicMessages,
            ),
            (
                "google",
                ProviderAdapter::Gemini,
                ProviderApi::GeminiGenerateContent,
            ),
            (
                "deepseek",
                ProviderAdapter::DeepSeek,
                ProviderApi::DeepSeekChat,
            ),
            ("ollama", ProviderAdapter::Ollama, ProviderApi::OllamaChat),
        ] {
            let provider = resolve_provider(name, &providers).unwrap();
            assert_eq!(provider.adapter(), adapter);
            assert_eq!(provider.api(), api);
        }
    }

    #[test]
    fn resolves_api_key_availability_from_provider_configuration() {
        let mut cfg = RuntimeConfig::default();
        cfg.custom_providers.insert(
            "configured".to_string(),
            CustomProviderConfig {
                api: ProviderApi::OpenAiResponses,
                base_url: None,
                api_key: Some("literal-key".to_string()),
            },
        );
        cfg.custom_providers.insert(
            "missing".to_string(),
            CustomProviderConfig {
                api: ProviderApi::AnthropicMessages,
                base_url: None,
                api_key: Some("$PHI_TEST_MISSING_API_KEY".to_string()),
            },
        );

        assert!(api_key_available("configured", &cfg));
        assert!(api_key_available("openai-codex", &cfg));
        assert!(!api_key_available("missing", &cfg));
        assert!(!api_key_available("unknown", &cfg));
    }
}
