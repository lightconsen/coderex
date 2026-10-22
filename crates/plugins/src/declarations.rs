//! TypeScript declaration generation for the host's own plugin surface.
//!
//! A plugin manifest may declare `types` — `.d.ts` files it ships. Those are
//! the plugin's own declarations. This module produces the *other* half: the
//! declarations CodeRex itself exports so a plugin author has typed access to
//! the builtin tools, the engine facts a hook receives, and the configured MCP
//! servers.
//!
//! Everything here is pure: a JSON Schema in, a TypeScript declaration out.
//! The generators do not read the filesystem, execute a TypeScript compiler,
//! or type-check what they emit. [`PluginManager::export_plugin_types`]
//! (crate::plugins) is the IO half; it writes these strings beside each
//! plugin's declared files.
//!
//! [`PluginManager::export_plugin_types`]: crate::plugins::PluginManager::export_plugin_types

use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// One builtin tool's name and the schemas its declaration is generated from.
///
/// `input_schema` is always required — a tool without one is advertised as
/// accepting `unknown`. `output_schema` is optional because many tools leave
/// the result shape untyped; a missing or empty schema becomes `unknown`
/// rather than a made-up object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTypeSchema {
    /// The tool name, used verbatim as the interface key (quoted when it is
    /// not a TypeScript identifier).
    pub name: String,
    /// The JSON Schema for the tool's arguments.
    pub input_schema: Value,
    /// The JSON Schema for the tool's result, when the tool declares one.
    pub output_schema: Option<Value>,
}

/// Render the builtin-tool declaration from input schemas alone.
///
/// The result interface is emitted too, with every tool mapped to `unknown`.
/// Use [`builtin_tools_declaration_with_results`] when output schemas are
/// available.
///
/// Tool names are sorted, so the output is stable regardless of registration
/// order.
#[must_use]
pub fn builtin_tools_declaration(tools: &[(String, Value)]) -> String {
    builtin_tools_declaration_with_results(tools, &[])
}

/// Render the builtin-tool declaration with both input and result shapes.
///
/// `results` is looked up by tool name. A tool with an input schema but no
/// entry here is declared as returning `unknown`; a tool that appears only in
/// `results` is still listed. Tool names are sorted for stable output.
#[must_use]
pub fn builtin_tools_declaration_with_results(
    tools: &[(String, Value)],
    results: &[(String, Value)],
) -> String {
    let inputs: std::collections::BTreeMap<&str, &Value> = tools
        .iter()
        .map(|(name, schema)| (name.as_str(), schema))
        .collect();
    let outputs: std::collections::BTreeMap<&str, &Value> = results
        .iter()
        .map(|(name, schema)| (name.as_str(), schema))
        .collect();
    let names: BTreeSet<&str> = inputs.keys().chain(outputs.keys()).copied().collect();

    let mut declaration = String::from("declare module 'coderex-tools' {\n");
    declaration.push_str("  interface BuiltinToolInputs {\n");
    for name in &names {
        let rendered = inputs
            .get(name)
            .map_or_else(|| "unknown".to_string(), |schema| schema_to_ts(schema, 4));
        let _ = writeln!(declaration, "    {}: {rendered};", property_name(name));
    }
    declaration.push_str("  }\n\n");

    declaration.push_str("  interface BuiltinToolResults {\n");
    for name in &names {
        let rendered = outputs
            .get(name)
            .map_or_else(|| "unknown".to_string(), |schema| schema_to_ts(schema, 4));
        let _ = writeln!(declaration, "    {}: {rendered};", property_name(name));
    }
    declaration.push_str("  }\n}\n");
    declaration
}

/// Render the engine declaration: the session facts a hook receives and the
/// hook signature shape.
#[must_use]
pub fn engine_declaration() -> String {
    let mut declaration = String::from("declare module 'coderex' {\n");
    declaration.push_str("  /** How tool approvals are resolved for the session. */\n");
    declaration.push_str("  type PermissionMode =\n");
    declaration.push_str("    | 'default'\n");
    declaration.push_str("    | 'accept-edits'\n");
    declaration.push_str("    | 'bypass-permissions'\n");
    declaration.push_str("    | 'dont-ask'\n");
    declaration.push_str("    | 'plan'\n");
    declaration.push_str("    | 'auto';\n\n");

    declaration.push_str("  /** The hook events CodeRex knows the name of. */\n");
    declaration.push_str("  type HookEvent =\n");
    for event in HOOK_EVENTS {
        let _ = writeln!(declaration, "    | '{event}'");
    }
    declaration.push_str("  ;\n\n");

    declaration.push_str("  /** Session facts passed to every hook invocation. */\n");
    declaration.push_str("  interface HookContext {\n");
    declaration.push_str("    /** Stable identifier of the running session. */\n");
    declaration.push_str("    sessionId: string;\n");
    declaration.push_str("    /** The session's working directory. */\n");
    declaration.push_str("    cwd: string;\n");
    declaration.push_str("    /** The model id the session is running. */\n");
    declaration.push_str("    model: string;\n");
    declaration.push_str("    /** The permission mode the session is running under. */\n");
    declaration.push_str("    permissionMode: PermissionMode;\n");
    declaration.push_str("    /** The one-based turn number within the session. */\n");
    declaration.push_str("    turnNumber: number;\n");
    declaration.push_str("  }\n\n");

    declaration
        .push_str("  /** What a hook may return. An empty result lets the action proceed. */\n");
    declaration.push_str("  interface HookResult {\n");
    declaration.push_str("    /** Stop the action the hook gates. */\n");
    declaration.push_str("    stop?: boolean;\n");
    declaration.push_str("    /** Why the action was stopped, when `stop` is set. */\n");
    declaration.push_str("    reason?: string;\n");
    declaration.push_str("    /** Tool names to permit for this call. */\n");
    declaration.push_str("    allow?: string[];\n");
    declaration.push_str("    /** Tool names to refuse for this call. */\n");
    declaration.push_str("    deny?: string[];\n");
    declaration.push_str("  }\n\n");

    declaration.push_str(
        "  /** A hook handler: receives the session context, synchronously or as a promise. */\n",
    );
    declaration.push_str(
        "  type Hook = (context: HookContext) => HookResult | void | Promise<HookResult | void>;\n\n",
    );

    declaration.push_str("  /** The engine surface exposed to a plugin. */\n");
    declaration.push_str("  interface EngineInterface {\n");
    declaration.push_str("    /** Stable identifier of the running session. */\n");
    declaration.push_str("    readonly sessionId: string;\n");
    declaration.push_str("    /** The session's working directory. */\n");
    declaration.push_str("    readonly cwd: string;\n");
    declaration.push_str("    /** The model id the session is running. */\n");
    declaration.push_str("    readonly model: string;\n");
    declaration.push_str("    /** The permission mode the session is running under. */\n");
    declaration.push_str("    readonly permissionMode: PermissionMode;\n");
    declaration.push_str("    /** The one-based turn number within the session. */\n");
    declaration.push_str("    readonly turnNumber: number;\n");
    declaration.push_str("    /** Register a handler for one hook event. */\n");
    declaration.push_str("    registerHook(event: HookEvent, handler: Hook): void;\n");
    declaration.push_str("  }\n}\n");
    declaration
}

/// Render the MCP declaration: a configured server, its transport, and its
/// connection state.
#[must_use]
pub fn mcp_declaration() -> String {
    let mut declaration = String::from("declare module 'coderex-mcp' {\n");
    declaration.push_str("  /** Transport used to reach an MCP server. */\n");
    declaration.push_str("  type McpTransport = 'stdio' | 'sse' | 'http';\n\n");

    declaration.push_str("  /** The connection state of an MCP server. */\n");
    declaration.push_str(
        "  type McpServerStatus = 'stopped' | 'starting' | 'running' | 'unhealthy' | 'errored';\n\n",
    );

    declaration.push_str("  /** A configured MCP server. */\n");
    declaration.push_str("  interface McpServer {\n");
    declaration.push_str("    /** The name the server is configured under. */\n");
    declaration.push_str("    name: string;\n");
    declaration.push_str("    /** How the server is reached. */\n");
    declaration.push_str("    transport: McpTransport;\n");
    declaration.push_str("    /** The command run for a stdio server. */\n");
    declaration.push_str("    command?: string;\n");
    declaration.push_str("    /** Arguments passed to a stdio server's command. */\n");
    declaration.push_str("    args?: string[];\n");
    declaration.push_str("    /** Extra environment for a stdio server. */\n");
    declaration.push_str("    env?: Record<string, string>;\n");
    declaration.push_str("    /** The endpoint of an sse or http server. */\n");
    declaration.push_str("    url?: string;\n");
    declaration.push_str("    /** Extra HTTP headers sent to a remote server. */\n");
    declaration.push_str("    headers?: Record<string, string>;\n");
    declaration.push_str("    /** The server's current connection state. */\n");
    declaration.push_str("    status: McpServerStatus;\n");
    declaration.push_str("  }\n\n");

    declaration.push_str("  /** The MCP servers configured for the session. */\n");
    declaration.push_str("  interface McpInterface {\n");
    declaration.push_str("    /** Every configured server, in configuration order. */\n");
    declaration.push_str("    readonly servers: McpServer[];\n");
    declaration.push_str("    /** The server configured under `name`, when there is one. */\n");
    declaration.push_str("    get(name: string): McpServer | undefined;\n");
    declaration.push_str("  }\n}\n");
    declaration
}

/// Render the `tsconfig.json` a plugin extends.
///
/// `declaration_dirs` are the directories emitted under the export root. They
/// are sorted and deduplicated so the file is stable. A plugin that extends
/// this config gets strict checking over every emitted declaration without
/// emitting JavaScript itself.
#[must_use]
pub fn tsconfig_declaration(declaration_dirs: &[String]) -> String {
    let dirs: BTreeSet<&str> = declaration_dirs.iter().map(String::as_str).collect();
    let value = serde_json::json!({
        "compilerOptions": {
            "strict": true,
            "noEmit": true,
            "skipLibCheck": true,
            "module": "esnext",
            "moduleResolution": "node",
            "target": "es2022",
        },
        "include": dirs.into_iter().collect::<Vec<_>>(),
    });
    let mut rendered = serde_json::to_string_pretty(&value)
        .unwrap_or_else(|_| "{\n  \"include\": []\n}".to_string());
    rendered.push('\n');
    rendered
}

/// The canonical spellings of the hook events CodeRex knows.
const HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "UserPromptSubmit",
    "SessionStart",
    "Setup",
    "SubagentStart",
    "Notification",
    "PermissionRequest",
    "PermissionDenied",
    "CwdChanged",
    "FileChanged",
    "WorktreeCreate",
    "PreCompact",
    "PostCompact",
    "Stop",
    "SubagentStop",
    "TaskCompleted",
    "TeammateIdle",
    "Elicitation",
    "ElicitationResult",
    "TurnEnd",
];

/// Convert one JSON Schema value to a TypeScript type.
///
/// `indent` is the column of the construct that encloses the rendered type,
/// used to lay out object members. The conversion is deliberately partial:
///
/// - `enum` becomes a literal union and `const` a literal.
/// - `anyOf` and `oneOf` become unions; duplicates are dropped.
/// - `type: object` renders its `properties`, honouring `required`, and an
///   `additionalProperties` schema becomes an index signature.
/// - `type: array` renders `Array<items>` (`unknown` when `items` is absent).
/// - `string`, `number`, `integer`, `boolean` and `null` map directly.
/// - Anything else, including `$ref` and an absent `type`, is `unknown`.
///
/// The function never fails: an unrecognised schema is `unknown`, not an
/// error, because a declaration that omits a shape is still usable while a
/// generator that refuses to run is not.
#[must_use]
pub fn schema_to_ts(schema: &Value, indent: usize) -> String {
    let Some(object) = schema.as_object() else {
        return "unknown".to_string();
    };

    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return "never".to_string();
        }
        return union(values.iter().map(ts_literal));
    }

    if let Some(value) = object.get("const") {
        return ts_literal(value);
    }

    for key in ["oneOf", "anyOf"] {
        if let Some(values) = object.get(key).and_then(Value::as_array) {
            if values.is_empty() {
                return "never".to_string();
            }
            return union(values.iter().map(|value| schema_to_ts(value, indent)));
        }
    }

    match object.get("type") {
        Some(Value::String(kind)) => match kind.as_str() {
            "object" => object_to_ts(object, indent),
            "array" => object.get("items").map_or_else(
                || "Array<unknown>".to_string(),
                |items| format!("Array<{}>", schema_to_ts(items, indent)),
            ),
            "string" => "string".to_string(),
            "number" | "integer" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            _ => "unknown".to_string(),
        },
        Some(Value::Array(kinds)) => {
            let variants: Vec<String> = kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|kind| match kind {
                    "object" => object_to_ts(object, indent),
                    "array" => "Array<unknown>".to_string(),
                    "string" => "string".to_string(),
                    "number" | "integer" => "number".to_string(),
                    "boolean" => "boolean".to_string(),
                    "null" => "null".to_string(),
                    _ => "unknown".to_string(),
                })
                .collect();
            union(variants)
        }
        _ if object.contains_key("properties") || object.contains_key("additionalProperties") => {
            object_to_ts(object, indent)
        }
        _ => "unknown".to_string(),
    }
}

/// Render a JSON object schema as a TypeScript object type.
fn object_to_ts(object: &serde_json::Map<String, Value>, indent: usize) -> String {
    let properties = object.get("properties").and_then(Value::as_object);
    let required: BTreeSet<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();

    let mut members: Vec<String> = Vec::new();
    let mut property_types: Vec<String> = Vec::new();
    if let Some(properties) = properties {
        let mut names: Vec<&String> = properties.keys().collect();
        names.sort();
        for name in names {
            let Some(schema) = properties.get(name) else {
                continue;
            };
            let rendered = schema_to_ts(schema, indent + 2);
            property_types.push(rendered.clone());
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            members.push(format!(
                "{}{}{}: {rendered};",
                " ".repeat(indent + 2),
                property_name(name),
                optional
            ));
        }
    }

    // An `additionalProperties` schema is an index signature. When explicit
    // properties exist, their types are folded into the value union so the
    // emitted signature is valid TypeScript (`[key: string]: T` must accept
    // every declared property).
    match object.get("additionalProperties") {
        Some(Value::Bool(true)) => {
            property_types.push("unknown".to_string());
            members.push(format!("{}[key: string]: unknown;", " ".repeat(indent + 2)));
        }
        Some(Value::Object(_)) => {
            if let Some(extra) = object.get("additionalProperties") {
                let rendered = schema_to_ts(extra, indent + 2);
                property_types.push(rendered);
                let index_type = union(property_types.iter().cloned());
                members.push(format!(
                    "{}[key: string]: {index_type};",
                    " ".repeat(indent + 2)
                ));
            }
        }
        _ => {}
    }

    if members.is_empty() {
        return "Record<string, unknown>".to_string();
    }

    format!("{{\n{}\n{}}}", members.join("\n"), " ".repeat(indent))
}

/// Join type variants into a union, dropping duplicates while keeping the
/// first occurrence.
fn union<I>(variants: I) -> String
where
    I: IntoIterator<Item = String>,
{
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for variant in variants {
        if seen.insert(variant.clone()) {
            unique.push(variant);
        }
    }
    if unique.is_empty() {
        "never".to_string()
    } else {
        unique.join(" | ")
    }
}

/// Render a JSON scalar as a TypeScript literal.
fn ts_literal(value: &Value) -> String {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
            serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_string())
        }
        _ => "unknown".to_string(),
    }
}

/// Quote a property or interface key unless it is a plain TypeScript
/// identifier.
fn property_name(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
    }
}

/// Whether `name` can appear unquoted as a TypeScript property name.
fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '$')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(schema: Value) -> String {
        schema_to_ts(&schema, 4)
    }

    #[test]
    fn object_schema_honours_properties_and_required() {
        let rendered = render(json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout": {"type": "integer"},
                "description": {"type": "string"},
            },
            "required": ["command"],
        }));
        assert_eq!(
            rendered,
            "{\n      command: string;\n      description?: string;\n      timeout?: number;\n    }"
        );
    }

    #[test]
    fn enum_becomes_a_literal_union() {
        assert_eq!(
            render(json!({"type": "string", "enum": ["low", "high"]})),
            "\"low\" | \"high\""
        );
        assert_eq!(render(json!({"enum": [1, true, null]})), "1 | true | null");
    }

    #[test]
    fn array_renders_its_item_type() {
        assert_eq!(
            render(json!({"type": "array", "items": {"type": "string"}})),
            "Array<string>"
        );
        assert_eq!(render(json!({"type": "array"})), "Array<unknown>");
    }

    #[test]
    fn any_of_and_one_of_become_unions() {
        assert_eq!(
            render(json!({"anyOf": [{"type": "string"}, {"type": "number"}]})),
            "string | number"
        );
        assert_eq!(
            render(json!({"oneOf": [{"type": "boolean"}, {"type": "boolean"}]})),
            "boolean"
        );
    }

    #[test]
    fn unknown_shapes_fall_back_to_unknown() {
        assert_eq!(render(json!({})), "unknown");
        assert_eq!(render(json!({"$ref": "#/definitions/x"})), "unknown");
        assert_eq!(render(json!("not a schema")), "unknown");
        assert_eq!(render(json!({"type": "quantum"})), "unknown");
    }

    #[test]
    fn non_identifier_property_names_are_quoted() {
        let rendered = render(json!({
            "type": "object",
            "properties": {"run-in-background": {"type": "boolean"}},
        }));
        assert!(
            rendered.contains("\"run-in-background\"?: boolean;"),
            "{rendered}"
        );
    }

    #[test]
    fn additional_properties_render_an_index_signature() {
        let rendered = render(json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "additionalProperties": {"type": "number"},
        }));
        assert!(rendered.contains("name?: string;"), "{rendered}");
        assert!(
            rendered.contains("[key: string]: string | number;"),
            "{rendered}"
        );
    }

    #[test]
    fn builtin_tools_are_sorted_and_include_both_interfaces() {
        let declaration = builtin_tools_declaration_with_results(
            &[
                (
                    "Write".to_string(),
                    json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                ),
                (
                    "Bash".to_string(),
                    json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
                ),
            ],
            &[(
                "Bash".to_string(),
                json!({"type": "object", "properties": {"stdout": {"type": "string"}}, "required": ["stdout"]}),
            )],
        );

        assert!(declaration.starts_with("declare module 'coderex-tools' {"));
        assert!(declaration.contains("interface BuiltinToolInputs {"));
        assert!(declaration.contains("interface BuiltinToolResults {"));
        let bash = declaration.find("Bash:").unwrap();
        let write = declaration.find("Write:").unwrap();
        assert!(bash < write, "tool names must be sorted: {declaration}");
        assert!(declaration.contains("command: string;"), "{declaration}");
        assert!(declaration.contains("stdout: string;"), "{declaration}");
        assert!(
            declaration.matches("Bash: unknown;").count() == 0,
            "Bash has both schemas: {declaration}"
        );
    }

    #[test]
    fn input_only_tools_result_in_unknown() {
        let declaration =
            builtin_tools_declaration(&[("Read".to_string(), json!({"type": "object"}))]);
        assert!(
            declaration.contains("Read: Record<string, unknown>;"),
            "{declaration}"
        );
        assert!(declaration.contains("Read: unknown;"), "{declaration}");
    }

    #[test]
    fn engine_declaration_describes_the_hook_context() {
        let declaration = engine_declaration();
        assert!(declaration.starts_with("declare module 'coderex' {"));
        assert!(declaration.contains("interface HookContext {"));
        assert!(declaration.contains("sessionId: string;"));
        assert!(declaration.contains("cwd: string;"));
        assert!(declaration.contains("model: string;"));
        assert!(declaration.contains("permissionMode: PermissionMode;"));
        assert!(declaration.contains("turnNumber: number;"));
        assert!(declaration.contains("type Hook ="));
        assert!(declaration.contains("interface EngineInterface {"));
    }

    #[test]
    fn mcp_declaration_describes_a_configured_server() {
        let declaration = mcp_declaration();
        assert!(declaration.starts_with("declare module 'coderex-mcp' {"));
        assert!(declaration.contains("interface McpServer {"));
        assert!(declaration.contains("name: string;"));
        assert!(declaration.contains("command?: string;"));
        assert!(declaration.contains("url?: string;"));
        assert!(declaration.contains("status: McpServerStatus;"));
    }

    #[test]
    fn tsconfig_lists_sorted_unique_directories() {
        let declaration = tsconfig_declaration(&[
            "coderex".to_string(),
            "acme".to_string(),
            "coderex".to_string(),
            "coderex-tools".to_string(),
        ]);
        let parsed: Value = serde_json::from_str(&declaration).unwrap();
        assert_eq!(
            parsed["include"],
            json!(["acme", "coderex", "coderex-tools"])
        );
        assert_eq!(parsed["compilerOptions"]["strict"], json!(true));
        assert_eq!(parsed["compilerOptions"]["noEmit"], json!(true));
    }
}
