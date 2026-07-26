use std::cell::RefCell;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ariadne::{Config as AriadneConfig, Label as AriadneLabel, Report, ReportKind, Source};
use aven_core::{
    Diagnostic as AvenDiagnostic, DiagnosticReport, FileId, SessionRecord, SessionRecordParts,
    SessionSummary, SessionTimings, Severity, SourceFile,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue, json};

/// Accumulates one session-log record for the current invocation.
///
/// Emitted exactly once via [`SessionCapture::emit`] from the single exit funnel
/// in `main` — never from a `Drop` impl, because several paths used to call
/// `std::process::exit` (and any future hard-exit would still skip destructors).
#[derive(Debug, Default)]
struct SessionCapture {
    subcommand: String,
    argv: Vec<String>,
    entry_path: Option<String>,
    entry_source: Option<String>,
    timings: Option<SessionTimings>,
    diagnostics: Vec<AvenDiagnostic>,
    summary: Option<SessionSummary>,
}

impl SessionCapture {
    fn new(subcommand: impl Into<String>, argv: Vec<String>) -> Self {
        Self {
            subcommand: subcommand.into(),
            argv,
            ..Self::default()
        }
    }

    fn set_entry_path(&mut self, path: &Path) {
        self.entry_path = Some(path.display().to_string());
    }

    fn set_entry_source(&mut self, source: &str) {
        self.entry_source = Some(source.to_owned());
    }

    fn set_timings(&mut self, timings: aven_compiler::PhaseTimings) {
        self.timings = Some(SessionTimings {
            parse: duration_ms(timings.parse),
            name: timings.name.map(duration_ms),
            check: timings.check.map(duration_ms),
            total: duration_ms(timings.total),
        });
    }

    fn set_diagnostics(&mut self, diagnostics: Vec<AvenDiagnostic>) {
        self.diagnostics = diagnostics;
    }

    fn set_diagnostics_from_reports(&mut self, reports: &[DiagnosticReport]) {
        self.diagnostics = reports
            .iter()
            .flat_map(|report| report.diagnostics.iter().cloned())
            .collect();
    }

    fn set_test_summary(&mut self, total: usize, passed: usize, failed: usize, errored: usize) {
        self.summary = Some(SessionSummary::Test {
            total,
            passed,
            failed,
            errored,
        });
    }

    fn emit(self, exit_code: i32) {
        let record = SessionRecord::from_parts(SessionRecordParts {
            tag: aven_core::session_tag_from_env(),
            aven_version: env!("CARGO_PKG_VERSION").to_owned(),
            aven_build_commit: option_env!("AVEN_BUILD_COMMIT").map(str::to_owned),
            subcommand: self.subcommand,
            argv: self.argv,
            entry_path: self.entry_path,
            entry_source: self.entry_source,
            timings: self.timings,
            diagnostics: self.diagnostics,
            exit_code,
            summary: self.summary,
        });
        aven_core::append_session_record_if_enabled(&record);
    }
}

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

        /// Declare a root custom operator as TOKEN:ANCHOR:ASSOCIATIVITY.
        #[arg(long = "operator")]
        operators: Vec<String>,
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

        /// Declare a root custom operator as TOKEN:ANCHOR:ASSOCIATIVITY.
        #[arg(long = "operator")]
        operators: Vec<String>,
    },

    /// Run a test suite module (record of zero-arg Result thunks).
    Test {
        /// Source file whose entry value is the suite record.
        path: PathBuf,

        /// Diagnostic / suite report output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,

        /// Logger sink target: stdout, stderr, syslog, journald, or a file path.
        #[arg(long, default_value = "stdout")]
        log: String,

        /// Logger record rendering format.
        #[arg(long = "log-format", value_enum, default_value_t = LogFormat::Json)]
        log_format: LogFormat,

        /// Declare a root custom operator as TOKEN:ANCHOR:ASSOCIATIVITY.
        #[arg(long = "operator")]
        operators: Vec<String>,
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

        /// Declare a root custom operator as TOKEN:ANCHOR:ASSOCIATIVITY.
        #[arg(long = "operator")]
        operators: Vec<String>,
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

#[derive(Debug, PartialEq, Eq)]
struct NormalizedArgv {
    args: Vec<OsString>,
    direct_shebang_arguments: Option<Vec<String>>,
}

fn normalize_direct_shebang_argv(args: Vec<OsString>) -> Result<NormalizedArgv> {
    let Some(blob) = args.get(1).and_then(|argument| argument.to_str()) else {
        return Ok(NormalizedArgv {
            args,
            direct_shebang_arguments: None,
        });
    };
    if !blob.starts_with("run ") {
        return Ok(NormalizedArgv {
            args,
            direct_shebang_arguments: None,
        });
    }
    if blob.ends_with(' ')
        || blob
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() && byte != b' ')
    {
        bail!("malformed direct Aven shebang argument: use unquoted arguments separated by spaces");
    }

    let words = blob
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let operator_arguments = words[1..]
        .iter()
        .map(|word| (*word).to_owned())
        .collect::<Vec<_>>();
    if let Err(diagnostics) = aven_compiler::parse_argv_operator_fixities(&operator_arguments) {
        let messages = diagnostics
            .iter()
            .map(|diagnostic| {
                diagnostic.code.as_deref().map_or_else(
                    || diagnostic.message.clone(),
                    |code| format!("{code}: {}", diagnostic.message),
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!("malformed direct Aven shebang argument: {messages}");
    }

    let mut normalized = Vec::with_capacity(args.len());
    normalized.push(args[0].clone());
    normalized.push(OsString::from("run"));
    normalized.extend(args.into_iter().skip(2));
    Ok(NormalizedArgv {
        args: normalized,
        direct_shebang_arguments: Some(operator_arguments),
    })
}

#[tokio::main]
async fn main() {
    // Single exit funnel: every command returns an exit code (or Err → 1), then
    // we emit the session record once and call `process::exit`. This avoids a
    // Drop-based logger, which would silently miss the old `process::exit` paths.
    let exit_code = match run_cli().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:?}");
            1
        }
    };
    std::process::exit(exit_code);
}

async fn run_cli() -> Result<i32> {
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    let NormalizedArgv {
        args,
        direct_shebang_arguments,
    } = normalize_direct_shebang_argv(raw_args)?;
    let argv = args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let cli = Cli::parse_from(&args);
    let subcommand = command_name(&cli.command);
    let mut session = SessionCapture::new(subcommand, argv);

    let result = match cli.command {
        Command::Check {
            path,
            format,
            timings,
            operators,
        } => {
            session.set_entry_path(&path);
            check(&path, format, timings, &operators, &mut session)
        }
        Command::Run {
            path,
            format,
            log,
            log_format,
            operators,
        } => {
            session.set_entry_path(&path);
            run(
                &path,
                format,
                &RunConfig { log, log_format },
                &operators,
                direct_shebang_arguments.as_deref(),
                &mut session,
            )
        }
        Command::Test {
            path,
            format,
            log,
            log_format,
            operators,
        } => {
            session.set_entry_path(&path);
            test(
                &path,
                format,
                &RunConfig { log, log_format },
                &operators,
                direct_shebang_arguments.as_deref(),
                &mut session,
            )
        }
        Command::Explain { code } => explain(&code).map(|()| 0),
        Command::Tokens { path } => {
            session.set_entry_path(&path);
            tokens(&path, &mut session)
        }
        Command::Layout { path } => {
            session.set_entry_path(&path);
            layout(&path, &mut session)
        }
        Command::Fmt {
            check,
            path,
            operators,
        } => {
            session.set_entry_path(&path);
            fmt(&path, check, &operators, &mut session)
        }
        Command::Lsp => {
            aven_lsp::run_stdio().await;
            Ok(0)
        }
    };

    match result {
        Ok(code) => {
            session.emit(code);
            Ok(code)
        }
        Err(error) => {
            // Emit before propagating so load/bail failures still leave a record.
            session.emit(1);
            Err(error)
        }
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Check { .. } => "check",
        Command::Run { .. } => "run",
        Command::Test { .. } => "test",
        Command::Explain { .. } => "explain",
        Command::Tokens { .. } => "tokens",
        Command::Layout { .. } => "layout",
        Command::Fmt { .. } => "fmt",
        Command::Lsp => "lsp",
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

fn check(
    path: &Path,
    format: OutputFormat,
    show_timings: bool,
    operators: &[String],
    session: &mut SessionCapture,
) -> Result<i32> {
    let host = parse_only_host();
    let roots = discover_roots(path);
    let configured =
        load_path_operator_config(path, &roots, operators, &host, None, false, format)?;
    session.set_entry_source(configured.file.source());
    let checked =
        aven_compiler::check_path_with_host_globals_and_entry_source_and_fixities_with_roots(
            path,
            &aven_host::standard_check_host_globals(),
            configured.file.source(),
            &configured.operator_fixities,
            &roots,
        )
        .with_context(|| format!("failed to load {}", path.display()))?;
    let timings = checked.timings;
    session.set_timings(timings);
    session.set_diagnostics_from_reports(&checked.reports);
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

    Ok(0)
}

fn run(
    path: &Path,
    format: OutputFormat,
    config: &RunConfig,
    operators: &[String],
    direct_shebang_arguments: Option<&[String]>,
    session: &mut SessionCapture,
) -> Result<i32> {
    let output = eval_entry_module(
        path,
        format,
        config,
        operators,
        direct_shebang_arguments,
        session,
    )?;
    session.set_diagnostics_from_reports(&output.reports);
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
        // Final-value printing uses the same rendering as interpolation: the
        // toText protocol with the `repr` fallback. It fails only when a
        // user `toText` override itself fails.
        let rendered = match aven_eval::display_text(&value) {
            Ok(rendered) => rendered,
            Err(diagnostics) => {
                session.set_diagnostics(diagnostics.clone());
                for diagnostic in diagnostics {
                    eprintln!("{}", diagnostic.message);
                }
                bail!("run failed");
            }
        };
        if is_err_value(&value) {
            eprintln!("{rendered}");
            return Ok(1);
        }
        println!("{rendered}");
    }

    Ok(0)
}

/// Exit codes for `aven test`: harnesses depend on these being distinct.
const TEST_EXIT_OK: i32 = 0;
const TEST_EXIT_CASES_FAILED: i32 = 1;
const TEST_EXIT_SUITE_UNRUNNABLE: i32 = 2;

fn test(
    path: &Path,
    format: OutputFormat,
    config: &RunConfig,
    operators: &[String],
    direct_shebang_arguments: Option<&[String]>,
    session: &mut SessionCapture,
) -> Result<i32> {
    let output = eval_entry_module(
        path,
        format,
        config,
        operators,
        direct_shebang_arguments,
        session,
    )?;
    if reports_have_errors(&output.reports) {
        session.set_diagnostics_from_reports(&output.reports);
        match format {
            OutputFormat::Text => {
                print_diagnostic_reports(&output.source_map, &output.reports)?;
            }
            OutputFormat::Json => {
                print_json_diagnostic_reports(&output.source_map, &output.reports, None)?;
            }
        }
        return Ok(TEST_EXIT_SUITE_UNRUNNABLE);
    }

    let Some(suite_value) = output.value else {
        let diagnostic = AvenDiagnostic::error(
            "test suite module produced no entry value; export a record of zero-arg thunks",
        )
        .with_code(aven_core::codes::test::NOT_A_RECORD)
        .with_note(
            "write a record literal as the last expression, e.g. `{ \"case\": () => test.pass }`",
        );
        return emit_suite_unrunnable(path, format, &output.source_map, vec![diagnostic], session);
    };

    let aven_eval::Value::Record(fields) = suite_value else {
        let diagnostic = AvenDiagnostic::error(format!(
            "test suite entry value is {}, expected a record of zero-arg thunks",
            suite_value.type_name()
        ))
        .with_code(aven_core::codes::test::NOT_A_RECORD)
        .with_note(
            "export a record whose fields are `() => Result({}, Text)` thunks as the module value",
        );
        return emit_suite_unrunnable(path, format, &output.source_map, vec![diagnostic], session);
    };

    // Field order follows the record's evaluation order (source order for
    // ordinary record literals). The harness may treat case order as meaningful.
    let mut suite_diagnostics = Vec::new();
    for (name, field) in fields.iter() {
        if !aven_eval::is_callable(field) {
            suite_diagnostics.push(
                AvenDiagnostic::error(format!(
                    "test case `{name}` is not callable (got {})",
                    field.type_name()
                ))
                .with_code(aven_core::codes::test::NOT_CALLABLE)
                .with_note(
                    "each suite field must be a zero-parameter thunk, e.g. `() => test.expectEq(a, b)`",
                ),
            );
            continue;
        }
        if let Some((required, _)) = aven_eval::callable_arity(field)
            && required != 0
        {
            suite_diagnostics.push(
                AvenDiagnostic::error(format!(
                    "test case `{name}` requires {required} argument(s); cases must be zero-arg thunks"
                ))
                .with_code(aven_core::codes::test::NON_ZERO_ARITY)
                .with_note(
                    "write `() => ...` so the runner can call the case without supplying arguments",
                ),
            );
        }
    }
    if !suite_diagnostics.is_empty() {
        return emit_suite_unrunnable(path, format, &output.source_map, suite_diagnostics, session);
    }

    let suite_started = std::time::Instant::now();
    let mut cases = Vec::with_capacity(fields.len());
    for (name, field) in fields.iter() {
        let case_started = std::time::Instant::now();
        let outcome = run_test_case(field);
        cases.push(TestCaseResult {
            name: name.clone(),
            outcome,
            duration_ms: duration_ms(case_started.elapsed()),
        });
    }
    let report = TestSuiteReport {
        path: path.display().to_string(),
        cases,
        duration_ms: duration_ms(suite_started.elapsed()),
    };

    session.set_test_summary(
        report.total(),
        report.passed(),
        report.failed(),
        report.errored(),
    );

    match format {
        OutputFormat::Text => print_test_suite_text(&report)?,
        OutputFormat::Json => print_test_suite_json(&report)?,
    }

    if report.failed() + report.errored() > 0 {
        return Ok(TEST_EXIT_CASES_FAILED);
    }
    debug_assert_eq!(TEST_EXIT_OK, 0);
    Ok(TEST_EXIT_OK)
}

/// Shared host/root/operator/eval path for `run` and `test`.
fn eval_entry_module(
    path: &Path,
    format: OutputFormat,
    config: &RunConfig,
    operators: &[String],
    direct_shebang_arguments: Option<&[String]>,
    session: &mut SessionCapture,
) -> Result<aven_compiler::ModuleEvalOutput> {
    let host = build_host(config)?;
    let roots = discover_roots_for_host(path, &host);
    let configured = load_path_operator_config(
        path,
        &roots,
        operators,
        &host,
        direct_shebang_arguments,
        direct_shebang_arguments.is_none(),
        format,
    )?;
    session.set_entry_source(configured.file.source());
    aven_compiler::eval_path_with_host_globals_and_entry_source_and_fixities_with_roots(
        path,
        &host.check_host_globals(),
        host.eval_globals(),
        configured.file.source(),
        &configured.operator_fixities,
        &roots,
    )
    .with_context(|| format!("failed to load {}", path.display()))
}

#[derive(Debug)]
struct TestSuiteReport {
    path: String,
    cases: Vec<TestCaseResult>,
    duration_ms: f64,
}

impl TestSuiteReport {
    fn total(&self) -> usize {
        self.cases.len()
    }

    fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, TestCaseOutcome::Pass))
            .count()
    }

    fn failed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, TestCaseOutcome::Fail { .. }))
            .count()
    }

    fn errored(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, TestCaseOutcome::Error { .. }))
            .count()
    }

    fn ok(&self) -> bool {
        self.failed() + self.errored() == 0
    }
}

#[derive(Debug)]
struct TestCaseResult {
    name: String,
    outcome: TestCaseOutcome,
    duration_ms: f64,
}

#[derive(Debug)]
enum TestCaseOutcome {
    Pass,
    Fail { message: String },
    Error { diagnostics: Vec<AvenDiagnostic> },
}

fn run_test_case(thunk: &aven_eval::Value) -> TestCaseOutcome {
    match aven_eval::call_value(thunk, Vec::new()) {
        Ok(value) => match result_case_outcome(&value) {
            Some(outcome) => outcome,
            None => TestCaseOutcome::Error {
                diagnostics: vec![AvenDiagnostic::error(format!(
                    "test case returned {}, expected Result(@Ok({{}}) | @Err(Text))",
                    value.type_name()
                ))
                .with_code(aven_core::codes::test::NOT_A_RESULT)
                .with_note(
                    "return `@Ok({})` on success or `@Err(message)` on failure (see `std/test`)",
                )],
            },
        },
        Err(diagnostics) => TestCaseOutcome::Error { diagnostics },
    }
}

/// Interpret a thunk return value as a test outcome. `None` means not a Result.
fn result_case_outcome(value: &aven_eval::Value) -> Option<TestCaseOutcome> {
    let aven_eval::Value::Tag { name, payload } = value else {
        return None;
    };
    match (name.as_str(), payload.as_slice()) {
        ("Ok", [_]) => Some(TestCaseOutcome::Pass),
        ("Err", [error]) => {
            let message = match aven_eval::display_text(error) {
                Ok(text) => text,
                Err(_) => error.to_string(),
            };
            Some(TestCaseOutcome::Fail { message })
        }
        ("Ok", _) | ("Err", _) => None,
        _ => None,
    }
}

fn emit_suite_unrunnable(
    path: &Path,
    format: OutputFormat,
    source_map: &aven_core::SourceMap,
    diagnostics: Vec<AvenDiagnostic>,
    session: &mut SessionCapture,
) -> Result<i32> {
    session.set_diagnostics(diagnostics.clone());
    let file_id = source_map
        .files()
        .iter()
        .find(|file| {
            file.path
                .as_ref()
                .is_some_and(|file_path| file_path == path)
        })
        .map(|file| file.id)
        .or_else(|| source_map.files().first().map(|file| file.id))
        .unwrap_or(FileId(0));
    let reports = vec![DiagnosticReport::new(file_id, diagnostics)];
    match format {
        OutputFormat::Text => print_diagnostic_reports(source_map, &reports)?,
        OutputFormat::Json => print_json_diagnostic_reports(source_map, &reports, None)?,
    }
    Ok(TEST_EXIT_SUITE_UNRUNNABLE)
}

fn print_test_suite_text(report: &TestSuiteReport) -> Result<()> {
    for case in &report.cases {
        match &case.outcome {
            TestCaseOutcome::Pass => println!("ok  - {}", case.name),
            TestCaseOutcome::Fail { message } => {
                println!("FAIL - {}: {message}", case.name);
            }
            TestCaseOutcome::Error { diagnostics } => {
                println!("ERROR - {}", case.name);
                for diagnostic in diagnostics {
                    if let Some(code) = &diagnostic.code {
                        println!("  [{code}] {}", diagnostic.message);
                    } else {
                        println!("  {}", diagnostic.message);
                    }
                }
            }
        }
    }
    println!(
        "{}: {} passed, {} failed, {} errored ({}/{}) in {:.3} ms",
        report.path,
        report.passed(),
        report.failed(),
        report.errored(),
        report.passed(),
        report.total(),
        report.duration_ms
    );
    Ok(())
}

fn print_test_suite_json(report: &TestSuiteReport) -> Result<()> {
    let cases = report
        .cases
        .iter()
        .map(|case| match &case.outcome {
            TestCaseOutcome::Pass => json!({
                "name": case.name,
                "outcome": "pass",
                "duration_ms": case.duration_ms,
            }),
            TestCaseOutcome::Fail { message } => json!({
                "name": case.name,
                "outcome": "fail",
                "message": message,
                "duration_ms": case.duration_ms,
            }),
            TestCaseOutcome::Error { diagnostics } => json!({
                "name": case.name,
                "outcome": "error",
                "diagnostics": diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
                "duration_ms": case.duration_ms,
            }),
        })
        .collect::<Vec<_>>();

    let output = json!({
        "ok": report.ok(),
        "path": report.path,
        "total": report.total(),
        "passed": report.passed(),
        "failed": report.failed(),
        "errored": report.errored(),
        "duration_ms": report.duration_ms,
        "cases": cases,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
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
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied())
        .with_library_only_global_names(aven_host::standard_library_only_global_names())
}

fn discover_roots_for_host(path: &Path, host: &aven_host::Host) -> aven_compiler::ModuleRoots {
    host.disabled_capability_modules().into_iter().fold(
        aven_compiler::ModuleRoots::discover(path)
            .with_library(aven_host::STD_LIBRARY_NAME, host.std_library())
            .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied())
            .with_library_only_global_names(host.library_only_global_names()),
        |roots, (specifier, capability, register_method)| {
            roots.with_disabled_capability_module(specifier, capability, register_method)
        },
    )
}

struct PathOperatorConfig {
    file: SourceFile,
    operator_fixities: aven_parser::OperatorFixityTable,
}

fn load_path_operator_config(
    path: &Path,
    roots: &aven_compiler::ModuleRoots,
    operators: &[String],
    host: &aven_host::Host,
    direct_shebang_arguments: Option<&[String]>,
    allow_env_s_transport: bool,
    format: OutputFormat,
) -> Result<PathOperatorConfig> {
    let project = aven_compiler::ProjectConfig::load(roots)
        .with_context(|| "failed to read discovered Aven.toml")?;
    let file = load_source_file(path)?;
    let mut argv_atoms = operators
        .iter()
        .map(|operator| format!("--operator={operator}"))
        .collect::<Vec<_>>();

    if allow_env_s_transport
        && let Ok(Some(invocation)) = aven_compiler::parse_shebang_invocation(file.source())
        && invocation.style() == aven_compiler::ShebangStyle::EnvS
        && argv_atoms.starts_with(invocation.operator_arguments())
    {
        argv_atoms.drain(..invocation.operator_arguments().len());
    }

    let operator_fixities = match project.operator_fixity_table(
        file.source(),
        &argv_atoms,
        host.operator_fixities(),
        direct_shebang_arguments,
    ) {
        Ok(operator_fixities) => operator_fixities,
        Err(diagnostics) => {
            print_operator_config_diagnostics(&project, &file, &argv_atoms, &diagnostics, format)?;
            bail!("operator configuration failed");
        }
    };

    Ok(PathOperatorConfig {
        file,
        operator_fixities,
    })
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

/// Custom operator fixity contributed by the CLI platform.
///
/// Fixity decides how the entry parses, so `check` and `fmt` must see exactly
/// what `run` sees — otherwise a platform operator would run fine but fail to
/// check, or reformat into something that no longer parses. Keeping the
/// registrations in one function shared by all three commands is what makes
/// that agreement structural rather than a thing to remember.
fn register_platform_operators(host: &mut aven_host::Host) {
    // The stock CLI platform declares no custom operators yet; an embedder
    // adding `host.register_operator(..)` here gets it in all three commands.
    let _ = host;
}

/// A host carrying only what parsing needs: the platform's operator fixity.
///
/// `check` and `fmt` take the standard platform's *types* from
/// [`aven_host::standard_check_host_globals`] but never evaluate, so they skip
/// the IO registrations `build_host` performs.
fn parse_only_host() -> aven_host::Host {
    let mut host = aven_host::Host::new();
    register_platform_operators(&mut host);
    host
}

/// Build the host registry that feeds both `run` (values) and `check` (types).
///
/// The CLI owns the concrete IO (the selected log sink, the root trace context,
/// and the bare IO/`dbg` natives); `aven-host` owns the registration/typing
/// vocabulary for the standard host types.
fn build_host(config: &RunConfig) -> Result<aven_host::Host> {
    let mut host = aven_host::Host::new();

    register_platform_operators(&mut host);
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

/// Writes its single argument's `repr` rendering to stderr (optionally
/// prefixed with `file:line: ` from its lexical eval source) and returns the
/// argument unchanged, so `dbg(x)` is usable inline. Keeps stdout clean for the
/// program's value and log output. The IO effect lives in the host, so the
/// native is injected by the CLI prelude rather than `aven-eval`.
fn dbg_native() -> aven_eval::Value {
    aven_eval::Value::native_at(|args, context| {
        let [value] = args else {
            return Err(format!("dbg expects 1 argument, got {}", args.len()));
        };

        let rendered = aven_eval::repr_text(value);
        let mut stderr = io::stderr().lock();
        if let Some(source) = &context.source {
            write!(stderr, "{}", source.format_location(context.span))
                .map_err(|error| error.to_string())?;
        }
        writeln!(stderr, "{rendered}").map_err(|error| error.to_string())?;
        Ok(value.clone())
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
        aven_eval::Value::Int(value) => {
            let text = value.to_string();
            text.parse::<JsonNumber>()
                .map(JsonValue::Number)
                .unwrap_or_else(|_| JsonValue::String(text))
        }
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
        aven_eval::Value::Record(fields) | aven_eval::Value::NamedRecord { fields, .. } => {
            let mut output = JsonMap::new();
            for (name, value) in fields.iter() {
                output.insert(name.clone(), aven_value_json(value));
            }
            JsonValue::Object(output)
        }
        aven_eval::Value::SlotRecord { fields, .. } => {
            let mut output = JsonMap::new();
            for (name, value) in fields.iter() {
                output.insert(name.clone(), aven_value_json(value));
            }
            JsonValue::Object(output)
        }
        aven_eval::Value::BrandedPrimitive { payload, .. } => aven_value_json(&payload.to_value()),
        aven_eval::Value::Tag { name, payload } => json!({
            "tag": name,
            "payload": payload.iter().map(aven_value_json).collect::<Vec<_>>(),
        }),
        aven_eval::Value::ResultMethod { .. } => JsonValue::String("<method>".to_owned()),
        aven_eval::Value::NamedMethod { .. } | aven_eval::Value::UnboundNamedMethod { .. } => {
            JsonValue::String("<method>".to_owned())
        }
        aven_eval::Value::Closure(_) => JsonValue::String("<function>".to_owned()),
        aven_eval::Value::Native(_) => JsonValue::String("<native>".to_owned()),
        aven_eval::Value::Type(ty) => JsonValue::String(ty.to_string()),
        aven_eval::Value::NamedFamily(_) => JsonValue::String(value.to_string()),
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
    let path_backed_files = source_map
        .files()
        .iter()
        .filter(|file| file.path.is_some())
        .collect::<Vec<_>>();
    if let [file] = path_backed_files.as_slice() {
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

/// Same shape as `aven check --format json` diagnostics — delegated to
/// [`AvenDiagnostic`]'s serde impl in `aven-core` so session logs and the CLI
/// cannot drift apart.
fn diagnostic_json(diagnostic: &AvenDiagnostic) -> serde_json::Value {
    serde_json::to_value(diagnostic).expect("Diagnostic always serializes to JSON")
}

fn tokens(path: &Path, session: &mut SessionCapture) -> Result<i32> {
    let file = load_source_file(path)?;
    session.set_entry_source(file.source());
    let output = aven_parser::lex_source(file.source());
    let report = DiagnosticReport::new(file.id, output.diagnostics.clone());
    session.set_diagnostics(report.diagnostics.clone());

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

    Ok(0)
}

fn layout(path: &Path, session: &mut SessionCapture) -> Result<i32> {
    let file = load_source_file(path)?;
    session.set_entry_source(file.source());
    let output = aven_parser::layout_source(file.source());
    let report = DiagnosticReport::new(file.id, output.diagnostics.clone());
    session.set_diagnostics(report.diagnostics.clone());

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

    Ok(0)
}

fn fmt(
    path: &Path,
    check: bool,
    operators: &[String],
    session: &mut SessionCapture,
) -> Result<i32> {
    let host = parse_only_host();
    let roots = aven_compiler::ModuleRoots::discover(path);
    let configured = load_path_operator_config(
        path,
        &roots,
        operators,
        &host,
        None,
        false,
        OutputFormat::Text,
    )?;
    let file = configured.file;
    session.set_entry_source(file.source());
    let formatted =
        match aven_fmt::format_source_with_fixities(file.source(), &configured.operator_fixities) {
            Ok(formatted) => formatted,
            Err(diagnostics) => {
                session.set_diagnostics(diagnostics.clone());
                let report = DiagnosticReport::new(file.id, diagnostics);
                print_diagnostics(&file, &report)?;
                bail!("formatting failed");
            }
        };

    if file.source() == formatted {
        return Ok(0);
    }

    if check {
        bail!("{} is not formatted", path.display());
    }

    fs::write(path, formatted).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(0)
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

fn print_operator_config_diagnostics(
    project: &aven_compiler::ProjectConfig,
    entry: &SourceFile,
    argv_atoms: &[String],
    diagnostics: &[aven_compiler::OperatorConfigDiagnostic],
    format: OutputFormat,
) -> Result<()> {
    let reports = diagnostics
        .iter()
        .enumerate()
        .map(|(index, located)| {
            let id = FileId(index);
            let file = match located.source() {
                aven_compiler::OperatorConfigDiagnosticSource::Manifest => SourceFile::new(
                    id,
                    project
                        .manifest_path()
                        .map_or_else(|| "Aven.toml".to_owned(), |path| path.display().to_string()),
                    project.manifest_path().map(Path::to_path_buf),
                    project.manifest_source().unwrap_or_default(),
                ),
                aven_compiler::OperatorConfigDiagnosticSource::Shebang => {
                    SourceFile::new(id, entry.name.clone(), entry.path.clone(), entry.source())
                }
                aven_compiler::OperatorConfigDiagnosticSource::Argv { declaration_index } => {
                    SourceFile::new(
                        id,
                        format!(
                            "<command-line operator declaration {}>",
                            declaration_index + 1
                        ),
                        None,
                        argv_atoms.get(declaration_index).map_or("", String::as_str),
                    )
                }
                aven_compiler::OperatorConfigDiagnosticSource::Platform => {
                    SourceFile::new(id, "<platform operator configuration>", None, "")
                }
                aven_compiler::OperatorConfigDiagnosticSource::Multiple => {
                    SourceFile::new(id, "<operator configuration>", None, "")
                }
            };
            let report = DiagnosticReport::new(id, vec![located.diagnostic().clone()]);
            (file, report)
        })
        .collect::<Vec<_>>();

    match format {
        OutputFormat::Text => {
            for (file, report) in &reports {
                print_diagnostics(file, report)?;
            }
        }
        OutputFormat::Json => {
            let output = json!({
                "ok": false,
                "files": reports
                    .iter()
                    .map(|(file, report)| json_report(file, report))
                    .collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
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

    /// `run` parses with `build_host`'s fixity while `check` and `fmt` parse
    /// with `parse_only_host`'s. If those ever diverge, a platform operator
    /// would run but fail to check, so they must stay identical. Both are empty
    /// today; this fails the moment a `register_operator` call is added to
    /// `build_host` directly instead of to `register_platform_operators`.
    #[test]
    fn check_and_run_agree_on_platform_operator_fixity() -> Result<()> {
        assert_eq!(
            parse_only_host().operator_fixities(),
            build_host(&RunConfig::default())?.operator_fixities()
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

    #[test]
    fn structured_json_keeps_arbitrary_precision_ints_as_numbers() {
        let value = aven_eval::Value::Int(
            "115132219018763992565095597973971522401"
                .parse()
                .expect("test integer literal is valid"),
        );
        let json = aven_value_json(&value);

        assert!(json.is_number());
        assert_eq!(json.to_string(), "115132219018763992565095597973971522401");
    }
}
