use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use aven_core::{Diagnostic, Severity};

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/parser");

#[test]
fn valid_parser_fixtures_have_no_diagnostics() -> Result<(), Box<dyn Error>> {
    for path in fixture_files("valid")? {
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
    for path in fixture_files("invalid")? {
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

fn fixture_files(group: &str) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(Path::new(FIXTURE_ROOT).join(group))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("av") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
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
