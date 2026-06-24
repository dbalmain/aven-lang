use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ariadne::{Config as AriadneConfig, Label as AriadneLabel, Report, ReportKind, Source};
use aven_core::{Diagnostic as AvenDiagnostic, DiagnosticReport, FileId, Severity, SourceFile};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue, json};

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

    /// Run a file and print the last expression value.
    Run {
        /// Source file to run.
        path: PathBuf,

        /// Diagnostic output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },

    /// Explain a diagnostic code.
    Explain {
        /// Diagnostic code to explain.
        code: String,
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
        Command::Run { path, format } => run(&path, format),
        Command::Explain { code } => explain(&code),
        Command::Tokens { path } => tokens(&path),
        Command::Layout { path } => layout(&path),
        Command::Fmt { check, path } => fmt(&path, check),
        Command::Lsp => {
            aven_lsp::run_stdio().await;
            Ok(())
        }
    }
}

fn explain(code: &str) -> Result<()> {
    let Some(explanation) = aven_core::explain(code) else {
        bail!("no explanation found for diagnostic code `{code}`");
    };

    println!("{}", explanation.code);
    println!();
    println!("{}", explanation.text);
    Ok(())
}

fn check(path: &Path, format: OutputFormat, show_timings: bool) -> Result<()> {
    let file = load_source_file(path)?;
    let checked =
        aven_compiler::check_source_file_with_globals(file, &build_host()?.check_globals());
    let timings = checked.timings;
    let file = checked.document.file();

    let mut report = checked.document.diagnostic_report();
    report.sort_by_primary_span();

    match format {
        OutputFormat::Text => {
            if !report.is_empty() {
                print_diagnostics(file, &report)?;
            }
            if show_timings {
                print_timings(timings);
            }
        }
        OutputFormat::Json => {
            print_json_diagnostics(file, &report, show_timings.then_some(timings))?
        }
    }

    if report.has_errors() {
        bail!("check failed");
    }

    if format == OutputFormat::Text {
        println!(
            "{}: ok (parse, name, annotation, and partial monomorphic inference checks)",
            path.display()
        );
    }

    Ok(())
}

fn run(path: &Path, format: OutputFormat) -> Result<()> {
    let file = load_source_file(path)?;
    let parse = aven_parser::parse_source(&file);
    let mut diagnostics = parse.diagnostics.clone();
    let mut value = None;

    if !diagnostics.iter().any(AvenDiagnostic::is_error) {
        let outcome =
            aven_eval::eval_module_with_globals(&parse.module, build_host()?.eval_globals());
        value = outcome.value;
        diagnostics.extend(outcome.diagnostics);
    }

    let mut report = DiagnosticReport::new(file.id, diagnostics);
    report.sort_by_primary_span();

    match format {
        OutputFormat::Text => {
            if !report.is_empty() {
                print_diagnostics(&file, &report)?;
            }
        }
        OutputFormat::Json => print_json_diagnostics(&file, &report, None)?,
    }

    if report.has_errors() {
        bail!("run failed");
    }

    if let Some(value) = value.filter(|value| !value.is_unit()) {
        println!("{value}");
    }

    Ok(())
}

/// Build the host registry that feeds both `run` (values) and `check` (types).
///
/// The CLI owns the concrete IO (the `StdoutLogSink`, the root trace context, the
/// `Console.log`/`debug` natives); `aven-host` owns the registration/typing
/// vocabulary, so the logger type lives there and the CLI only calls `build::*`.
fn build_host() -> Result<aven_host::Host> {
    let mut host = aven_host::Host::new();

    // Share one logger value between the `logger` global and `Platform.Log` so
    // they emit on the same sink and trace context.
    let log_sink = Rc::new(StdoutLogSink);
    let log = aven_eval::logging::logger(log_sink, root_trace_context()?);
    host.register("logger".to_owned(), log.clone(), aven_host::logger_type());

    use aven_host::build;
    // Closed record: `Platform.Console.log` and `Platform.Log` are both precisely
    // typed, so the platform boundary type-checks end to end.
    let platform_type = build::record(vec![
        (
            "Console",
            build::record(vec![(
                "log",
                build::function(vec![build::text()], build::unit()),
            )]),
        ),
        ("Log", aven_host::logger_type()),
    ]);
    host.register("Platform".to_owned(), default_platform(log), platform_type);

    // TODO(P2): `debug : (a) -> a` is generic; its type isn't expressible until
    // scheme support / the typed-fn adapter lands, so it runs untyped for now.
    host.register_runtime_only("debug".to_owned(), debug_native());

    Ok(host)
}

/// Writes each argument's `Display` to stderr (space-separated, newline-terminated)
/// and returns its single argument unchanged, so `debug(x)` is usable inline. This
/// keeps stdout clean for the program's value and log output. The IO effect lives in
/// the host, so the native is injected by the CLI prelude rather than `aven-eval`.
fn debug_native() -> aven_eval::Value {
    aven_eval::Value::native(|args| {
        let mut stderr = io::stderr().lock();
        for (index, value) in args.iter().enumerate() {
            if index > 0 {
                write!(stderr, " ").map_err(|error| error.to_string())?;
            }
            write!(stderr, "{value}").map_err(|error| error.to_string())?;
        }
        writeln!(stderr).map_err(|error| error.to_string())?;

        Ok(match args {
            [single] => single.clone(),
            _ => aven_eval::Value::unit(),
        })
    })
}

fn default_platform(log: aven_eval::Value) -> aven_eval::Value {
    aven_eval::Value::record(vec![
        (
            "Console".to_owned(),
            aven_eval::Value::record(vec![(
                "log".to_owned(),
                aven_eval::Value::native(|args| {
                    let mut stdout = io::stdout().lock();
                    for (index, value) in args.iter().enumerate() {
                        if index > 0 {
                            write!(stdout, " ").map_err(|error| error.to_string())?;
                        }
                        write!(stdout, "{value}").map_err(|error| error.to_string())?;
                    }
                    writeln!(stdout).map_err(|error| error.to_string())?;
                    Ok(aven_eval::Value::unit())
                }),
            )]),
        ),
        ("Log".to_owned(), log),
    ])
}

struct StdoutLogSink;

impl aven_eval::logging::LogSink for StdoutLogSink {
    fn emit(&self, record: &aven_eval::logging::LogRecord<'_>) {
        let output = log_record_json(record);
        let mut stdout = io::stdout().lock();
        if let Err(error) = serde_json::to_writer(&mut stdout, &output) {
            eprintln!("failed to serialize log record: {error}");
            return;
        }
        if let Err(error) = writeln!(stdout) {
            eprintln!("failed to write log record: {error}");
        }
    }
}

fn log_record_json(record: &aven_eval::logging::LogRecord<'_>) -> JsonValue {
    let mut output = JsonMap::new();
    output.insert(
        "level".to_owned(),
        JsonValue::String(record.level.as_str().to_owned()),
    );
    output.insert(
        "severity".to_owned(),
        JsonValue::Number(JsonNumber::from(record.level.severity_number())),
    );
    output.insert(
        "time".to_owned(),
        JsonValue::Number(JsonNumber::from(unix_time_ms())),
    );
    output.insert("msg".to_owned(), JsonValue::String(record.message.clone()));
    output.insert(
        "traceId".to_owned(),
        JsonValue::String(record.trace.trace_id.clone()),
    );
    output.insert(
        "spanId".to_owned(),
        JsonValue::String(record.trace.span_id.clone()),
    );
    output.insert(
        "traceFlags".to_owned(),
        JsonValue::String(record.trace.trace_flags.clone()),
    );
    output.insert(
        "traceState".to_owned(),
        JsonValue::String(record.trace.trace_state.clone()),
    );

    for (name, value) in record.attributes {
        output.insert(name.clone(), aven_value_json(value));
    }

    JsonValue::Object(output)
}

fn aven_value_json(value: &aven_eval::Value) -> JsonValue {
    match value {
        aven_eval::Value::Int(value) => JsonValue::Number(JsonNumber::from(*value)),
        aven_eval::Value::Float(value) => JsonNumber::from_f64(*value)
            .map(JsonValue::Number)
            .unwrap_or_else(|| JsonValue::String(value.to_string())),
        aven_eval::Value::Text(value) => JsonValue::String(value.clone()),
        aven_eval::Value::Bool(value) => JsonValue::Bool(*value),
        aven_eval::Value::Array(values)
        | aven_eval::Value::Tuple(values)
        | aven_eval::Value::Set(values) => {
            JsonValue::Array(values.iter().map(aven_value_json).collect())
        }
        aven_eval::Value::Record(fields) => {
            let mut output = JsonMap::new();
            for (name, value) in fields.iter() {
                output.insert(name.clone(), aven_value_json(value));
            }
            JsonValue::Object(output)
        }
        aven_eval::Value::Tag { name, payload } => json!({
            "tag": name,
            "payload": payload.iter().map(aven_value_json).collect::<Vec<_>>(),
        }),
        aven_eval::Value::Closure(_) => JsonValue::String("<function>".to_owned()),
        aven_eval::Value::Native(_) => JsonValue::String("<native>".to_owned()),
        aven_eval::Value::Type(name) => JsonValue::String(name.clone()),
        aven_eval::Value::Undefined | aven_eval::Value::Null => JsonValue::Null,
    }
}

fn unix_time_ms() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn root_trace_context() -> Result<aven_eval::logging::TraceContext> {
    Ok(aven_eval::logging::TraceContext {
        trace_id: random_hex_id::<16>().context("failed to generate W3C trace id")?,
        span_id: random_hex_id::<8>().context("failed to generate W3C span id")?,
        trace_flags: "01".to_owned(),
        trace_state: String::new(),
    })
}

fn random_hex_id<const N: usize>() -> io::Result<String> {
    loop {
        let mut bytes = [0u8; N];
        fill_random(&mut bytes)?;
        if bytes.iter().any(|byte| *byte != 0) {
            return Ok(hex_encode(&bytes));
        }
    }
}

fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    // The CLI host owns randomness. Reading OS randomness directly keeps aven-eval
    // effect-free without adding a dependency for this small host-side need.
    fs::File::open("/dev/urandom")?.read_exact(bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn print_timings(timings: aven_compiler::PhaseTimings) {
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
    timings: Option<aven_compiler::PhaseTimings>,
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

fn timings_json(timings: aven_compiler::PhaseTimings) -> serde_json::Value {
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
