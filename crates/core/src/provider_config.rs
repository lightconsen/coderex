//! Provider configuration — build a [`ModelRouter`] from environment and files.
//!
//! Supports:
//! - `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL`
//! - `OPENAI_API_KEY` / `OPENAI_BASE_URL`
//! - `GROQ_API_KEY` / `GROQ_BASE_URL`
//! - Config file at `~/.coderex/providers.toml`

use crate::api_client::ApiConfig;
use crate::model_router::ModelRouter;
use crate::provider_router::ProviderRouter;
use crate::providers::cloud::{
    CloudEndpoint, ProviderKind, SelectionInputs, default_base_url, foundry_base_url, select,
    vertex_base_url,
};
use crate::providers::{AnthropicProvider, OpenAiCompatibleProvider};
use std::collections::{BTreeMap, HashMap};

/// Configuration for all providers.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// The cloud provider this deployment uses, if it is not Anthropic's own
    /// API. Selected from the environment; it takes precedence over the plain
    /// endpoint.
    pub cloud: Option<CloudEndpoint>,
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
    /// HTTP request timeout for the Anthropic-protocol transport, in seconds.
    /// `None` keeps the transport default (120 s).
    pub request_timeout_secs: Option<u64>,
    /// Whether Anthropic-protocol requests may carry prompt-cache breakpoints.
    ///
    /// `None` falls back to the `CODEREX_PROMPT_CACHING` environment override
    /// and then to the enabled default. The CLI sets this from the resolved
    /// `prompt_caching` setting.
    pub prompt_caching: Option<bool>,
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

/// Environment variables that supply the Anthropic-protocol API key, in
/// priority order. The CODEREX_ spellings win over the ANTHROPIC_ ones so a
/// session can override a globally exported Claude configuration.
const API_KEY_ENV: &[&str] = &[
    "CODEREX_API_KEY",
    "CODEREX_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];

/// Environment variables that supply the Anthropic-protocol base URL.
const BASE_URL_ENV: &[&str] = &["CODEREX_BASE_URL", "ANTHROPIC_BASE_URL"];

/// Environment variable that overrides the HTTP request timeout, in seconds.
const REQUEST_TIMEOUT_ENV: &str = "CODEREX_REQUEST_TIMEOUT_SECS";

/// The first variable in `names` that is set to a non-blank value.
fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

/// The request timeout from the environment, in seconds.
///
/// `CODEREX_REQUEST_TIMEOUT_SECS` is already in seconds; Claude's
/// `API_TIMEOUT_MS` is in milliseconds and is rounded up. The CodeRex variable
/// wins when both are set.
fn request_timeout_from_env() -> Option<u64> {
    if let Some(raw) = env_first(&[REQUEST_TIMEOUT_ENV]) {
        return match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Some(secs),
            _ => {
                tracing::warn!(
                    value = %raw,
                    "ignoring {REQUEST_TIMEOUT_ENV}: expected a positive integer"
                );
                None
            }
        };
    }

    let raw = env_first(&["API_TIMEOUT_MS"])?;
    match raw.trim().parse::<u64>() {
        Ok(ms) if ms > 0 => Some(ms.div_ceil(1000)),
        _ => {
            tracing::warn!(value = %raw, "ignoring API_TIMEOUT_MS: expected a positive integer");
            None
        }
    }
}

impl ProviderConfig {
    /// Load configuration from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        // A cloud provider is selected, not detected: the deployment says
        // which one it is, and that choice outranks the plain API key.
        let mut config = Self {
            cloud: cloud_endpoint_from_env(),
            ..Self::default()
        };

        if let Some(key) = env_first(API_KEY_ENV) {
            config.anthropic = Some(ProviderEndpoint {
                api_key: key,
                base_url: env_first(BASE_URL_ENV),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }

        config.request_timeout_secs = request_timeout_from_env();

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
                base_url: env_first(BASE_URL_ENV),
                provider_profile: std::env::var("CODEREX_PROVIDER_PROFILE").ok(),
                custom_headers: parse_custom_headers_env(),
            });
        }
    }

    /// Override the Anthropic provider base URL.
    ///
    /// Used by settings (`provider.base_url`) to select an Anthropic-compatible
    /// endpoint. Does nothing when no Anthropic endpoint is configured: without
    /// a key there is nothing to send to that URL anyway.
    pub fn set_anthropic_base_url(&mut self, base_url: String) {
        if let Some(ref mut endpoint) = self.anthropic {
            endpoint.base_url = Some(base_url);
        }
    }

    /// Build a [`ModelRouter`] from this configuration.
    ///
    /// Providers are added in priority order: Anthropic > OpenAI > Groq > custom.
    #[must_use]
    pub fn build_router(&self) -> ProviderRouter {
        let mut router =
            ModelRouter::new("default").with_fallback_providers(self.fallback_providers.clone());

        if let Some(ref endpoint) = self.cloud {
            // The cloud provider replaces the plain endpoint rather than
            // joining it: both answer the same API, and sending the work to
            // whichever is first would make the choice of provider a race.
            let key = self
                .anthropic
                .as_ref()
                .map(|ep| ep.api_key.clone())
                .or_else(|| env_first(API_KEY_ENV))
                .unwrap_or_default();

            let mut config = ApiConfig::new(key);
            config.base_url = cloud_base_url(endpoint);
            config.cloud = Some(endpoint.clone());
            config.prompt_caching = crate::api_client::prompt_caching_enabled(
                self.prompt_caching,
                std::env::var(crate::api_client::PROMPT_CACHING_ENV)
                    .ok()
                    .as_deref(),
            );
            if let Some(secs) = self.request_timeout_secs {
                config.request_timeout_secs = secs;
            }
            router.add_provider(Box::new(AnthropicProvider::new(config)));
        } else if let Some(ref ep) = self.anthropic {
            let mut config = ApiConfig::new(ep.api_key.clone());
            if let Some(ref url) = ep.base_url {
                config.base_url.clone_from(url);
            }
            config.prompt_caching = crate::api_client::prompt_caching_enabled(
                self.prompt_caching,
                std::env::var(crate::api_client::PROMPT_CACHING_ENV)
                    .ok()
                    .as_deref(),
            );
            if let Some(ref profile) = ep.provider_profile {
                config = config.with_provider_profile(profile.clone());
            }
            for (k, v) in &ep.custom_headers {
                config = config.with_custom_header(k.clone(), v.clone());
            }
            if let Some(secs) = self.request_timeout_secs {
                config.request_timeout_secs = secs;
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

        ProviderRouter::from_model_router(router)
    }
}

fn parse_custom_headers_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(raw) = std::env::var("CODEREX_CUSTOM_HEADERS") else {
        return out;
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
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// The cloud deployment the environment asks for, if any.
#[must_use]
pub fn cloud_endpoint_from_env() -> Option<CloudEndpoint> {
    let kind = select(&SelectionInputs::from_env());
    if kind == ProviderKind::FirstParty {
        return None;
    }

    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .or_else(|_| std::env::var("CLOUD_ML_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string());

    let endpoint = CloudEndpoint::new(kind, region);
    match kind {
        ProviderKind::Vertex => {
            let project = std::env::var("ANTHROPIC_VERTEX_PROJECT_ID")
                .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT"))
                .unwrap_or_default();
            Some(endpoint.with_project(project))
        }
        _ => Some(endpoint),
    }
}

/// The base URL a cloud deployment talks to.
#[must_use]
pub fn cloud_base_url(endpoint: &CloudEndpoint) -> String {
    match endpoint.kind {
        ProviderKind::Vertex => std::env::var("ANTHROPIC_VERTEX_BASE_URL")
            .unwrap_or_else(|_| vertex_base_url(&endpoint.region)),
        ProviderKind::Bedrock => std::env::var("ANTHROPIC_BEDROCK_BASE_URL")
            .unwrap_or_else(|_| default_base_url(endpoint.kind, &endpoint.region)),
        ProviderKind::Mantle => std::env::var("ANTHROPIC_BEDROCK_MANTLE_BASE_URL")
            .unwrap_or_else(|_| default_base_url(endpoint.kind, &endpoint.region)),
        ProviderKind::AnthropicAws => std::env::var("ANTHROPIC_AWS_BASE_URL")
            .unwrap_or_else(|_| default_base_url(endpoint.kind, &endpoint.region)),
        ProviderKind::Foundry => std::env::var("ANTHROPIC_FOUNDRY_BASE_URL").unwrap_or_else(|_| {
            foundry_base_url(&std::env::var("ANTHROPIC_FOUNDRY_RESOURCE").unwrap_or_default())
        }),
        ProviderKind::Gateway => std::env::var("CODEREX_GATEWAY_URL").unwrap_or_default(),
        ProviderKind::AnthropicGoogleCloud => std::env::var("ANTHROPIC_GOOGLE_CLOUD_BASE_URL")
            .unwrap_or_else(|_| default_base_url(endpoint.kind, &endpoint.region)),
        ProviderKind::FirstParty => default_base_url(endpoint.kind, &endpoint.region),
    }
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

    /// Settings (`provider.base_url`) select an Anthropic-compatible endpoint
    /// without an environment wrapper.
    #[test]
    fn a_settings_base_url_reaches_the_anthropic_endpoint() {
        let mut config = ProviderConfig::default();
        config.set_anthropic_api_key("sk-test".into());
        config.set_anthropic_base_url("https://api.deepseek.com/anthropic".into());

        let endpoint = config.anthropic.as_ref().unwrap();
        assert_eq!(endpoint.api_key, "sk-test");
        assert_eq!(
            endpoint.base_url.as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
    }

    #[test]
    fn a_base_url_without_a_key_is_ignored() {
        let mut config = ProviderConfig::default();
        config.set_anthropic_base_url("https://example.test".into());
        assert!(config.anthropic.is_none());
    }

    /// Environment tests mutate process state, so they take a lock and put
    /// every key back when they finish.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    #[allow(unsafe_code)] // Environment mutation is unsafe in edition 2024; ENV_LOCK guards it.
    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let previous = std::env::var(key).ok();
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, previous }
        }
    }

    #[allow(unsafe_code)] // Restoring the environment, under the same lock.
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn coderx_variables_win_over_anthropic_variables() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guards = [
            EnvGuard::set("CODEREX_API_KEY", Some("coderx-key")),
            EnvGuard::set("ANTHROPIC_API_KEY", Some("anthropic-key")),
            EnvGuard::set("CODEREX_AUTH_TOKEN", None),
            EnvGuard::set("ANTHROPIC_AUTH_TOKEN", None),
            EnvGuard::set("CODEREX_BASE_URL", Some("https://coderx.example")),
            EnvGuard::set("ANTHROPIC_BASE_URL", Some("https://anthropic.example")),
            EnvGuard::set("CODEREX_REQUEST_TIMEOUT_SECS", Some("3000")),
        ];

        let config = ProviderConfig::from_env();
        let endpoint = config.anthropic.as_ref().unwrap();
        assert_eq!(endpoint.api_key, "coderx-key");
        assert_eq!(endpoint.base_url.as_deref(), Some("https://coderx.example"));
        assert_eq!(config.request_timeout_secs, Some(3000));
    }

    /// Claude Code exports `ANTHROPIC_AUTH_TOKEN`; it is accepted as a key in
    /// its own right, and when only that variable is set there is no URL
    /// override to apply.
    #[test]
    fn anthropic_auth_token_is_accepted_as_a_key() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guards = [
            EnvGuard::set("CODEREX_API_KEY", None),
            EnvGuard::set("CODEREX_AUTH_TOKEN", None),
            EnvGuard::set("ANTHROPIC_API_KEY", None),
            EnvGuard::set("ANTHROPIC_AUTH_TOKEN", Some("auth-token")),
            EnvGuard::set("CODEREX_BASE_URL", None),
            EnvGuard::set("ANTHROPIC_BASE_URL", None),
        ];

        let config = ProviderConfig::from_env();
        let endpoint = config.anthropic.as_ref().unwrap();
        assert_eq!(endpoint.api_key, "auth-token");
        assert_eq!(endpoint.base_url, None);
    }

    /// Claude's timeout variable is in milliseconds and is accepted so a
    /// Claude `env` block can be copied over unchanged.
    #[test]
    fn claude_api_timeout_ms_is_converted_to_seconds() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guards = [
            EnvGuard::set("CODEREX_REQUEST_TIMEOUT_SECS", None),
            EnvGuard::set("API_TIMEOUT_MS", Some("3000000")),
        ];

        assert_eq!(ProviderConfig::from_env().request_timeout_secs, Some(3000));
    }
}
