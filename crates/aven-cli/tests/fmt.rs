use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn fmt_check_accepts_formatted_source() {
    let source = "value =\n  item = 1\n";
    let file = TempFile::new("formatted", source);

    let output = run_aven(["fmt", "--check"], file.path());

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(file.path()).expect("failed to reread formatted source"),
        source
    );
}

#[test]
fn fmt_check_rejects_unformatted_source_without_writing() {
    let source = "value =\n    item = 1   \n";
    let file = TempFile::new("unformatted", source);

    let output = run_aven(["fmt", "--check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("is not formatted"),
        "expected fmt --check message, got:\n{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(file.path()).expect("failed to reread unformatted source"),
        source
    );
}

#[test]
fn fmt_writes_formatted_source() {
    let file = TempFile::new("write", "value =\n    item = 1   \n");

    let output = run_aven(["fmt"], file.path());

    assert_success(&output);
    assert_eq!(
        fs::read_to_string(file.path()).expect("failed to reread written source"),
        "value =\n  item = 1\n"
    );
}

#[test]
fn fmt_refuses_parse_errors_without_writing() {
    let source = "value = )\n";
    let file = TempFile::new("parse-error", source);

    let output = run_aven(["fmt"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("parse.unexpected-delimiter"),
        "expected parse diagnostic, got:\n{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(file.path()).expect("failed to reread parse-error source"),
        source
    );
}

#[test]
fn check_accepts_valid_source() {
    let file = TempFile::new("check-ok", "value = 1\n");

    let output = run_aven(["check"], file.path());

    assert_success(&output);
    assert!(
        stdout(&output)
            .contains("ok (parse, name, annotation, and partial monomorphic inference checks)"),
        "expected success message, got:\n{}",
        stdout(&output)
    );
}

#[test]
fn check_timings_reports_text_timings() {
    let file = TempFile::new("check-timings", "value = 1\n");

    let output = run_aven(["check", "--timings"], file.path());

    assert_success(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("timings:"),
        "expected timings header, got:\n{stderr}"
    );
    assert!(
        stderr.contains("parse:"),
        "expected parse timing, got:\n{stderr}"
    );
    assert!(
        stderr.contains("check:"),
        "expected check timing, got:\n{stderr}"
    );
}

#[test]
fn check_timings_marks_skipped_semantic_phases() {
    let file = TempFile::new("skipped-timings", "value = )\n");

    let output = run_aven(["check", "--timings"], file.path());

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("name: skipped"),
        "expected skipped name timing, got:\n{stderr}"
    );
    assert!(
        stderr.contains("check: skipped"),
        "expected skipped check timing, got:\n{stderr}"
    );
}

#[test]
fn check_reports_name_diagnostics() {
    let source = "value = 1\nvalue = 2\n";
    let file = TempFile::new("name-error", source);

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("name.duplicate-declaration"),
        "expected name diagnostic, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn check_reports_type_diagnostics() {
    let source = "value : Missing = value\n";
    let file = TempFile::new("type-error", source);

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("type.unknown-name"),
        "expected type diagnostic, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn check_accepts_logger_calls_while_untyped() {
    // `logger` is registered runtime-only until default/optional parameters
    // (Milestone D) let its optional trailing fields argument be typed without
    // rejecting the one-argument form. Both call shapes must check cleanly.
    let one_arg = TempFile::new("check-logger-one", "logger.info(\"hi\")\n");
    let two_arg = TempFile::new("check-logger-two", "logger.info(\"hi\", { n: 1 })\n");

    assert_success(&run_aven(["check"], one_arg.path()));
    assert_success(&run_aven(["check"], two_arg.path()));
}

#[test]
fn check_rejects_platform_call_with_wrong_argument_type() {
    let file = TempFile::new("check-platform-arg", "Platform.Console.log(42)\n");

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("type.mismatch"),
        "expected type mismatch, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn check_accepts_valid_platform_call() {
    let file = TempFile::new("check-platform-ok", "Platform.Console.log(\"hi\")\n");

    let output = run_aven(["check"], file.path());

    assert_success(&output);
}

#[test]
fn check_json_reports_structured_diagnostics() {
    let source = "value : Missing = value\n";
    let file = TempFile::new("json-type-error", source);

    let output = run_aven(["check", "--format", "json"], file.path());

    assert_failure(&output);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("expected valid JSON diagnostics");

    assert_eq!(json["ok"], false);
    assert_eq!(json["fileId"], 0);
    assert_eq!(json["diagnostics"][0]["severity"], "error");
    assert_eq!(json["diagnostics"][0]["code"], "type.unknown-name");
    assert_eq!(
        json["diagnostics"][0]["message"],
        "unknown type name `Missing`"
    );
    assert_eq!(json["diagnostics"][0]["labels"][0]["span"]["start"], 8);
    assert_eq!(json["diagnostics"][0]["labels"][0]["span"]["end"], 15);
}

#[test]
fn check_json_timings_reports_structured_timings() {
    let file = TempFile::new("json-timings", "value = 1\n");

    let output = run_aven(["check", "--format", "json", "--timings"], file.path());

    assert_success(&output);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("expected valid JSON diagnostics");

    assert_eq!(json["ok"], true);
    assert!(json["timingsMs"]["parse"].is_number());
    assert!(json["timingsMs"]["name"].is_number());
    assert!(json["timingsMs"]["check"].is_number());
    assert!(json["timingsMs"]["total"].is_number());
}

#[test]
fn check_json_timings_marks_skipped_semantic_phases() {
    let file = TempFile::new("json-skipped-timings", "value = )\n");

    let output = run_aven(["check", "--format", "json", "--timings"], file.path());

    assert_failure(&output);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("expected valid JSON diagnostics");

    assert_eq!(json["ok"], false);
    assert!(json["timingsMs"]["parse"].is_number());
    assert!(json["timingsMs"]["name"].is_null());
    assert!(json["timingsMs"]["check"].is_null());
    assert!(json["timingsMs"]["total"].is_number());
}

#[test]
fn run_prints_last_expression_value() {
    let file = TempFile::new("run-ok", "1 + 2 * 3\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "7\n");
}

#[test]
fn run_prints_final_value_after_bindings() {
    let file = TempFile::new("run-bindings", "x = 5\ny = x + 1\ny\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "6\n");
}

#[test]
fn run_prints_function_call_value() {
    let file = TempFile::new("run-function", "double = (x) => x * 2\ndouble(5)\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "10\n");
}

#[test]
fn run_applies_parameter_default_when_omitted() {
    let file = TempFile::new(
        "run-default-omitted",
        "greet = (name, greeting = \"hello\") => greeting + \", \" + name\ngreet(\"world\")\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hello, world\n");
}

#[test]
fn run_overrides_parameter_default_when_supplied() {
    let file = TempFile::new(
        "run-default-supplied",
        "greet = (name, greeting = \"hello\") => greeting + \", \" + name\ngreet(\"world\", \"hi\")\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi, world\n");
}

#[test]
fn run_prints_pick_record_comprehension_value() {
    let file = TempFile::new(
        "run-pick-record-comprehension",
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }\n\
         result : { name: Text, email: Text } = pick(user, @{\"name\", \"email\"})\n\
         result\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ name: \"Ada\", email: \"ada@x.dev\" }\n");
}

#[test]
fn run_prints_omit_record_comprehension_value() {
    let file = TempFile::new(
        "run-omit-record-comprehension",
        "User = { name: Text, email: Text }\n\
         user : User = { name: \"Ada\", email: \"ada@x.dev\" }\n\
         omit = (o: {..r}, @keys: keysOf(r)@{}) => { keysOf(o) -> k, !keys.has(k); (k, o[k]) }\n\
         result : { email: Text } = omit(user, @{\"name\"})\n\
         result\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ email: \"ada@x.dev\" }\n");
}

#[test]
fn run_debug_writes_type_to_stderr_and_keeps_stdout_clean() {
    let file = TempFile::new("run-debug-type", "User = { name: Text }\ndebug(User)\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ name: Text }\n");
    assert_eq!(stderr(&output), "{ name: Text }\n");
}

#[test]
fn run_injects_default_console_platform() {
    let file = TempFile::new(
        "run-platform-log",
        "Platform.Console.log(\"Hello, Aven!\")\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "Hello, Aven!\n");
}

#[test]
fn run_log_writes_structured_json_line() {
    let file = TempFile::new(
        "run-ambient-structured-log",
        "logger.info(\"hello\", { n: 1 })\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains("\"msg\":\"hello\""),
        "expected log message, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"level\":\"info\""),
        "expected info level, got:\n{stdout}"
    );
    assert!(
        stdout.contains("\"n\":1"),
        "expected numeric attribute, got:\n{stdout}"
    );
    let records = json_log_lines(&stdout);
    assert_eq!(records.len(), 1, "expected one log line, got:\n{stdout}");
    assert_w3c_trace_context(&records[0], &stdout);
}

#[test]
fn run_ambient_log_and_platform_log_share_trace_context() {
    let file = TempFile::new(
        "run-shared-structured-log",
        "logger.info(\"hello\", { n: 1 })\nPlatform.Log.info(\"hello\", { n: 1 })\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    let stdout = stdout(&output);
    let records = json_log_lines(&stdout);
    assert_eq!(records.len(), 2, "expected two log lines, got:\n{stdout}");

    let ambient = &records[0];
    let namespaced = &records[1];
    for field in ["level", "severity", "msg", "n"] {
        assert_eq!(
            ambient[field], namespaced[field],
            "expected matching `{field}` fields, got:\n{stdout}"
        );
    }
    for field in ["traceId", "spanId", "traceFlags", "traceState"] {
        assert_eq!(
            ambient[field], namespaced[field],
            "expected shared trace `{field}`, got:\n{stdout}"
        );
    }
    assert_w3c_trace_context(ambient, &stdout);
    assert_w3c_trace_context(namespaced, &stdout);
}

#[test]
fn run_user_binding_shadows_prelude_log() {
    let file = TempFile::new("run-shadow-ambient-log", "logger = 5\nlogger\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "5\n");
}

#[test]
fn run_prints_match_factorial_value() {
    let file = TempFile::new(
        "run-match-factorial",
        "fact = (n) =>\n  n ?>\n    0 => 1\n    _ => n * fact(n - 1)\nfact(5)\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "120\n");
}

#[test]
fn run_prints_record_field_access_value() {
    let file = TempFile::new(
        "run-record",
        "user = { name: \"Ada\", age: 36 }\nuser.name\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "Ada\n");
}

#[test]
fn run_prints_collection_and_nullable_program_value() {
    let file = TempFile::new(
        "run-collections",
        "xs = [10, 20, 30]\npair = (1, \"a\")\nset = @{ 1, 2, 2, 3 }\nchosen = null?.name ?? xs[1]\npair ?>\n  (n, _) => chosen + n\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "21\n");
}

#[test]
fn run_reports_runtime_diagnostics() {
    let file = TempFile::new("run-error", "1 / 0\n");

    let output = run_aven(["run"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("runtime.division-by-zero"),
        "expected runtime diagnostic, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn run_threads_result_with_propagation_operator() {
    let file = TempFile::new(
        "run-propagate",
        "parse = (n) =>\n  n ?>\n    0 => @Err(\"zero\")\n    _ => @Ok(n)\n\
         add = (a, b) =>\n  x = parse(a)?^\n  y = parse(b)?^\n  @Ok(x + y)\n\
         add(2, 3)\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(5)\n");
}

#[test]
fn run_panic_operator_exits_non_zero_with_runtime_panic() {
    let file = TempFile::new("run-panic", "@Err(\"boom\")?!\n");

    let output = run_aven(["run"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("runtime.panic"),
        "expected runtime.panic diagnostic, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn explain_prints_diagnostic_explanations() {
    let output = run_aven_without_path(["explain", "parse.unclosed-delimiter"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains("parse.unclosed-delimiter"),
        "expected diagnostic code, got:\n{stdout}"
    );
    assert!(
        stdout.contains("opened but not closed"),
        "expected explanation text, got:\n{stdout}"
    );
}

#[test]
fn explain_rejects_unknown_diagnostic_codes() {
    let output = run_aven_without_path(["explain", "parse.not-real"]);

    assert_failure(&output);
    assert!(
        stderr(&output).contains("no explanation found"),
        "expected unknown-code error, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn tokens_prints_lexer_stream() {
    let file = TempFile::new("tokens", "value = 1\n");

    let output = run_aven(["tokens"], file.path());

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains("identifier `value`"),
        "expected identifier token, got:\n{stdout}"
    );
    assert!(
        stdout.contains("operator `=`"),
        "expected operator token, got:\n{stdout}"
    );
    assert!(
        stdout.contains("number `1`"),
        "expected number token, got:\n{stdout}"
    );
}

#[test]
fn layout_prints_layout_stream() {
    let file = TempFile::new("layout", "value =\n  item = 1\n");

    let output = run_aven(["layout"], file.path());

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains("layout indent"),
        "expected layout indent token, got:\n{stdout}"
    );
    assert!(
        stdout.contains("layout dedent"),
        "expected layout dedent token, got:\n{stdout}"
    );
}

fn run_aven<const N: usize>(args: [&str; N], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(args)
        .arg(path)
        .output()
        .expect("failed to run aven")
}

fn run_aven_without_path<const N: usize>(args: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(args)
        .output()
        .expect("failed to run aven")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_log_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => panic!("expected valid JSON log line, got {error}: {line}"),
        })
        .collect()
}

fn assert_w3c_trace_context(record: &serde_json::Value, stdout: &str) {
    let trace_id = record
        .get("traceId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        is_lower_hex(trace_id, 32),
        "expected 32-lower-hex traceId, got:\n{stdout}"
    );

    let span_id = record
        .get("spanId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        is_lower_hex(span_id, 16),
        "expected 16-lower-hex spanId, got:\n{stdout}"
    );

    assert_eq!(record["traceFlags"], "01", "unexpected traceFlags");
    assert_eq!(record["traceState"], "", "unexpected traceState");
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(label: &str, source: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aven-fmt-{label}-{}-{unique}.av",
            std::process::id()
        ));
        fs::write(&path, source).expect("failed to write temp source");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
