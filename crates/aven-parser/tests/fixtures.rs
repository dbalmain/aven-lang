use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use aven_core::{Diagnostic, Severity};
use aven_parser::{
    Declaration, DeclarationKind, DeclarationPhase, DeclarationShape, Expr, ExprKind, Item,
    Literal, MatchArm, Module, Param, PropagationMode, RecordEntry, Token,
};

const PARSER_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/parser");
const PARSER_AST_FIXTURE_ROOT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/parser/ast");
const DECLARATION_FIXTURE_ROOT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/declarations");
const NAME_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/names");
const LEXER_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lexer");
const LAYOUT_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/layout");

#[test]
fn valid_parser_fixtures_have_no_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(PARSER_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );
    }

    Ok(())
}

#[test]
fn invalid_parser_fixtures_match_expected_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(PARSER_FIXTURE_ROOT, "invalid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);
        let actual = render_diagnostics(&output.diagnostics);
        let expected_path = path.with_extension("diag");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "diagnostics for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn parser_ast_fixtures_match_expected_tree() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(PARSER_AST_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );

        let actual = render_module_ast(&output.module);
        let expected_path = path.with_extension("ast");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "AST summary for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn declaration_fixtures_match_expected_output() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(DECLARATION_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced parse diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );

        let declarations = aven_parser::collect_declarations(&output.module);
        let actual = render_declarations(&declarations);
        let expected_path = path.with_extension("decl");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "declarations for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn valid_name_fixtures_have_no_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(NAME_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced parse diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );

        let analysis = aven_parser::analyze_names(&output.module);

        assert!(
            analysis.diagnostics.is_empty(),
            "{} unexpectedly produced name diagnostics:\n{}",
            path.display(),
            render_diagnostics(&analysis.diagnostics)
        );
    }

    Ok(())
}

#[test]
fn invalid_name_fixtures_match_expected_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(NAME_FIXTURE_ROOT, "invalid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::parse_module(&source);

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced parse diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );

        let analysis = aven_parser::analyze_names(&output.module);
        let actual = render_diagnostics(&analysis.diagnostics);
        let expected_path = path.with_extension("diag");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "name diagnostics for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn valid_lexer_fixtures_match_expected_tokens() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(LEXER_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::lex_source(&source);
        let actual = render_tokens(&output.tokens);
        let expected_path = path.with_extension("tokens");
        let expected = fs::read_to_string(&expected_path)?;

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );
        assert_eq!(
            actual,
            expected,
            "tokens for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn invalid_lexer_fixtures_match_expected_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(LEXER_FIXTURE_ROOT, "invalid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::lex_source(&source);
        let actual = render_diagnostics(&output.diagnostics);
        let expected_path = path.with_extension("diag");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "diagnostics for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn valid_layout_fixtures_match_expected_tokens() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(LAYOUT_FIXTURE_ROOT, "valid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::layout_source(&source);
        let actual = render_tokens(&output.tokens);
        let expected_path = path.with_extension("layout");
        let expected = fs::read_to_string(&expected_path)?;

        assert!(
            output.diagnostics.is_empty(),
            "{} unexpectedly produced diagnostics:\n{}",
            path.display(),
            render_diagnostics(&output.diagnostics)
        );
        assert_eq!(
            actual,
            expected,
            "layout tokens for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

#[test]
fn invalid_layout_fixtures_match_expected_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files(LAYOUT_FIXTURE_ROOT, "invalid")? {
        let source = fs::read_to_string(&path)?;
        let output = aven_parser::layout_source(&source);
        let actual = render_diagnostics(&output.diagnostics);
        let expected_path = path.with_extension("diag");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "diagnostics for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

fn fixture_files(root: &str, group: &str) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(Path::new(root).join(group))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("av") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn render_tokens(tokens: &[Token]) -> String {
    let mut output = String::new();

    for token in tokens {
        let _ = writeln!(
            output,
            "{}..{} {}",
            token.span.start,
            token.span.end,
            token.kind.describe()
        );
    }

    output
}

fn render_module_ast(module: &Module) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "module");

    for item in &module.items {
        render_item_ast(&mut output, item, 1);
    }

    output
}

fn render_declarations(declarations: &[Declaration]) -> String {
    let mut output = String::new();

    for declaration in declarations {
        let _ = writeln!(
            output,
            "{} {} phase={} annotated={} shape={} span={}..{} name={}..{}",
            declaration_kind_name(declaration.kind),
            declaration.name,
            declaration_phase_name(declaration.phase),
            declaration.is_annotated,
            declaration_shape_name(&declaration.shape),
            declaration.span.start,
            declaration.span.end,
            declaration.name_span.start,
            declaration.name_span.end
        );
    }

    output
}

fn declaration_shape_name(shape: &DeclarationShape) -> String {
    match shape {
        DeclarationShape::Value => "value".to_owned(),
        DeclarationShape::Callable(callable) => {
            let parameters = callable
                .parameter_annotations
                .iter()
                .map(|is_annotated| if *is_annotated { "typed" } else { "?" })
                .collect::<Vec<_>>();
            let result = if callable.has_result_annotation {
                "typed"
            } else {
                "?"
            };
            format!("fn({})->{result}", parameters.join(","))
        }
    }
}

fn declaration_kind_name(kind: DeclarationKind) -> &'static str {
    match kind {
        DeclarationKind::Binding => "binding",
        DeclarationKind::Function => "function",
        DeclarationKind::Signature => "signature",
    }
}

fn declaration_phase_name(phase: DeclarationPhase) -> &'static str {
    match phase {
        DeclarationPhase::Runtime => "runtime",
        DeclarationPhase::Comptime => "comptime",
    }
}

fn render_item_ast(output: &mut String, item: &Item, indent: usize) {
    match item {
        Item::Binding(binding) => {
            write_indent(output, indent);
            let _ = writeln!(output, "binding {}", binding.name);
            if let Some(annotation) = &binding.annotation {
                write_indent(output, indent + 1);
                let _ = writeln!(output, "annotation");
                render_expr_ast(output, annotation, indent + 2);
            }
            write_indent(output, indent + 1);
            let _ = writeln!(output, "value");
            render_expr_ast(output, &binding.value, indent + 2);
        }
        Item::Signature(signature) => {
            write_indent(output, indent);
            let _ = writeln!(output, "signature {}", signature.name);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "annotation");
            render_expr_ast(output, &signature.annotation, indent + 2);
        }
        Item::Expr(expr) => {
            write_indent(output, indent);
            let _ = writeln!(output, "expr");
            render_expr_ast(output, expr, indent + 1);
        }
    }
}

fn render_expr_ast(output: &mut String, expr: &Expr, indent: usize) {
    match &expr.kind {
        ExprKind::Missing => {
            write_indent(output, indent);
            let _ = writeln!(output, "missing");
        }
        ExprKind::Literal(literal) => render_literal_ast(output, literal, indent),
        ExprKind::Name(name) => {
            write_indent(output, indent);
            let _ = writeln!(output, "name {name}");
        }
        ExprKind::ComptimeName(name) => {
            write_indent(output, indent);
            let _ = writeln!(output, "comptime {name}");
        }
        ExprKind::Tag(name) => {
            write_indent(output, indent);
            let _ = writeln!(output, "tag {name}");
        }
        ExprKind::Group(inner) => {
            write_indent(output, indent);
            let _ = writeln!(output, "group");
            render_expr_ast(output, inner, indent + 1);
        }
        ExprKind::Tuple(items) => {
            write_indent(output, indent);
            let _ = writeln!(output, "tuple");
            render_expr_list_ast(output, items, indent + 1);
        }
        ExprKind::Array(items) => {
            write_indent(output, indent);
            let _ = writeln!(output, "array");
            render_expr_list_ast(output, items, indent + 1);
        }
        ExprKind::Record(entries) => {
            write_indent(output, indent);
            let _ = writeln!(output, "record");
            render_record_entries_ast(output, entries, indent + 1);
        }
        ExprKind::Set(entries) => {
            write_indent(output, indent);
            let _ = writeln!(output, "set");
            render_record_entries_ast(output, entries, indent + 1);
        }
        ExprKind::Index { callee, args } => {
            write_indent(output, indent);
            let _ = writeln!(output, "index");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "callee");
            render_expr_ast(output, callee, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "args");
            render_expr_list_ast(output, args, indent + 2);
        }
        ExprKind::Nullable(inner) => {
            write_indent(output, indent);
            let _ = writeln!(output, "nullable");
            render_expr_ast(output, inner, indent + 1);
        }
        ExprKind::Arrow { params, result } => {
            write_indent(output, indent);
            let _ = writeln!(output, "arrow");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "params");
            render_expr_list_ast(output, params, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "result");
            render_expr_ast(output, result, indent + 2);
        }
        ExprKind::FieldAccess {
            receiver,
            field,
            null_safe,
            ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "field-access {field} null_safe={null_safe}");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "receiver");
            render_expr_ast(output, receiver, indent + 2);
        }
        ExprKind::Call { callee, args } => {
            write_indent(output, indent);
            let _ = writeln!(output, "call");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "callee");
            render_expr_ast(output, callee, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "args");
            render_expr_list_ast(output, args, indent + 2);
        }
        ExprKind::Binary {
            left,
            operator,
            right,
            ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "binary {operator}");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "left");
            render_expr_ast(output, left, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "right");
            render_expr_ast(output, right, indent + 2);
        }
        ExprKind::Unary {
            operator, value, ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "unary {operator}");
            render_expr_ast(output, value, indent + 1);
        }
        ExprKind::Propagate { value, mode, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "propagate {}", propagation_mode_name(*mode));
            render_expr_ast(output, value, indent + 1);
        }
        ExprKind::Match { subject, arms, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "match");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "subject");
            render_expr_ast(output, subject, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "arms");
            for arm in arms {
                render_match_arm_ast(output, arm, indent + 2);
            }
        }
        ExprKind::Lambda {
            params,
            return_annotation,
            body,
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "lambda");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "params");
            for param in params {
                render_param_ast(output, param, indent + 2);
            }
            if let Some(return_annotation) = return_annotation {
                write_indent(output, indent + 1);
                let _ = writeln!(output, "return");
                render_expr_ast(output, return_annotation, indent + 2);
            }
            write_indent(output, indent + 1);
            let _ = writeln!(output, "body");
            render_expr_ast(output, body, indent + 2);
        }
        ExprKind::Block(items) => {
            write_indent(output, indent);
            let _ = writeln!(output, "block");
            for item in items {
                render_item_ast(output, item, indent + 1);
            }
        }
    }
}

fn render_expr_list_ast(output: &mut String, items: &[Expr], indent: usize) {
    for item in items {
        render_expr_ast(output, item, indent);
    }
}

fn render_record_entries_ast(output: &mut String, entries: &[RecordEntry], indent: usize) {
    for entry in entries {
        render_record_entry_ast(output, entry, indent);
    }
}

fn render_record_entry_ast(output: &mut String, entry: &RecordEntry, indent: usize) {
    match entry {
        RecordEntry::Field {
            name,
            value,
            overwrite,
            optional,
            ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(
                output,
                "field {name} optional={optional} overwrite={overwrite}"
            );
            render_expr_ast(output, value, indent + 1);
        }
        RecordEntry::Shorthand { name, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "shorthand {name}");
        }
        RecordEntry::Spread {
            value, overwrite, ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "spread overwrite={overwrite}");
            render_expr_ast(output, value, indent + 1);
        }
        RecordEntry::Delete { name, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "delete {name}");
        }
        RecordEntry::DeleteComputed { key, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "delete computed");
            render_expr_ast(output, key, indent + 1);
        }
        RecordEntry::Rename { from, to, .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "rename {from} -> {to}");
        }
        RecordEntry::Iteration {
            source,
            binder,
            guard,
            body,
            ..
        } => {
            write_indent(output, indent);
            let _ = writeln!(output, "iteration {binder}");
            write_indent(output, indent + 1);
            let _ = writeln!(output, "source");
            render_expr_ast(output, source, indent + 2);
            write_indent(output, indent + 1);
            let _ = writeln!(output, "guard");
            if let Some(guard) = guard {
                render_expr_ast(output, guard, indent + 2);
            } else {
                write_indent(output, indent + 2);
                let _ = writeln!(output, "none");
            }
            write_indent(output, indent + 1);
            let _ = writeln!(output, "body");
            render_record_entries_ast(output, body, indent + 2);
        }
        RecordEntry::Open { .. } => {
            write_indent(output, indent);
            let _ = writeln!(output, "open");
        }
        RecordEntry::Element(expr) => {
            write_indent(output, indent);
            let _ = writeln!(output, "element");
            render_expr_ast(output, expr, indent + 1);
        }
    }
}

fn render_match_arm_ast(output: &mut String, arm: &MatchArm, indent: usize) {
    write_indent(output, indent);
    let _ = writeln!(output, "arm");
    write_indent(output, indent + 1);
    let _ = writeln!(output, "pattern");
    render_expr_ast(output, &arm.pattern, indent + 2);
    for guard in &arm.guards {
        write_indent(output, indent + 1);
        let _ = writeln!(output, "guard");
        render_expr_ast(output, guard, indent + 2);
    }
    write_indent(output, indent + 1);
    let _ = writeln!(output, "body");
    render_expr_ast(output, &arm.body, indent + 2);
}

fn render_param_ast(output: &mut String, param: &Param, indent: usize) {
    write_indent(output, indent);
    let _ = writeln!(output, "param {} comptime={}", param.name, param.comptime);
    if let Some(annotation) = &param.annotation {
        write_indent(output, indent + 1);
        let _ = writeln!(output, "annotation");
        render_expr_ast(output, annotation, indent + 2);
    }
}

fn render_literal_ast(output: &mut String, literal: &Literal, indent: usize) {
    write_indent(output, indent);
    match literal {
        Literal::Number(number) => {
            let _ = writeln!(output, "number {number}");
        }
        Literal::String(text) => {
            let _ = writeln!(output, "string {text}");
        }
        Literal::Regex(regex) => {
            let _ = writeln!(output, "regex {regex}");
        }
        Literal::Path(path) => {
            let _ = writeln!(output, "path {path}");
        }
        Literal::Label(label) => {
            let _ = writeln!(output, "label {label}");
        }
    }
}

fn propagation_mode_name(mode: PropagationMode) -> &'static str {
    match mode {
        PropagationMode::ReturnError => "return-error",
        PropagationMode::Panic => "panic",
    }
}

fn write_indent(output: &mut String, indent: usize) {
    for _ in 0..indent {
        output.push_str("  ");
    }
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();

    for diagnostic in diagnostics {
        let code = diagnostic.code.as_deref().unwrap_or("none");
        let _ = writeln!(
            output,
            "{} {}: {}",
            severity_name(diagnostic.severity),
            code,
            diagnostic.message
        );

        for label in &diagnostic.labels {
            let _ = writeln!(
                output,
                "  label {}..{}: {}",
                label.span.start, label.span.end, label.message
            );
        }

        for note in &diagnostic.notes {
            let _ = writeln!(output, "  note: {note}");
        }
    }

    output
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}
