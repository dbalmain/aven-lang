use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use aven_check::{Type, build};
use aven_core::{Diagnostic, Severity};

const CHECK_FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/check");

#[test]
fn valid_check_fixtures_have_no_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files("valid")? {
        let source = fs::read_to_string(&path)?;
        let parse = aven_parser::parse_module(&source);

        assert!(
            parse.diagnostics.is_empty(),
            "{} unexpectedly produced parse diagnostics",
            path.display()
        );

        let name_errors = name_error_diagnostics(&parse.module);
        assert!(
            name_errors.is_empty(),
            "{} unexpectedly produced name errors:\n{}",
            path.display(),
            render_diagnostics(&name_errors)
        );

        let globals = fixture_globals();
        let check = aven_check::check_module_with_globals(&parse.module, &globals);

        assert!(
            check.diagnostics.is_empty(),
            "{} unexpectedly produced check diagnostics:\n{}",
            path.display(),
            render_diagnostics(&check.diagnostics)
        );
    }

    Ok(())
}

#[test]
fn invalid_check_fixtures_match_expected_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files("invalid")? {
        let source = fs::read_to_string(&path)?;
        let parse = aven_parser::parse_module(&source);

        assert!(
            parse.diagnostics.is_empty(),
            "{} unexpectedly produced parse diagnostics",
            path.display()
        );

        let globals = fixture_globals();
        let mut diagnostics = name_error_diagnostics(&parse.module);
        let check = aven_check::check_module_with_globals(&parse.module, &globals);
        diagnostics.extend(check.diagnostics);
        let actual = render_diagnostics(&diagnostics);
        let expected_path = path.with_extension("diag");
        let expected = fs::read_to_string(&expected_path)?;

        assert_eq!(
            actual,
            expected,
            "check diagnostics for {} did not match {}",
            path.display(),
            expected_path.display()
        );
    }

    Ok(())
}

fn fixture_files(kind: &str) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let root = Path::new(CHECK_FIXTURE_ROOT).join(kind);
    let mut paths = Vec::new();

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|extension| extension.to_str()) == Some("av") {
            paths.push(path);
        }
    }

    paths.sort();
    Ok(paths)
}

fn name_error_diagnostics(module: &aven_parser::Module) -> Vec<Diagnostic> {
    aven_parser::analyze_names(module)
        .diagnostics
        .into_iter()
        .filter(Diagnostic::is_error)
        .collect()
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();

    for diagnostic in diagnostics {
        let code = diagnostic.code.as_deref().unwrap_or("none");
        let _ = writeln!(
            output,
            "{} {code}: {}",
            severity_name(diagnostic.severity),
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

fn fixture_globals() -> Vec<(String, Type)> {
    let logger = build::record(vec![
        ("info", logger_method_type()),
        ("error", logger_method_type()),
    ]);

    vec![
        ("logger".to_owned(), logger),
        (
            "dbg".to_owned(),
            build::function(vec![build::var("a")], build::var("a")),
        ),
        (
            "write".to_owned(),
            build::function(vec![build::text()], build::empty_record()),
        ),
        (
            "writeLine".to_owned(),
            build::function(vec![build::text()], build::empty_record()),
        ),
        (
            "readLine".to_owned(),
            build::function(vec![], build::optional(build::text())),
        ),
        ("readAll".to_owned(), build::function(vec![], build::text())),
    ]
}

fn logger_method_type() -> Type {
    build::function_opt(
        vec![build::text()],
        vec![build::open_record(vec![])],
        build::unit(),
    )
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}
