//! Reading `meta` out of a workflow script, without running it.
//!
//! A script starts with `export const meta = { … }` written as a *literal*:
//! no variables, no calls, no interpolation. That restriction is what lets the
//! name and description be known — and shown to the person who has to approve
//! the run — before any of it executes, which is the point.
//!
//! So this is a small literal reader rather than an evaluator: it walks the
//! object and accepts only what a literal can contain. Anything clever is
//! refused with a reason, because a `meta` that needs evaluating is a `meta`
//! that cannot be shown in advance.

use serde::{Deserialize, Serialize};

/// What a script says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Name, used to refer to the workflow.
    pub name: String,
    /// One line, shown in the approval dialog.
    pub description: String,
    /// Optional longer title.
    pub title: Option<String>,
    /// Optional "use this when".
    pub when_to_use: Option<String>,
    /// Optional progress groups.
    pub phases: Vec<Phase>,
}

/// A progress group a script declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    /// The name, which has to match a `phase()` call to group with it.
    pub title: String,
    /// Optional longer note.
    pub detail: Option<String>,
    /// Optional model for the agents in this phase.
    pub model: Option<String>,
}

/// Something wrong with a script or its metadata.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    /// The script does not start with a `meta` export.
    #[error(
        "a workflow script starts with `export const meta = {{ … }}`; this one does not, so \
         there is nothing to show before it runs"
    )]
    MissingExport,
    /// The `meta` object is not a literal.
    #[error("meta must be a literal object: {0}")]
    NotLiteral(String),
    /// A required field is missing or empty.
    #[error("meta is missing {0}")]
    MissingField(&'static str),
    /// The object could not be read.
    #[error("could not read meta: {0}")]
    Malformed(String),
    /// The script cannot be run at all.
    #[error("{0}")]
    InvalidScript(String),
}

/// The largest script accepted, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 524_288;

/// Read the `meta` block of a script.
///
/// Returns the metadata and the body — everything after the export — because
/// the body is what runs, and the caller should not have to re-find the
/// boundary.
pub fn read(script: &str) -> Result<(Meta, String), MetaError> {
    if script.len() > MAX_SCRIPT_BYTES {
        return Err(MetaError::InvalidScript(format!(
            "script exceeds {MAX_SCRIPT_BYTES} bytes"
        )));
    }

    let start = find_meta_start(script).ok_or(MetaError::MissingExport)?;
    let open = script[start..]
        .find('{')
        .map(|offset| start + offset)
        .ok_or_else(|| MetaError::NotLiteral("no object follows the export".to_string()))?;

    let end = matching_brace(script, open).ok_or_else(|| {
        MetaError::Malformed("the meta object is not closed".to_string())
    })?;

    let object = &script[open..=end];
    let value = read_literal_object(object)?;
    let meta = build(&value)?;

    let body = script[end + 1..].trim_start().to_string();
    Ok((meta, body))
}

/// Find where `export const meta =` starts, skipping leading whitespace and
/// comments.
fn find_meta_start(script: &str) -> Option<usize> {
    let mut rest = script;
    let mut offset = 0;

    loop {
        let trimmed = rest.trim_start();
        offset += rest.len() - trimmed.len();
        rest = trimmed;

        if let Some(after) = rest.strip_prefix("//") {
            let line_end = after.find('\n').unwrap_or(after.len());
            offset += 2 + line_end;
            rest = &rest[(2 + line_end).min(rest.len())..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            let end = after.find("*/")? + 2;
            offset += 2 + end;
            rest = &rest[(2 + end).min(rest.len())..];
            continue;
        }
        break;
    }

    rest.starts_with("export") .then_some(offset)
}

/// Find the brace that closes the one at `open`.
///
/// Quotes and comments are respected: a `}` inside a string is a character,
/// not the end of the object.
fn matching_brace(script: &str, open: usize) -> Option<usize> {
    let bytes = script.as_bytes();
    let mut depth = 0usize;
    let mut index = open;
    let mut quote: Option<u8> = None;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active {
                quote = None;
            }
            index += 1;
            continue;
        }

        match byte {
            b'\'' | b'"' | b'`' => quote = Some(byte),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Read an object literal into a map, refusing anything not literal.
fn read_literal_object(text: &str) -> Result<Vec<(String, Literal)>, MetaError> {
    let inner = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| MetaError::NotLiteral("not an object".to_string()))?;

    let mut entries = Vec::new();
    for entry in split_top_level(inner)? {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, value) = entry
            .split_once(':')
            .ok_or_else(|| MetaError::NotLiteral(format!("'{entry}' is not key: value")))?;

        let key = read_key(key.trim())?;
        let value = read_value(value.trim())?;
        entries.push((key, value));
    }
    Ok(entries)
}

/// Split on commas that are not inside a nested object, array, or string.
fn split_top_level(text: &str) -> Result<Vec<String>, MetaError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in text.chars() {
        if let Some(active) = quote {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' | '`' => {
                quote = Some(ch);
                current.push(ch);
            }
            '{' | '[' | '(' => {
                depth += 1;
                current.push(ch);
            }
            '}' | ']' | ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }

    if quote.is_some() {
        return Err(MetaError::Malformed("a string is not closed".to_string()));
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

/// Keys are bare identifiers or quoted strings, never computed.
fn read_key(text: &str) -> Result<String, MetaError> {
    if let Some(quoted) = text.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Ok(quoted.to_string());
    }
    if let Some(quoted) = text.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Ok(quoted.to_string());
    }

    if text.starts_with('[') {
        return Err(MetaError::NotLiteral(
            "a computed key needs evaluating".to_string(),
        ));
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return Err(MetaError::NotLiteral(format!("'{text}' is not a key")));
    }
    Ok(text.to_string())
}

/// A value a literal can hold.
#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Text(String),
    Bool(bool),
    Number(f64),
    Array(Vec<Literal>),
    Object(Vec<(String, Literal)>),
}

fn read_value(text: &str) -> Result<Literal, MetaError> {
    if text.is_empty() {
        return Err(MetaError::NotLiteral("an empty value".to_string()));
    }

    if let Some(inner) = text.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Ok(Literal::Text(unescape(inner)?));
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        return Ok(Literal::Text(unescape(inner)?));
    }
    if text.starts_with('`') {
        return Err(MetaError::NotLiteral(
            "template interpolation is not allowed here".to_string(),
        ));
    }
    if text.starts_with('[') {
        let inner = text
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .ok_or_else(|| MetaError::NotLiteral("an array is not closed".to_string()))?;
        let mut items = Vec::new();
        for item in split_top_level(inner)? {
            let item = item.trim();
            if item.is_empty() {
                return Err(MetaError::NotLiteral(
                    "a sparse array is not a literal".to_string(),
                ));
            }
            items.push(read_value(item)?);
        }
        return Ok(Literal::Array(items));
    }
    if text.starts_with('{') {
        return Ok(Literal::Object(read_literal_object(text)?));
    }

    match text {
        "true" => Ok(Literal::Bool(true)),
        "false" => Ok(Literal::Bool(false)),
        "null" | "undefined" => Ok(Literal::Text(String::new())),
        other => other
            .parse::<f64>()
            .map(Literal::Number)
            .map_err(|_| {
                MetaError::NotLiteral(format!("'{other}' would have to be evaluated"))
            }),
    }
}

fn unescape(text: &str) -> Result<String, MetaError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some(other) => {
                return Err(MetaError::NotLiteral(format!(
                    "'\\{other}' is an escape this reader does not take"
                )))
            }
            None => return Err(MetaError::Malformed("a string ends in a backslash".to_string())),
        }
    }
    Ok(out)
}

/// Turn the read object into metadata, checking what has to be there.
fn build(entries: &[(String, Literal)]) -> Result<Meta, MetaError> {
    let mut name = None;
    let mut description = None;
    let mut title = None;
    let mut when_to_use = None;
    let mut phases = Vec::new();

    for (key, value) in entries {
        match key.as_str() {
            "name" => name = as_text(value),
            "description" => description = as_text(value),
            "title" => title = as_text(value),
            "whenToUse" => when_to_use = as_text(value),
            "phases" => {
                let Literal::Array(items) = value else {
                    return Err(MetaError::NotLiteral(
                        "phases must be an array of objects".to_string(),
                    ));
                };
                for item in items {
                    let Literal::Object(fields) = item else {
                        return Err(MetaError::NotLiteral(
                            "each phase must be an object".to_string(),
                        ));
                    };
                    phases.push(read_phase(fields)?);
                }
            }
            // Known to exist in the reference and ignored here, rather than
            // refused: a script written for a newer runtime still runs.
            _ => {}
        }
    }

    let name = name.filter(|value| !value.trim().is_empty());
    let description = description.filter(|value| !value.trim().is_empty());

    Ok(Meta {
        name: name.ok_or(MetaError::MissingField("a name"))?,
        description: description.ok_or(MetaError::MissingField("a description"))?,
        title,
        when_to_use,
        phases,
    })
}

fn read_phase(fields: &[(String, Literal)]) -> Result<Phase, MetaError> {
    let mut title = None;
    let mut detail = None;
    let mut model = None;

    for (key, value) in fields {
        match key.as_str() {
            "title" => title = as_text(value),
            "detail" => detail = as_text(value),
            "model" => model = as_text(value),
            _ => {}
        }
    }

    Ok(Phase {
        title: title
            .filter(|value| !value.trim().is_empty())
            .ok_or(MetaError::MissingField("a phase title"))?,
        detail,
        model,
    })
}

fn as_text(value: &Literal) -> Option<String> {
    match value {
        Literal::Text(text) => Some(text.clone()),
        Literal::Number(number) => Some(number.to_string()),
        Literal::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"export const meta = {
  name: 'review-changes',
  description: 'Review the pending diff and verify each finding.',
  title: 'Review',
  whenToUse: 'before shipping',
  phases: [
    { title: 'Review' },
    { title: 'Verify', detail: 'two votes each', model: 'claude-sonnet-4-6' },
  ],
}

phase('Review')
const found = await parallel([() => agent('look')])
"#;

    #[test]
    fn a_script_yields_its_metadata_and_its_body() {
        let (meta, body) = read(SCRIPT).unwrap();

        assert_eq!(meta.name, "review-changes");
        assert_eq!(
            meta.description,
            "Review the pending diff and verify each finding."
        );
        assert_eq!(meta.title.as_deref(), Some("Review"));
        assert_eq!(meta.when_to_use.as_deref(), Some("before shipping"));
        assert_eq!(meta.phases.len(), 2);
        assert_eq!(meta.phases[0].title, "Review");
        assert_eq!(meta.phases[1].detail.as_deref(), Some("two votes each"));
        assert_eq!(
            meta.phases[1].model.as_deref(),
            Some("claude-sonnet-4-6")
        );

        // The body is what runs, and it starts after the export.
        assert!(body.starts_with("phase('Review')"), "{body}");
        assert!(!body.contains("name:"), "{body}");
    }

    #[test]
    fn a_script_without_the_export_is_refused() {
        let error = read("console.log('hi')").unwrap_err();
        assert!(error.to_string().contains("export const meta"), "{error}");
    }

    #[test]
    fn leading_comments_and_blank_lines_do_not_hide_the_export() {
        let script = "// a note\n\n/* another */\nexport const meta = { name: 'a', description: 'b' }\nrun()";
        let (meta, body) = read(script).unwrap();
        assert_eq!(meta.name, "a");
        assert_eq!(body, "run()");
    }

    #[test]
    fn a_required_field_has_to_be_there() {
        let missing_name = "export const meta = { description: 'b' }";
        assert!(matches!(
            read(missing_name).unwrap_err(),
            MetaError::MissingField("a name")
        ));

        let empty = "export const meta = { name: '  ', description: 'b' }";
        assert!(matches!(
            read(empty).unwrap_err(),
            MetaError::MissingField("a name")
        ));

        let no_description = "export const meta = { name: 'a' }";
        assert!(matches!(
            read(no_description).unwrap_err(),
            MetaError::MissingField("a description")
        ));
    }

    #[test]
    fn a_meta_that_would_have_to_be_evaluated_is_refused() {
        for script in [
            "export const meta = { name: NAME, description: 'b' }",
            "export const meta = { name: 'a', description: `hi ${x}` }",
            "export const meta = { name: 'a', description: 'b', phases: [makePhase()] }",
            "export const meta = { ['na' + 'me']: 'a', description: 'b' }",
            "export const meta = { name: 'a', description: 'b', ...extra }",
        ] {
            let result = read(script);
            assert!(result.is_err(), "{script} was accepted");
        }
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_object() {
        let script = r#"export const meta = { name: 'a', description: 'ends with }' }
body()"#;
        let (meta, body) = read(script).unwrap();
        assert_eq!(meta.description, "ends with }");
        assert_eq!(body, "body()");
    }

    #[test]
    fn a_phase_without_a_title_is_refused() {
        let script = "export const meta = { name: 'a', description: 'b', phases: [{ detail: 'x' }] }";
        let error = read(script).unwrap_err();
        assert!(error.to_string().contains("phase title"), "{error}");
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_refused() {
        let script = "export const meta = { name: 'a', description: 'b', somethingNew: true }";
        assert_eq!(read(script).unwrap().0.name, "a");
    }

    #[test]
    fn an_unclosed_object_says_so() {
        let error = read("export const meta = { name: 'a' ").unwrap_err();
        assert!(error.to_string().contains("not closed"), "{error}");
    }

    #[test]
    fn a_script_over_the_size_cap_is_refused_before_anything_else() {
        let huge = format!(
            "export const meta = {{ name: 'a', description: 'b' }}\n{}",
            "x".repeat(MAX_SCRIPT_BYTES)
        );
        let error = read(&huge).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn escapes_in_a_string_are_read() {
        let script = r#"export const meta = { name: 'a', description: 'line\nquote\'s' }"#;
        let (meta, _) = read(script).unwrap();
        assert_eq!(meta.description, "line\nquote's");
    }
}
