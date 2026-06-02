use std::fs;
use std::io::{self, IsTerminal};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ariadne::{Config as AriadneConfig, Label as AriadneLabel, Report, ReportKind, Source};
use aven_core::{Diagnostic as AvenDiagnostic, FileId, Severity, SourceFile};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "aven")]
#[command(about = "Aven language tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse a file and report diagnostics.
    Check {
        /// Source file to check.
        path: PathBuf,

        /// Diagnostic output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    /// Print lexer tokens for debugging parser work.
    Tokens {
        /// Source file to tokenize.
        path: PathBuf,
    },

    /// Print layout tokens for debugging parser work.
    Layout {
        /// Source file to layout.
        path: PathBuf,
    },

    /// Format a source file.
    Fmt {
        /// Check formatting without writing changes.
        #[arg(long)]
        check: bool,

        /// Source file to format.
        path: PathBuf,
    },

    /// Start the language server on stdin/stdout.
    Lsp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Check { path, format } => check(&path, format),
        Command::Tokens { path } => tokens(&path),
        Command::Layout { path } => layout(&path),
        Command::Fmt { check, path } => fmt(&path, check),
        Command::Lsp => {
            aven_lsp::run_stdio().await;
            Ok(())
        }
    }
}

fn check(path: &Path, format: OutputFormat) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let file = SourceFile::new(
        FileId(0),
        path.display().to_string(),
        Some(path.to_path_buf()),
        source,
    );
    let output = aven_parser::parse_source(&file);
    let mut diagnostics = output.diagnostics.clone();

    if !diagnostics.iter().any(AvenDiagnostic::is_error) {
        // Name analysis intentionally waits for a clean parse in the first pass.
        // Analyzing recovered `Missing` trees is a later diagnostics-recovery task.
        let name_analysis = aven_parser::analyze_names(&output.module);
        let check_output = aven_check::check_module(&output.module);
        diagnostics.extend(name_analysis.diagnostics);
        diagnostics.extend(check_output.diagnostics);
    }

    diagnostics.sort_by_key(diagnostic_sort_key);

    match format {
        OutputFormat::Text => {
            if !diagnostics.is_empty() {
                print_diagnostics(path, file.source(), &diagnostics)?;
            }
        }
        OutputFormat::Json => print_json_diagnostics(&file, &diagnostics)?,
    }

    if diagnostics.iter().any(AvenDiagnostic::is_error) {
        bail!("check failed");
    }

    if format == OutputFormat::Text {
        println!(
            "{}: ok (parse, name, and annotation checks only; inference is not implemented yet)",
            path.display()
        );
    }

    Ok(())
}

fn diagnostic_sort_key(diagnostic: &AvenDiagnostic) -> (usize, usize) {
    diagnostic
        .labels
        .first()
        .map_or((usize::MAX, usize::MAX), |label| {
            (label.span.start, label.span.end)
        })
}

fn print_json_diagnostics(file: &SourceFile, diagnostics: &[AvenDiagnostic]) -> Result<()> {
    let output = json!({
        "fileId": file.id.0,
        "path": file.path.as_ref().map(|path| path.display().to_string()),
        "name": file.name.as_str(),
        "ok": !diagnostics.iter().any(AvenDiagnostic::is_error),
        "diagnostics": diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    });

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn diagnostic_json(diagnostic: &AvenDiagnostic) -> serde_json::Value {
    json!({
        "severity": severity_name(diagnostic.severity),
        "code": diagnostic.code,
        "message": diagnostic.message,
        "labels": diagnostic.labels.iter().map(|label| {
            json!({
                "span": {
                    "start": label.span.start,
                    "end": label.span.end,
                },
                "message": label.message,
            })
        }).collect::<Vec<_>>(),
        "notes": diagnostic.notes,
    })
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

fn tokens(path: &Path) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let output = aven_parser::lex_source(&source);

    if !output.diagnostics.is_empty() {
        print_diagnostics(path, &source, &output.diagnostics)?;
    }

    for token in output.tokens {
        println!(
            "{}..{} {}",
            token.span.start,
            token.span.end,
            token.kind.describe()
        );
    }

    if output.diagnostics.iter().any(AvenDiagnostic::is_error) {
        bail!("tokenization failed");
    }

    Ok(())
}

fn layout(path: &Path) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let output = aven_parser::layout_source(&source);

    if !output.diagnostics.is_empty() {
        print_diagnostics(path, &source, &output.diagnostics)?;
    }

    for token in output.tokens {
        println!(
            "{}..{} {}",
            token.span.start,
            token.span.end,
            token.kind.describe()
        );
    }

    if output.diagnostics.iter().any(AvenDiagnostic::is_error) {
        bail!("layout failed");
    }

    Ok(())
}

fn fmt(path: &Path, check: bool) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let formatted = match aven_fmt::format_source(&source) {
        Ok(formatted) => formatted,
        Err(diagnostics) => {
            print_diagnostics(path, &source, &diagnostics)?;
            bail!("formatting failed");
        }
    };

    if source == formatted {
        return Ok(());
    }

    if check {
        bail!("{} is not formatted", path.display());
    }

    fs::write(path, formatted).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn print_diagnostics(path: &Path, source: &str, diagnostics: &[AvenDiagnostic]) -> Result<()> {
    let source_id = path.display().to_string();
    let use_color = io::stderr().is_terminal();

    for diagnostic in diagnostics {
        print_diagnostic(&source_id, source, diagnostic, use_color)
            .context("failed to print diagnostic")?;
    }

    Ok(())
}

fn print_diagnostic(
    source_id: &str,
    source: &str,
    diagnostic: &AvenDiagnostic,
    use_color: bool,
) -> std::io::Result<()> {
    debug_assert!(
        !diagnostic.labels.is_empty(),
        "diagnostic `{}` has no labels",
        diagnostic.code.as_deref().unwrap_or("unclassified")
    );

    let primary_span = diagnostic
        .labels
        .first()
        .map(|label| label.span)
        .unwrap_or_else(|| aven_core::Span::point(source.len()));

    let kind = match diagnostic.severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
        Severity::Note => ReportKind::Advice,
    };

    let mut builder = Report::build(kind, (source_id, span_range(source, primary_span)))
        .with_config(AriadneConfig::default().with_color(use_color))
        .with_message(diagnostic.message.clone());

    if let Some(code) = &diagnostic.code {
        builder = builder.with_code(code);
    }

    for label in &diagnostic.labels {
        builder = builder.with_label(
            AriadneLabel::new((source_id, span_range(source, label.span)))
                .with_message(label.message.clone()),
        );
    }

    for note in &diagnostic.notes {
        builder = builder.with_note(note);
    }

    builder.finish().eprint((source_id, Source::from(source)))
}

fn span_range(source: &str, span: aven_core::Span) -> Range<usize> {
    debug_assert!(
        span.start <= span.end,
        "invalid span: start {} is after end {}",
        span.start,
        span.end
    );
    debug_assert!(
        span.start <= source.len(),
        "invalid span: start {} is beyond source length {}",
        span.start,
        source.len()
    );
    debug_assert!(
        span.end <= source.len(),
        "invalid span: end {} is beyond source length {}",
        span.end,
        source.len()
    );

    let start = span.start.min(source.len());
    let end = span.end.min(source.len()).max(start);

    start..end
}
