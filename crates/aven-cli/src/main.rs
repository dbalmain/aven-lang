use std::fs;
use std::io::{self, IsTerminal};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ariadne::{Config as AriadneConfig, Label as AriadneLabel, Report, ReportKind, Source};
use aven_core::{Diagnostic as AvenDiagnostic, DiagnosticReport, FileId, Severity, SourceFile};
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

        /// Print parse/name/check timings.
        #[arg(long)]
        timings: bool,
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
        Command::Check {
            path,
            format,
            timings,
        } => check(&path, format, timings),
        Command::Tokens { path } => tokens(&path),
        Command::Layout { path } => layout(&path),
        Command::Fmt { check, path } => fmt(&path, check),
        Command::Lsp => {
            aven_lsp::run_stdio().await;
            Ok(())
        }
    }
}

fn check(path: &Path, format: OutputFormat, show_timings: bool) -> Result<()> {
    let file = load_source_file(path)?;
    let total_start = Instant::now();

    let (output, parse_duration) = timed(|| aven_parser::parse_source(&file));

    let mut diagnostics = output.diagnostics.clone();
    let mut name_duration = None;
    let mut check_duration = None;

    if !diagnostics.iter().any(AvenDiagnostic::is_error) {
        // Name analysis intentionally waits for a clean parse in the first pass.
        // Analyzing recovered `Missing` trees is a later diagnostics-recovery task.
        let (name_analysis, duration) = timed(|| aven_parser::analyze_names(&output.module));
        name_duration = Some(duration);

        let (check_output, duration) = timed(|| aven_check::check_module(&output.module));
        check_duration = Some(duration);

        diagnostics.extend(name_analysis.diagnostics);
        diagnostics.extend(check_output.diagnostics);
    }

    let timings = CheckTimings {
        parse: parse_duration,
        name: name_duration,
        check: check_duration,
        total: total_start.elapsed(),
    };

    let mut report = DiagnosticReport::new(output.file_id, diagnostics);
    report.sort_by_primary_span();

    match format {
        OutputFormat::Text => {
            if !report.is_empty() {
                print_diagnostics(&file, &report)?;
            }
            if show_timings {
                print_timings(timings);
            }
        }
        OutputFormat::Json => {
            print_json_diagnostics(&file, &report, show_timings.then_some(timings))?
        }
    }

    if report.has_errors() {
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

#[derive(Debug, Clone, Copy)]
struct CheckTimings {
    parse: Duration,
    name: Option<Duration>,
    check: Option<Duration>,
    total: Duration,
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = f();
    (value, start.elapsed())
}

fn print_timings(timings: CheckTimings) {
    eprintln!("timings:");
    eprintln!("  parse: {:.3} ms", duration_ms(timings.parse));
    print_timing_line("name", timings.name);
    print_timing_line("check", timings.check);
    eprintln!("  total: {:.3} ms", duration_ms(timings.total));
}

fn print_timing_line(name: &str, duration: Option<Duration>) {
    match duration {
        Some(duration) => eprintln!("  {name}: {:.3} ms", duration_ms(duration)),
        None => eprintln!("  {name}: skipped"),
    }
}

fn print_json_diagnostics(
    file: &SourceFile,
    report: &DiagnosticReport,
    timings: Option<CheckTimings>,
) -> Result<()> {
    debug_assert_eq!(file.id, report.file_id);

    let mut output = json!({
        "fileId": report.file_id.0,
        "path": file.path.as_ref().map(|path| path.display().to_string()),
        "name": file.name.as_str(),
        "ok": !report.has_errors(),
        "diagnostics": report.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    });

    if let Some(timings) = timings {
        output["timingsMs"] = timings_json(timings);
    }

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn timings_json(timings: CheckTimings) -> serde_json::Value {
    json!({
        "parse": duration_ms(timings.parse),
        "name": timings.name.map(duration_ms),
        "check": timings.check.map(duration_ms),
        "total": duration_ms(timings.total),
    })
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
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
    let file = load_source_file(path)?;
    let output = aven_parser::lex_source(file.source());
    let report = DiagnosticReport::new(file.id, output.diagnostics.clone());

    if !report.is_empty() {
        print_diagnostics(&file, &report)?;
    }

    for token in output.tokens {
        println!(
            "{}..{} {}",
            token.span.start,
            token.span.end,
            token.kind.describe()
        );
    }

    if report.has_errors() {
        bail!("tokenization failed");
    }

    Ok(())
}

fn layout(path: &Path) -> Result<()> {
    let file = load_source_file(path)?;
    let output = aven_parser::layout_source(file.source());
    let report = DiagnosticReport::new(file.id, output.diagnostics.clone());

    if !report.is_empty() {
        print_diagnostics(&file, &report)?;
    }

    for token in output.tokens {
        println!(
            "{}..{} {}",
            token.span.start,
            token.span.end,
            token.kind.describe()
        );
    }

    if report.has_errors() {
        bail!("layout failed");
    }

    Ok(())
}

fn fmt(path: &Path, check: bool) -> Result<()> {
    let file = load_source_file(path)?;
    let formatted = match aven_fmt::format_source(file.source()) {
        Ok(formatted) => formatted,
        Err(diagnostics) => {
            let report = DiagnosticReport::new(file.id, diagnostics);
            print_diagnostics(&file, &report)?;
            bail!("formatting failed");
        }
    };

    if file.source() == formatted {
        return Ok(());
    }

    if check {
        bail!("{} is not formatted", path.display());
    }

    fs::write(path, formatted).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_source_file(path: &Path) -> Result<SourceFile> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    Ok(SourceFile::new(
        FileId(0),
        path.display().to_string(),
        Some(path.to_path_buf()),
        source,
    ))
}

fn print_diagnostics(file: &SourceFile, report: &DiagnosticReport) -> Result<()> {
    debug_assert_eq!(file.id, report.file_id);

    let source_id = file.name.clone();
    let use_color = io::stderr().is_terminal();

    for diagnostic in &report.diagnostics {
        print_diagnostic(&source_id, file.source(), diagnostic, use_color)
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
