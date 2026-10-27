//! StructuredOutput tool — validate JSON output against a JSON schema.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Validate JSON against a schema and return the validation result.
#[derive(Debug)]
pub struct StructuredOutputTool;

#[async_trait]
impl Tool for StructuredOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "StructuredOutput".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["json_schema", "output"],
                "properties": {
                    "json_schema": { "type": "object", "description": "JSON Schema to validate against" },
                    "output": { "type": "string", "description": "JSON string to validate" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "valid": { "type": "boolean" },
                    "error": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        _ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let schema = input
            .arguments
            .get("json_schema")
            .ok_or_else(|| ToolError::InvalidInput {
                message: "missing required field: json_schema".into(),
            })?;
        let output_str = extract_string(&input.arguments, "output")?;

        let value: serde_json::Value =
            serde_json::from_str(&output_str).map_err(|e| ToolError::InvalidInput {
                message: format!("output is not valid JSON: {e}"),
            })?;

        let result = jsonschema_validation(schema, &value);

        let out = if result.is_valid {
            serde_json::json!({ "valid": true }).to_string()
        } else {
            serde_json::json!({ "valid": false, "error": result.error }).to_string()
        };

        Ok(ToolOutput {
            output: out,
            is_display: true,
            images: Vec::new(),
        })
    }
}

struct ValidationResult {
    is_valid: bool,
    error: String,
}

/// Basic JSON schema validation (supports type, required, properties, items, enum).
fn jsonschema_validation(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> ValidationResult {
    validate_node(schema, value, "$")
}

fn validate_node(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> ValidationResult {
    let Some(obj) = schema.as_object() else {
        return ValidationResult {
            is_valid: true,
            error: String::new(),
        };
    };

    // Type check
    if let Some(type_val) = obj.get("type") {
        if let Some(type_str) = type_val.as_str() {
            let matches = match type_str {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.is_i64() || value.is_u64(),
                "boolean" => value.is_boolean(),
                "array" => value.is_array(),
                "object" => value.is_object(),
                "null" => value.is_null(),
                _ => true,
            };
            if !matches {
                return ValidationResult {
                    is_valid: false,
                    error: format!("{path}: expected type '{type_str}'"),
                };
            }
        }
    }

    // Enum check
    if let Some(enum_val) = obj.get("enum") {
        if let Some(arr) = enum_val.as_array() {
            if !arr.contains(value) {
                return ValidationResult {
                    is_valid: false,
                    error: format!("{path}: value not in enum"),
                };
            }
        }
    }

    // Object properties
    if value.is_object() {
        // Required
        if let Some(req) = obj.get("required") {
            if let Some(arr) = req.as_array() {
                for r in arr {
                    if let Some(key) = r.as_str() {
                        if value.get(key).is_none() {
                            return ValidationResult {
                                is_valid: false,
                                error: format!("{path}: missing required field '{key}'"),
                            };
                        }
                    }
                }
            }
        }

        // Properties
        if let Some(props) = obj.get("properties") {
            if let Some(prop_obj) = props.as_object() {
                if let Some(val_obj) = value.as_object() {
                    for (key, prop_schema) in prop_obj {
                        if let Some(val) = val_obj.get(key) {
                            let child = validate_node(prop_schema, val, &format!("{path}.{key}"));
                            if !child.is_valid {
                                return child;
                            }
                        }
                    }
                }
            }
        }
    }

    // Array items
    if value.is_array() {
        if let Some(items) = obj.get("items") {
            if let Some(arr) = value.as_array() {
                for (i, item) in arr.iter().enumerate() {
                    let child = validate_node(items, item, &format!("{path}[{i}]"));
                    if !child.is_valid {
                        return child;
                    }
                }
            }
        }
    }

    ValidationResult {
        is_valid: true,
        error: String::new(),
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn structured_output_valid() {
        let output = StructuredOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "StructuredOutput".into(),
                    arguments: serde_json::json!({
                        "json_schema": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } },
                        "output": "{\"name\": \"test\"}"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"valid\":true"));
    }

    #[tokio::test]
    async fn structured_output_invalid_missing_required() {
        let output = StructuredOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "StructuredOutput".into(),
                    arguments: serde_json::json!({
                        "json_schema": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string" } } },
                        "output": "{}"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"valid\":false"));
    }

    #[tokio::test]
    async fn structured_output_invalid_type_mismatch() {
        let output = StructuredOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "StructuredOutput".into(),
                    arguments: serde_json::json!({
                        "json_schema": { "type": "object", "properties": { "count": { "type": "number" } } },
                        "output": "{\"count\": \"not a number\"}"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("\"valid\":false"));
    }
}
