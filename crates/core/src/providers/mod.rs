//! Concrete provider implementations.

pub mod anthropic;
pub mod cloud;
pub mod openai_compatible;

pub use anthropic::AnthropicProvider;
pub use cloud::{
    AuthStyle, AwsCredentials, ProviderKind, RequestShape, SelectionInputs, default_base_url,
    foundry_base_url, select, sign_v4, signing_service, vertex_base_url,
};
pub use openai_compatible::OpenAiCompatibleProvider;
