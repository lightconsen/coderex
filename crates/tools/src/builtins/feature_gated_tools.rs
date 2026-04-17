//! Feature-gated tool stubs — registered with `ToolTier::FeatureGated`.

use crate::context::{ToolError, ToolOutput};
use crate::registry::ToolRegistry;
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use std::sync::Arc;

macro_rules! feature_gated_tool {
    ($name:ident, $tool_name:expr) => {
        #[derive(Debug)]
        pub struct $name;

        #[async_trait]
        impl Tool for $name {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor {
                    name: $tool_name.into(),
                    tier: ToolTier::FeatureGated,
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                    output_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "error": { "type": "string" }
                        }
                    }),
                    requires_approval: false,
                    is_enabled: false,
                    is_read_only: true,
                    is_concurrency_safe: true,
                    interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
                }
            }

            async fn execute(&self, _input: ToolInput, _ctx: ToolContext<'_>) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    output: format!("tool '{}' is feature-gated and not enabled", $tool_name),
                    is_display: true,
                })
            }
        }
    };
}

feature_gated_tool!(WebBrowserTool, "WebBrowser");
feature_gated_tool!(TerminalCaptureTool, "TerminalCapture");
feature_gated_tool!(CtxInspectTool, "CtxInspect");
feature_gated_tool!(MonitorTool, "Monitor");
feature_gated_tool!(WorkflowToolTool, "WorkflowTool");
feature_gated_tool!(SnipToolTool, "SnipTool");
feature_gated_tool!(VerifyPlanExecutionTool, "VerifyPlanExecution");
feature_gated_tool!(SuggestBackgroundPRTool, "SuggestBackgroundPR");
feature_gated_tool!(PushNotificationTool, "PushNotification");
feature_gated_tool!(SendUserFileTool, "SendUserFile");
feature_gated_tool!(SubscribePRTool, "SubscribePR");
feature_gated_tool!(ListPeersTool, "ListPeers");
feature_gated_tool!(TungstenTool, "Tungsten");

/// Register all feature-gated tools into the registry.
pub fn register_feature_gated_tools(reg: &mut ToolRegistry) {
    reg.register(Arc::new(WebBrowserTool));
    reg.register(Arc::new(TerminalCaptureTool));
    reg.register(Arc::new(CtxInspectTool));
    reg.register(Arc::new(MonitorTool));
    reg.register(Arc::new(WorkflowToolTool));
    reg.register(Arc::new(SnipToolTool));
    reg.register(Arc::new(VerifyPlanExecutionTool));
    reg.register(Arc::new(SuggestBackgroundPRTool));
    reg.register(Arc::new(PushNotificationTool));
    reg.register(Arc::new(SendUserFileTool));
    reg.register(Arc::new(SubscribePRTool));
    reg.register(Arc::new(ListPeersTool));
    reg.register(Arc::new(TungstenTool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use crate::context::ExecutionContext;
    use std::collections::HashMap;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn feature_gated_tool_returns_disabled_message() {
        let output = WebBrowserTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "WebBrowser".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("feature-gated"));
    }

    #[test]
    fn feature_gated_tool_descriptor_is_gated() {
        let desc = WebBrowserTool.descriptor();
        assert!(matches!(desc.tier, ToolTier::FeatureGated));
        assert!(!desc.is_enabled);
    }

    #[test]
    fn all_feature_gated_tools_registered() {
        let mut reg = ToolRegistry::new();
        register_feature_gated_tools(&mut reg);
        let tools = reg.list_tools();
        assert_eq!(tools.len(), 13);
    }
}
