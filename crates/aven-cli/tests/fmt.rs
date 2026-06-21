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
