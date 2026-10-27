//! NotebookEdit tool — edit cells in a Jupyter notebook.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Seek, SeekFrom, Write as _};

/// Edit a cell in a Jupyter notebook (.ipynb file).
#[derive(Debug)]
pub struct NotebookEditTool;

#[derive(Debug, Serialize, Deserialize)]
struct Notebook {
    cells: Vec<NotebookCell>,
    metadata: serde_json::Value,
    nbformat: i64,
    nbformat_minor: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct NotebookCell {
    cell_type: String,
    source: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outputs: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution_count: Option<serde_json::Value>,
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "NotebookEdit".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["notebook_path", "new_source"],
                "properties": {
                    "notebook_path": { "type": "string" },
                    "cell_id": { "type": "string" },
                    "new_source": { "type": "string" },
                    "cell_type": { "type": "string", "enum": ["code", "markdown"] },
                    "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"] }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "edited": { "type": "boolean" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let notebook_path = extract_string(&input.arguments, "notebook_path")?;
        let new_source = extract_string(&input.arguments, "new_source")?;
        let cell_id = extract_string_opt(&input.arguments, "cell_id");
        let cell_type =
            extract_string_opt(&input.arguments, "cell_type").unwrap_or_else(|| "code".into());
        let edit_mode =
            extract_string_opt(&input.arguments, "edit_mode").unwrap_or_else(|| "replace".into());

        let mut verified =
            crate::verified_fs::open_read_write(std::path::Path::new(&notebook_path), &ctx)?;
        let actual = verified.path.clone();
        let mut content = String::new();
        verified
            .file
            .read_to_string(&mut content)
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to read notebook: {e}"),
            })?;
        verified
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to seek notebook: {e}"),
            })?;

        let mut notebook: Notebook =
            serde_json::from_str(&content).map_err(|e| ToolError::InvalidInput {
                message: format!("invalid notebook format: {e}"),
            })?;

        match edit_mode.as_str() {
            "replace" => {
                let Some(cid) = cell_id else {
                    return Err(ToolError::InvalidInput {
                        message: "cell_id is required for replace mode".into(),
                    });
                };
                let Some(cell) = notebook
                    .cells
                    .iter_mut()
                    .find(|c| c.id.as_deref() == Some(&cid))
                else {
                    return Err(ToolError::InvalidInput {
                        message: format!("cell '{cid}' not found"),
                    });
                };
                cell.cell_type = cell_type;
                cell.source = split_lines(&new_source);
            }
            "insert" => {
                let new_cell = NotebookCell {
                    cell_type,
                    source: split_lines(&new_source),
                    id: cell_id.clone().or_else(|| Some(generate_cell_id())),
                    metadata: None,
                    outputs: None,
                    execution_count: None,
                };
                let idx = cell_id
                    .as_ref()
                    .and_then(|cid| {
                        notebook
                            .cells
                            .iter()
                            .position(|c| c.id.as_deref() == Some(cid))
                    })
                    .map_or(notebook.cells.len(), |i| i + 1);
                notebook.cells.insert(idx, new_cell);
            }
            "delete" => {
                let Some(cid) = cell_id else {
                    return Err(ToolError::InvalidInput {
                        message: "cell_id is required for delete mode".into(),
                    });
                };
                let len_before = notebook.cells.len();
                notebook.cells.retain(|c| c.id.as_deref() != Some(&cid));
                if notebook.cells.len() == len_before {
                    return Err(ToolError::InvalidInput {
                        message: format!("cell '{cid}' not found"),
                    });
                }
            }
            _ => {
                return Err(ToolError::InvalidInput {
                    message: format!("unknown edit_mode: {edit_mode}"),
                });
            }
        }

        let new_content =
            serde_json::to_string_pretty(&notebook).map_err(|e| ToolError::InvalidInput {
                message: format!("failed to serialize notebook: {e}"),
            })?;

        // Snapshot the current notebook so `/rewind` can undo this edit.
        if let Some(store) = crate::tool::checkpoint_store_from(&ctx) {
            store.snapshot(&input.call_id, &actual);
        }

        verified
            .file
            .set_len(0)
            .and_then(|()| verified.file.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| verified.file.write_all(new_content.as_bytes()))
            .map_err(|e| ToolError::InvalidInput {
                message: format!("failed to write notebook: {e}"),
            })?;

        Ok(ToolOutput {
            output: "true".to_string(),
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn split_lines(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.lines().map(|line| format!("{line}\n")).collect()
}

fn generate_cell_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("cell-{}", COUNTER.fetch_add(1, Ordering::SeqCst))
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
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

    fn create_test_notebook(path: &str) {
        let nb = Notebook {
            cells: vec![NotebookCell {
                cell_type: "code".into(),
                source: vec!["print('hello')\n".into()],
                id: Some("cell-1".into()),
                metadata: None,
                outputs: None,
                execution_count: None,
            }],
            metadata: serde_json::json!({}),
            nbformat: 4,
            nbformat_minor: 0,
        };
        std::fs::write(path, serde_json::to_string_pretty(&nb).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn notebook_edit_replace() {
        let path = "/tmp/test_notebook_replace.ipynb";
        create_test_notebook(path);

        let output = NotebookEditTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "NotebookEdit".into(),
                    arguments: serde_json::json!({
                        "notebook_path": path,
                        "cell_id": "cell-1",
                        "new_source": "print('world')",
                        "edit_mode": "replace"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("print('world')"));
    }

    #[tokio::test]
    async fn notebook_edit_insert() {
        let path = "/tmp/test_notebook_insert.ipynb";
        create_test_notebook(path);

        let output = NotebookEditTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "NotebookEdit".into(),
                    arguments: serde_json::json!({
                        "notebook_path": path,
                        "new_source": "# new cell",
                        "cell_type": "markdown",
                        "edit_mode": "insert"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");

        let content = std::fs::read_to_string(path).unwrap();
        let nb: Notebook = serde_json::from_str(&content).unwrap();
        assert_eq!(nb.cells.len(), 2);
    }

    #[tokio::test]
    async fn notebook_edit_delete() {
        let path = "/tmp/test_notebook_delete.ipynb";
        create_test_notebook(path);

        let output = NotebookEditTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "NotebookEdit".into(),
                    arguments: serde_json::json!({
                        "notebook_path": path,
                        "cell_id": "cell-1",
                        "new_source": "",
                        "edit_mode": "delete"
                    }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert_eq!(output.output, "true");

        let content = std::fs::read_to_string(path).unwrap();
        let nb: Notebook = serde_json::from_str(&content).unwrap();
        assert_eq!(nb.cells.len(), 0);
    }

    #[tokio::test]
    async fn notebook_edit_replace_missing_cell_id() {
        let path = "/tmp/test_notebook_err.ipynb";
        create_test_notebook(path);

        let result = NotebookEditTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "NotebookEdit".into(),
                    arguments: serde_json::json!({
                        "notebook_path": path,
                        "new_source": "x",
                        "edit_mode": "replace"
                    }),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }
}
