//! Provider configuration — build a [`ModelRouter`] from environment and files.
//!
//! Supports:
//! - `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`
//! - `OPENAI_API_KEY` / `OPENAI_BASE_URL`
//! - `GROQ_API_KEY` / `GROQ_BASE_URL`
//! - Config file at `~/.coderex/providers.toml`

use crate::api_client::ApiConfig;
use crate::model_router::ModelRouter;
use crate::providers::{AnthropicProvider, OpenAiCompatibleProvider};
use std::collections::{BTreeMap, HashMap};

/// Configuration for all providers.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Anthropic API configuration.
    pub anthropic: Option<ProviderEndpoint>,
    /// OpenAI API configuration.
    pub openai: Option<ProviderEndpoint>,
    /// Groq API configuration.
    pub groq: Option<ProviderEndpoint>,
    /// Additional custom providers.
    pub custom: Vec<NamedProviderEndpoint>,
    /// Explicit fallback provider allowlist/order.
    pub fallback_providers: Vec<String>,
}

/// A provider endpoint configuration.
#[derive(Debug, Clone)]
pub struct ProviderEndpoint {
    /// API key.
    pub api_key: String,
    /// Base URL (optional — uses provider default if not set).
    pub base_url: Option<String>,
    /// Provider profile hint used by API transport.
    pub provider_profile: Option<String>,
    /// Additional custom headers for this endpoint.
    pub custom_headers: BTreeMap<String, String>,
}

/// A named custom provider endpoint.
#[derive(Debug, Clone)]
pub struct NamedProviderEndpoint {
    /// Provider name.
    pub name: String,
    /// Base URL.
    pub base_url: String,
    /// API key.
    pub api_key: String,
    /// Model mapping (generic model -> provider model).
    pub model_map: HashMap<String, String>,
}

impl ProviderConfig {
    /// Load configuration from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            config.anthropic = Some(ProviderEndpoint {
                api_key: key,
                base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }

        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            config.openai = Some(ProviderEndpoint {
                api_key: key,
                base_url: std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .or_else(|| Some("https://api.openai.com".into())),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }

        if let Ok(key) = std::env::var("GROQ_API_KEY") {
            config.groq = Some(ProviderEndpoint {
                api_key: key,
                base_url: std::env::var("GROQ_BASE_URL")
                    .ok()
                    .or_else(|| Some("https://api.groq.com/openai/v1".into())),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }

        config.fallback_providers = parse_fallback_providers_env();
        config
    }

    /// Override the Anthropic provider API key (e.g. from CLI --api-key).
    ///
    /// If no Anthropic provider is configured yet, creates one with the given
    /// key and no base URL override.
    pub fn set_anthropic_api_key(&mut self, key: String) {
        if let Some(ref mut ep) = self.anthropic {
            ep.api_key = key;
        } else {
            self.anthropic = Some(ProviderEndpoint {
                api_key: key,
                base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }
    }

    /// Build a [`ModelRouter`] from this configuration.
    ///
    /// Providers are added in priority order: Anthropic > OpenAI > Groq > custom.
    #[must_use]
    pub fn build_router(&self) -> ModelRouter {
        let mut router =
            ModelRouter::new("default").with_fallback_providers(self.fallback_providers.clone());

        if let Some(ref ep) = self.anthropic {
            let mut config = ApiConfig::new(ep.api_key.clone());
            if let Some(ref url) = ep.base_url {
                config.base_url = url.clone();
            }
            if let Some(ref profile) = ep.provider_profile {
                config = config.with_provider_profile(profile.clone());
            }
            for (k, v) in &ep.custom_headers {
                config = config.with_custom_header(k.clone(), v.clone());
            }
            router.add_provider(Box::new(AnthropicProvider::new(config)));
        }

        if let Some(ref ep) = self.openai {
            let base_url = ep
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com".into());
            let mut provider =
                OpenAiCompatibleProvider::new("openai", base_url, ep.api_key.clone());
            provider = provider.with_model_map({
                let mut map = HashMap::new();
                map.insert("claude-sonnet-4-6".into(), "gpt-4o".into());
                map.insert("claude-opus-4-6".into(), "gpt-4o".into());
                map.insert("claude-haiku-4-5-20251001".into(), "gpt-4o-mini".into());
                map
            });
            if let Some(ref profile) = ep.provider_profile {
                provider = provider.with_provider_profile(profile.clone());
            }
            provider = provider.with_default_headers(ep.custom_headers.clone());
            router.add_provider(Box::new(provider));
        }

        if let Some(ref ep) = self.groq {
            let base_url = ep
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.groq.com/openai/v1".into());
            let mut provider = OpenAiCompatibleProvider::new("groq", base_url, ep.api_key.clone());
            provider = provider.with_model_map({
                let mut map = HashMap::new();
                map.insert("claude-sonnet-4-6".into(), "llama-3.3-70b-versatile".into());
                map.insert("claude-opus-4-6".into(), "llama-3.3-70b-versatile".into());
                map.insert(
                    "claude-haiku-4-5-20251001".into(),
                    "llama-3.1-8b-instant".into(),
                );
                map
            });
            if let Some(ref profile) = ep.provider_profile {
                provider = provider.with_provider_profile(profile.clone());
            }
            provider = provider.with_default_headers(ep.custom_headers.clone());
            router.add_provider(Box::new(provider));
        }

        for custom in &self.custom {
            let mut provider =
                OpenAiCompatibleProvider::new(&custom.name, &custom.base_url, &custom.api_key);
            provider = provider.with_model_map(custom.model_map.clone());
            provider = provider.with_default_headers(parse_custom_headers_env());
            router.add_provider(Box::new(provider));
        }

        router
    }
}

fn parse_custom_headers_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let raw = match std::env::var("CODEREX_CUSTOM_HEADERS") {
        Ok(v) => v,
        Err(_) => return out,
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim();
            let val = v.trim();
            if !key.is_empty() {
                out.insert(key.to_string(), val.to_string());
            }
        }
    }
    out
}

fn parse_fallback_providers_env() -> Vec<String> {
    std::env::var("CODEREX_FALLBACK_PROVIDERS")
        .ok()
        .map(|raw| {
            raw.split([',', '\n'])
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| item.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_builds_router() {
        // This test just verifies the API — it won't create providers
        // without real API keys in the environment.
        let config = ProviderConfig::from_env();
        let router = config.build_router();
        // Router exists even if empty
        let _ = router;
    }
}
