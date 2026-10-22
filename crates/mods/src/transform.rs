//! TypeScript and JSX lowering for mod files.
//!
//! `.ts`, `.tsx` and `.jsx` files are parsed with oxc, TypeScript types are
//! stripped, JSX is lowered to the classic runtime (`h(...)` and `Fragment`),
//! and the result is printed back to JavaScript for `boa_engine`. The work
//! happens at load time on the loading thread, so a syntax error is reported
//! once with its file and line and the mod is skipped.
//!
//! The JSX configuration is built in source rather than read from a
//! `tsconfig.json`: classic runtime, `pragma: "h"`, `pragmaFrag: "Fragment"`.

use std::ffi::OsStr;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer, TypeScriptOptions};

/// Lower one mod file to JavaScript.
///
/// `path` supplies the extension the parser uses to decide between JavaScript,
/// TypeScript, JSX and TSX. The caller only invokes this for extensions other
/// than `.js`.
pub fn transform(source: &str, path: &Path) -> Result<String, String> {
    let allocator = Allocator::default();
    // Lower-case the extension so `.TSX` and `.Ts` choose the same parse as
    // their lower-case spellings; the caller already matched them
    // case-insensitively.
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let source_type = SourceType::from_extension(&extension)
        .map_err(|error| format!("could not determine the source type: {error}"))?;

    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(diagnostic) = parsed.diagnostics.first() {
        return Err(describe(diagnostic, source));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .build(&program);
    if let Some(diagnostic) = semantic.diagnostics.first() {
        return Err(describe(diagnostic, source));
    }

    let options = TransformOptions {
        typescript: TypeScriptOptions {
            jsx_pragma: "h".into(),
            jsx_pragma_frag: "Fragment".into(),
            ..TypeScriptOptions::default()
        },
        jsx: JsxOptions {
            runtime: JsxRuntime::Classic,
            pragma: Some("h".to_string()),
            pragma_frag: Some("Fragment".to_string()),
            ..JsxOptions::default()
        },
        ..TransformOptions::default()
    };

    let transformed = Transformer::new(&allocator, path, &options)
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if let Some(diagnostic) = transformed.diagnostics.first() {
        return Err(describe(diagnostic, source));
    }

    Ok(Codegen::new().build(&program).code)
}

/// One diagnostic as `line N, column M: message`.
///
/// File names are added by the caller, which owns the path. A diagnostic with
/// no label is reported at the start of the file.
fn describe(diagnostic: &OxcDiagnostic, source: &str) -> String {
    let (line, column) = diagnostic
        .labels
        .first()
        .map_or((1, 1), |label| line_column(source, label.offset()));
    format!("line {line}, column {column}: {}", diagnostic.message)
}

/// Convert a byte offset into a one-based line and column.
fn line_column(source: &str, offset: u32) -> (usize, usize) {
    let offset = usize::try_from(offset).unwrap_or_default();
    let prefix = source.get(..offset).unwrap_or(source);
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rfind('\n')
        .map_or(prefix.len(), |index| prefix.len().saturating_sub(index + 1))
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_annotations_are_removed() {
        let source = "const value: number = 1;\nfunction register(on: unknown): void {}\n";
        let output = transform(source, Path::new("mod.ts")).unwrap();
        assert!(output.contains("const value = 1"), "{output}");
        assert!(!output.contains(": number"), "{output}");
        assert!(!output.contains(": void"), "{output}");
    }

    #[test]
    fn jsx_is_lowered_to_h_and_fragment() {
        let source = "const node = <box title=\"t\"><text style=\"bold\">hi</text></box>;\n\
                      const list = <><text>a</text></>;\n";
        let output = transform(source, Path::new("mod.tsx")).unwrap();
        assert!(output.contains("h(\"box\""), "{output}");
        assert!(output.contains("h(\"text\""), "{output}");
        assert!(output.contains("Fragment"), "{output}");
        assert!(!output.contains("<box"), "{output}");
    }

    #[test]
    fn a_syntax_error_names_the_line() {
        let error = transform("const value = ;\n", Path::new("broken.ts")).unwrap_err();
        assert!(error.contains("line 1"), "{error}");
    }

    #[test]
    fn a_multibyte_prefix_does_not_break_the_location() {
        assert_eq!(line_column("héllo\nworld", 8), (2, 2));
        // Offsets past the end clamp to the end rather than panicking.
        assert_eq!(line_column("abc", u32::MAX), (1, 4));
    }
}
