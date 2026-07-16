use std::cell::RefCell;
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

        /// Logger sink target: stdout, stderr, syslog, journald, or a file path.
        #[arg(long, default_value = "stdout")]
        log: String,

        /// Logger record rendering format.
        #[arg(long = "log-format", value_enum, default_value_t = LogFormat::Json)]
        log_format: LogFormat,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunConfig {
    log: String,
    log_format: LogFormat,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            log: "stdout".to_owned(),
            log_format: LogFormat::Json,
        }
    }
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
        Command::Run {
            path,
            format,
            log,
            log_format,
        } => run(&path, format, &RunConfig { log, log_format }),
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
    let roots = discover_roots(path);
    let checked = aven_compiler::check_path_with_host_globals_and_roots(
        path,
        &aven_host::standard_check_host_globals(),
        &roots,
    )
    .with_context(|| format!("failed to load {}", path.display()))?;
    let timings = checked.timings;
    let has_errors = reports_have_errors(&checked.reports);

    match format {
        OutputFormat::Text => {
            if !checked.reports.is_empty() {
                print_diagnostic_reports(&checked.source_map, &checked.reports)?;
            }
            if show_timings {
                print_timings(timings);
            }
        }
        OutputFormat::Json => print_json_diagnostic_reports(
            &checked.source_map,
            &checked.reports,
            show_timings.then_some(timings),
        )?,
    }

    if has_errors {
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

fn run(path: &Path, format: OutputFormat, config: &RunConfig) -> Result<()> {
    let host = build_host(config)?;
    let roots = discover_roots_for_host(path, &host);
    let output = aven_compiler::eval_path_with_host_globals_and_roots(
        path,
        &host.check_host_globals(),
        host.eval_globals(),
        &roots,
    )
    .with_context(|| format!("failed to load {}", path.display()))?;
    let has_errors = reports_have_errors(&output.reports);

    match format {
        OutputFormat::Text => {
            if !output.reports.is_empty() {
                print_diagnostic_reports(&output.source_map, &output.reports)?;
            }
        }
        OutputFormat::Json => {
            print_json_diagnostic_reports(&output.source_map, &output.reports, None)?
        }
    }

    if has_errors {
        bail!("run failed");
    }

    if let Some(value) = output.value.filter(|value| !is_trivial_value(value)) {
        if is_err_value(&value) {
            eprintln!("{value}");
            std::process::exit(1);
        }
        println!("{value}");
    }

    Ok(())
}

/// Module roots for `check`/`run`: filesystem discovery plus the embedded
/// standard library, so bare `import("std")`/`import("std/time")` resolve.
fn discover_roots(path: &Path) -> aven_compiler::ModuleRoots {
    aven_compiler::ModuleRoots::discover(path)
        .with_library(
            aven_host::STD_LIBRARY_NAME,
            aven_host::standard_std_library(),
        )
        .with_library_only_global_names(aven_host::standard_library_only_global_names())
}

fn discover_roots_for_host(path: &Path, host: &aven_host::Host) -> aven_compiler::ModuleRoots {
    host.disabled_capability_modules().into_iter().fold(
        aven_compiler::ModuleRoots::discover(path)
            .with_library(aven_host::STD_LIBRARY_NAME, host.std_library())
            .with_library_only_global_names(host.library_only_global_names()),
        |roots, (specifier, capability, register_method)| {
            roots.with_disabled_capability_module(specifier, capability, register_method)
        },
    )
}

fn is_err_value(value: &aven_eval::Value) -> bool {
    matches!(value, aven_eval::Value::Tag { name, .. } if name == "Err")
}

/// Whether a final value carries no information worth printing: `Unit` or the
/// empty record `{}` (the trivial value the bare IO functions return). Keeps
/// stdout clean for effect-terminated scripts like `writeLine("hi")`.
fn is_trivial_value(value: &aven_eval::Value) -> bool {
    value.is_unit() || matches!(value, aven_eval::Value::Record(fields) if fields.is_empty())
}

/// Build the host registry that feeds both `run` (values) and `check` (types).
///
/// The CLI owns the concrete IO (the selected log sink, the root trace context,
/// and the bare IO/`dbg` natives); `aven-host` owns the registration/typing
/// vocabulary for the standard host types.
fn build_host(config: &RunConfig) -> Result<aven_host::Host> {
    let mut host = aven_host::Host::new();

    host.register_logger(config.log_sink()?, root_trace_context()?);
    host.register("dbg", dbg_native(), aven_host::dbg_type());

    // Platform IO (bare + handle + files) lives in `aven-host`. `dbg` and the
    // logger stay CLI-owned: process-level config (log sink destination/format,
    // stderr-only debug printing).
    host.register_bare_io();
    host.register_std_streams();
    host.register_files();
    host.register_http();
    host.register_json();
    host.register_yaml();
    host.register_toml();
    host.register_temporals();
    host.register_clock();
    host.register_zones();

    Ok(host)
}

/// Writes each argument's `Display` to stderr (space-separated, newline-terminated)
/// and returns its single argument unchanged, so `dbg(x)` is usable inline. This
/// keeps stdout clean for the program's value and log output. The IO effect lives in
/// the host, so the native is injected by the CLI prelude rather than `aven-eval`.
fn dbg_native() -> aven_eval::Value {
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

enum LogDestination {
    Stdout,
    Stderr,
    File(RefCell<fs::File>),
}

struct ConfiguredLogSink {
    destination: LogDestination,
    format: LogFormat,
}

impl RunConfig {
    fn log_sink(&self) -> Result<Rc<dyn aven_eval::logging::LogSink>> {
        let destination = match self.log.as_str() {
            "stdout" => LogDestination::Stdout,
            "stderr" => LogDestination::Stderr,
            "syslog" => bail!("--log syslog is not yet implemented"),
            "journald" => bail!("--log journald is not yet implemented"),
            path => LogDestination::File(RefCell::new(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("failed to open log file {path}"))?,
            )),
        };

        Ok(Rc::new(ConfiguredLogSink {
            destination,
            format: self.log_format,
        }))
    }
}

impl aven_eval::logging::LogSink for ConfiguredLogSink {
    fn emit(&self, record: &aven_eval::logging::LogRecord<'_>) {
        let result = match &self.destination {
            LogDestination::Stdout => {
                let mut stdout = io::stdout().lock();
                write_log_record(&mut stdout, self.format, record)
            }
            LogDestination::Stderr => {
                let mut stderr = io::stderr().lock();
                write_log_record(&mut stderr, self.format, record)
            }
            LogDestination::File(file) => {
                let mut file = file.borrow_mut();
                write_log_record(&mut *file, self.format, record)
            }
        };

        if let Err(error) = result {
            eprintln!("{error}");
        }
    }
}

fn write_log_record(
    writer: &mut dyn Write,
    format: LogFormat,
    record: &aven_eval::logging::LogRecord<'_>,
) -> std::result::Result<(), String> {
    match format {
        LogFormat::Json => write_json_log_record(writer, record),
        LogFormat::Text => write_text_log_record(writer, record),
    }
}

fn write_json_log_record(
    writer: &mut dyn Write,
    record: &aven_eval::logging::LogRecord<'_>,
) -> std::result::Result<(), String> {
    serde_json::to_writer(&mut *writer, &log_record_json(record))
        .map_err(|error| format!("failed to serialize log record: {error}"))?;
    writeln!(writer).map_err(|error| format!("failed to write log record: {error}"))
}

fn write_text_log_record(
    writer: &mut dyn Write,
    record: &aven_eval::logging::LogRecord<'_>,
) -> std::result::Result<(), String> {
    write!(
        writer,
        "{} {}",
        record.level.as_str().to_ascii_uppercase(),
        record.message
    )
    .map_err(|error| format!("failed to write log record: {error}"))?;
    for (name, value) in record.attributes {
        write!(writer, " {name}={value}")
            .map_err(|error| format!("failed to write log record: {error}"))?;
    }
    writeln!(writer).map_err(|error| format!("failed to write log record: {error}"))
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
        aven_eval::Value::Map(entries) => JsonValue::Array(
            entries
                .iter()
                .map(|(key, value)| {
                    JsonValue::Array(vec![aven_value_json(key), aven_value_json(value)])
                })
                .collect(),
        ),
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
        aven_eval::Value::ResultMethod { .. } => JsonValue::String("<method>".to_owned()),
        aven_eval::Value::Closure(_) => JsonValue::String("<function>".to_owned()),
        aven_eval::Value::Native(_) => JsonValue::String("<native>".to_owned()),
        aven_eval::Value::Type(ty) => JsonValue::String(ty.to_string()),
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

fn print_json_diagnostic_reports(
    source_map: &aven_core::SourceMap,
    reports: &[DiagnosticReport],
    timings: Option<aven_compiler::PhaseTimings>,
) -> Result<()> {
    if let [file] = source_map.files() {
        let report = reports
            .iter()
            .find(|report| report.file_id == file.id)
            .cloned()
            .unwrap_or_else(|| DiagnosticReport::new(file.id, Vec::new()));
        let mut output = json_report(file, &report);
        output["ok"] = json!(!report.has_errors());
        if let Some(timings) = timings {
            output["timingsMs"] = timings_json(timings);
        }

        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    let mut output = json!({
        "ok": !reports_have_errors(reports),
        "files": reports.iter().filter_map(|report| {
            let file = source_map.get(report.file_id)?;
            Some(json_report(file, report))
        }).collect::<Vec<_>>(),
    });

    if let Some(timings) = timings {
        output["timingsMs"] = timings_json(timings);
    }

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn json_report(file: &SourceFile, report: &DiagnosticReport) -> serde_json::Value {
    debug_assert_eq!(file.id, report.file_id);

    json!({
        "fileId": report.file_id.0,
        "path": file.path.as_ref().map(|path| path.display().to_string()),
        "name": file.name.as_str(),
        "diagnostics": report.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    })
}

fn reports_have_errors(reports: &[DiagnosticReport]) -> bool {
    reports.iter().any(DiagnosticReport::has_errors)
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

fn print_diagnostic_reports(
    source_map: &aven_core::SourceMap,
    reports: &[DiagnosticReport],
) -> Result<()> {
    for report in reports {
        let Some(file) = source_map.get(report.file_id) else {
            continue;
        };
        print_diagnostics(file, report)?;
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

    let start = byte_offset_to_char_offset(source, span.start.min(source.len()));
    let end = byte_offset_to_char_offset(source, span.end.min(source.len())).max(start);

    start..end
}

fn byte_offset_to_char_offset(source: &str, byte_offset: usize) -> usize {
    source
        .char_indices()
        .take_while(|(offset, _)| *offset < byte_offset)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_host_check_globals_match_standard_host_types() -> Result<()> {
        let host = build_host(&RunConfig::default())?;

        assert_eq!(
            host.check_globals()
                .into_iter()
                .filter(|(name, _)| !matches!(name.as_str(), "now" | "zone"))
                .collect::<Vec<_>>(),
            aven_host::standard_check_globals()
        );
        assert_eq!(
            host.check_host_globals().types,
            aven_host::standard_check_host_globals().types
        );
        assert_eq!(
            host.check_host_globals().type_definitions,
            aven_host::standard_check_host_globals().type_definitions
        );
        assert_eq!(
            host.check_host_globals().statics,
            aven_host::standard_check_host_globals().statics
        );
        Ok(())
    }

    #[test]
    fn diagnostic_ranges_translate_byte_spans_to_character_offsets() {
        let source = "# em — dash\nTree = Tree\n";
        let tree_start = source.find("Tree").expect("source contains Tree");

        assert_eq!(
            span_range(
                source,
                aven_core::Span::new(tree_start, tree_start + "Tree".len())
            ),
            12..16
        );
    }
}
