//! Magic docs feature.
//!
//! Generates documentation for codebases, functions, or modules on demand
//! using a background AI agent with read-only tool access.

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_side_query};
use crate::query_profiles::ModelProfile;

/// Generates documentation for a specific file or code snippet.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `code_content` — The source code to document.
/// * `doc_format` — Documentation format (e.g., "markdown", "rustdoc", "jsdoc").
/// * `doc_level` — Detail level: "brief", "standard", or "comprehensive".
///
/// # Returns
/// Generated documentation text.
pub async fn generate_docs_for_code(
    api_client: &dyn ApiClient,
    code_content: &str,
    doc_format: &str,
    doc_level: &str,
) -> Result<String, OrchestratorError> {
    let system = "You are a technical documentation expert. Generate clear, accurate \
                  documentation for the provided code. Follow the specified format and \
                  detail level. Do not invent APIs or behaviors not present in the code.";
    let user = format!(
        "Generate documentation for the following code:\n\n\
         ```\n{code_content}\n```\n\n\
         Format: {doc_format}\n\
         Detail level: {doc_level}\n\n\
         Include: purpose, parameters, return value, and examples where applicable."
    );

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve(),
        1024,
    )
    .await?;

    Ok(result.content)
}

/// Generates an overview of a codebase structure.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `file_listing` — A listing of files and their purposes.
///
/// # Returns
/// A structured overview of the codebase.
pub async fn generate_codebase_overview(
    api_client: &dyn ApiClient,
    file_listing: &str,
) -> Result<String, OrchestratorError> {
    let system = "You are a software architect. Create a high-level overview of a \
                  codebase based on its file structure and descriptions. Identify \
                  the main components, their relationships, and the overall architecture.";
    let user = format!(
        "Here is the codebase structure:\n\n{file_listing}\n\n\
         Provide an overview of the architecture, main components, and how they interact."
    );

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve(),
        1024,
    )
    .await?;

    Ok(result.content)
}

/// Generates a README for a project or module.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `project_name` — Name of the project.
/// * `description` — Brief project description.
/// * `file_structure` — File tree or listing.
///
/// # Returns
/// A formatted README in markdown.
pub async fn generate_readme(
    api_client: &dyn ApiClient,
    project_name: &str,
    description: &str,
    file_structure: &str,
) -> Result<String, OrchestratorError> {
    let system = "You are a technical writer. Generate a professional README.md \
                  for a software project. Include: title, description, installation, \
                  usage, file structure, and contributing sections.";
    let user = format!(
        "Project: {project_name}\n\
         Description: {description}\n\n\
         File structure:\n{file_structure}\n\n\
         Generate a complete README in markdown."
    );

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve(),
        1024,
    )
    .await?;

    Ok(result.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::{ApiResponse, StreamEvent};
    use crate::types::{ModelId, UsageSummary};
    use std::pin::Pin;
    use futures::Stream;

    #[derive(Clone)]
    struct MockApi { response_content: String }

    impl MockApi {
        fn new(content: &str) -> Self { Self { response_content: content.into() } }
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                usage: UsageSummary {
                    input_tokens: 300,
                    output_tokens: 200,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn generate_docs_for_code_returns_content() {
        let api = MockApi::new("/// Adds two numbers.\nfn add(a: i32, b: i32) -> i32 { a + b }");
        let code = "fn add(a: i32, b: i32) -> i32 { a + b }";
        let docs = generate_docs_for_code(&api, code, "rustdoc", "brief").await;
        assert!(docs.is_ok());
    }

    #[tokio::test]
    async fn generate_codebase_overview_returns_content() {
        let api = MockApi::new("This is a Rust project with an orchestrator pattern...");
        let listing = "src/\n  main.rs\n  engine.rs\n  api_client.rs";
        let overview = generate_codebase_overview(&api, listing).await;
        assert!(overview.is_ok());
    }

    #[tokio::test]
    async fn generate_readme_returns_content() {
        let api = MockApi::new("# coderex\n\nA Rust code agent.");
        let readme = generate_readme(&api, "coderex", "A Rust code agent", "src/\n  main.rs").await;
        assert!(readme.is_ok());
    }
}
