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
        stdout(&output).contains("ok (parse, name, and annotation checks only"),
        "expected success message, got:\n{}",
        stdout(&output)
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
