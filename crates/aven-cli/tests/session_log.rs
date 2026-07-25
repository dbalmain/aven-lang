//! Integration tests for `AVEN_SESSION_LOG` structured session logging.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use aven_core::SessionRecord;

#[test]
fn unset_session_log_creates_no_file_and_preserves_behaviour() {
    let file = TempFile::new("no-log", "value = 1\n");
    // Choose a path that must not appear unless logging is enabled.
    let log_path = unique_path("should-not-exist");

    let output = Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(["check"])
        .arg(file.path())
        .env_remove("AVEN_SESSION_LOG")
        .env_remove("AVEN_SESSION_TAG")
        .output()
        .expect("run aven");

    assert_exit(&output, 0);
    assert!(
        !log_path.exists(),
        "unset AVEN_SESSION_LOG must not create a log file"
    );
    let stdout = stdout(&output);
    assert!(
        stdout.contains("ok"),
        "expected successful check stdout, got:\n{stdout}"
    );
}

#[test]
fn three_sequential_checks_append_three_ordered_records() {
    let file = TempFile::new("repair-sequence", "value = 1\n");
    let log = TempLog::new("repair");

    for _ in 0..3 {
        let output = run_with_log(["check"], file.path(), log.path(), None);
        assert_exit(&output, 0);
    }

    let records = read_records(log.path());
    assert_eq!(records.len(), 3, "expected three JSONL records");
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.subcommand, "check");
        assert_eq!(record.exit_code, 0);
        assert_eq!(record.schema_version, aven_core::SESSION_SCHEMA_VERSION);
        if index > 0 {
            assert!(
                record.timestamp >= records[index - 1].timestamp,
                "records should be in non-decreasing timestamp order"
            );
        }
    }
}

#[test]
fn failing_check_record_carries_diagnostic_codes() {
    let file = TempFile::new("type-error", "value : Missing = value\n");
    let log = TempLog::new("fail-check");

    let output = run_with_log(["check", "--format", "json"], file.path(), log.path(), None);
    assert_exit(&output, 1);

    // Existing JSON stdout contract unchanged.
    let stdout_json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout JSON");
    assert_eq!(stdout_json["ok"], false);
    assert_eq!(stdout_json["diagnostics"][0]["code"], "type.unknown-name");

    let records = read_records(log.path());
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.subcommand, "check");
    assert_eq!(record.exit_code, 1);
    assert!(
        record
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some("type.unknown-name")),
        "expected type.unknown-name in session diagnostics: {:?}",
        record.diagnostics
    );
    assert!(record.entry_path.is_some());
    assert!(record.entry_source_sha256.is_some());
    assert!(record.timings.is_some());
}

#[test]
fn test_subcommand_record_carries_summary_and_exit_code() {
    let file = TempFile::new(
        "suite-fail",
        r#"test = import("std/test")

{
  "passes": () => test.pass,
  "fails": () => test.expectEq(1, 2),
}
"#,
    );
    let log = TempLog::new("test-summary");

    let output = run_with_log(["test", "--format", "json"], file.path(), log.path(), None);
    assert_exit(&output, 1);

    let records = read_records(log.path());
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.subcommand, "test");
    assert_eq!(record.exit_code, 1);

    let summary = record
        .summary
        .as_ref()
        .expect("test record should carry summary");
    match summary {
        aven_core::SessionSummary::Test {
            total,
            passed,
            failed,
            errored,
        } => {
            assert_eq!(*total, 2);
            assert_eq!(*passed, 1);
            assert_eq!(*failed, 1);
            assert_eq!(*errored, 0);
        }
    }
}

#[test]
fn session_tag_is_copied_through_verbatim() {
    let file = TempFile::new("tagged", "value = 1\n");
    let log = TempLog::new("tag");
    let tag = "exercism-hello-world/attempt-7";

    let output = run_with_log(["check"], file.path(), log.path(), Some(tag));
    assert_exit(&output, 0);

    let records = read_records(log.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].tag.as_deref(), Some(tag));
}

#[test]
fn unwritable_session_log_path_does_not_change_exit_or_stdout() {
    let file = TempFile::new("unwritable-log", "value = 1\n");
    // Directory path: open() for append fails (EISDIR).
    let bad_path = std::env::temp_dir();

    let baseline = Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(["check"])
        .arg(file.path())
        .env_remove("AVEN_SESSION_LOG")
        .env_remove("AVEN_SESSION_TAG")
        .output()
        .expect("baseline run");

    let with_bad_log = Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(["check"])
        .arg(file.path())
        .env("AVEN_SESSION_LOG", &bad_path)
        .env_remove("AVEN_SESSION_TAG")
        .output()
        .expect("run with bad log path");

    assert_eq!(baseline.status.code(), with_bad_log.status.code());
    assert_eq!(baseline.stdout, with_bad_log.stdout);
    assert_exit(&with_bad_log, 0);
}

#[test]
fn written_line_deserializes_to_session_record() {
    let file = TempFile::new("roundtrip", "value = 1 + 2\n");
    let log = TempLog::new("roundtrip");

    let output = run_with_log(["check"], file.path(), log.path(), Some("rt-tag"));
    assert_exit(&output, 0);

    let contents = fs::read_to_string(log.path()).expect("read log");
    let line = contents.lines().next().expect("one line");
    let record: SessionRecord = serde_json::from_str(line).expect("deserialize SessionRecord");
    assert_eq!(record.subcommand, "check");
    assert_eq!(record.tag.as_deref(), Some("rt-tag"));
    assert_eq!(record.aven_version, env!("CARGO_PKG_VERSION"));
    assert!(record.argv.iter().any(|arg| arg == "check"));
}

#[test]
fn test_suite_unrunnable_records_exit_two() {
    let file = TempFile::new("not-a-suite", "42\n");
    let log = TempLog::new("unrunnable");

    let output = run_with_log(["test", "--format", "json"], file.path(), log.path(), None);
    assert_exit(&output, 2);

    let records = read_records(log.path());
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exit_code, 2);
    assert_eq!(records[0].subcommand, "test");
    assert!(
        records[0]
            .diagnostics
            .iter()
            .any(|d| d.code.as_deref() == Some(aven_core::codes::test::NOT_A_RECORD)),
        "expected suite-shape diagnostic, got {:?}",
        records[0].diagnostics
    );
}

fn run_with_log<const N: usize>(
    args: [&str; N],
    path: &Path,
    log_path: &Path,
    tag: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aven"));
    command
        .args(args)
        .arg(path)
        .env("AVEN_SESSION_LOG", log_path);
    match tag {
        Some(tag) => {
            command.env("AVEN_SESSION_TAG", tag);
        }
        None => {
            command.env_remove("AVEN_SESSION_TAG");
        }
    }
    command.output().expect("failed to run aven")
}

fn read_records(path: &Path) -> Vec<SessionRecord> {
    let contents = fs::read_to_string(path).expect("read session log");
    contents
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("failed to deserialize session record ({error}): {line}")
            })
        })
        .collect()
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "expected exit {expected}, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
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

fn unique_path(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "aven-session-log-{label}-{}-{unique}.jsonl",
        std::process::id()
    ))
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(label: &str, source: &str) -> Self {
        let path = unique_path(label).with_extension("av");
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

struct TempLog {
    path: PathBuf,
}

impl TempLog {
    fn new(label: &str) -> Self {
        let path = unique_path(label);
        let _ = fs::remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempLog {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
