//! Integration tests for `aven test`: exit codes and JSON contract.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn test_all_pass_exits_zero_and_json_shape() {
    let file = TempFile::new(
        "all-pass",
        r#"test = import("std/test")

{
  "one equals one": () => test.expectEq(1, 1),
  "empty is empty": () => test.expectEq([], []),
  "pass helper": () => test.pass,
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 0);
    let json = parse_json(&output);
    assert_eq!(json["ok"], true);
    assert_eq!(json["total"], 3);
    assert_eq!(json["passed"], 3);
    assert_eq!(json["failed"], 0);
    assert_eq!(json["errored"], 0);
    assert!(json["duration_ms"].is_number());
    assert!(json["path"].as_str().is_some_and(|p| p.ends_with(".av")));
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases.len(), 3);
    assert_eq!(cases[0]["name"], "one equals one");
    assert_eq!(cases[0]["outcome"], "pass");
    assert!(cases[0]["duration_ms"].is_number());
    assert!(cases[0].get("message").is_none());
    assert!(cases[0].get("diagnostics").is_none());
    assert_eq!(cases[1]["name"], "empty is empty");
    assert_eq!(cases[2]["name"], "pass helper");
}

#[test]
fn test_failing_case_continues_and_exits_one() {
    let file = TempFile::new(
        "with-fail",
        r#"test = import("std/test")

{
  "passes first": () => test.expectEq(1, 1),
  "fails middle": () => test.expectEq([], [0]),
  "passes after": () => test.pass,
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["total"], 3);
    assert_eq!(json["passed"], 2);
    assert_eq!(json["failed"], 1);
    assert_eq!(json["errored"], 0);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[0]["outcome"], "pass");
    assert_eq!(cases[1]["outcome"], "fail");
    assert_eq!(cases[1]["message"], "expected [0], got []");
    assert!(cases[1].get("diagnostics").is_none());
    assert_eq!(cases[2]["outcome"], "pass");
}

#[test]
fn test_runtime_error_continues_and_exits_one() {
    let file = TempFile::new(
        "with-error",
        r#"test = import("std/test")

{
  "passes first": () => test.expectEq(1, 1),
  "explodes": () => (undefined)(),
  "passes after": () => test.pass,
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["total"], 3);
    assert_eq!(json["passed"], 2);
    assert_eq!(json["failed"], 0);
    assert_eq!(json["errored"], 1);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[0]["outcome"], "pass");
    assert_eq!(cases[1]["outcome"], "error");
    assert!(cases[1].get("message").is_none());
    let diagnostics = cases[1]["diagnostics"]
        .as_array()
        .expect("error diagnostics array");
    assert!(!diagnostics.is_empty());
    assert!(diagnostics[0]["severity"].is_string());
    assert!(diagnostics[0]["message"].is_string());
    assert_eq!(cases[2]["outcome"], "pass");
}

#[test]
fn test_mixed_pass_fail_error_json_contract() {
    let file = TempFile::new(
        "mixed",
        r#"test = import("std/test")

{
  "reverses a list": () => test.expectEq([3, 2, 1], [3, 2, 1]),
  "handles empty": () => test.expectEq([], [0]),
  "explodes": () => (undefined)(),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["total"], 3);
    assert_eq!(json["passed"], 1);
    assert_eq!(json["failed"], 1);
    assert_eq!(json["errored"], 1);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[0]["name"], "reverses a list");
    assert_eq!(cases[0]["outcome"], "pass");
    assert_eq!(cases[1]["name"], "handles empty");
    assert_eq!(cases[1]["outcome"], "fail");
    assert_eq!(cases[1]["message"], "expected [0], got []");
    assert_eq!(cases[2]["name"], "explodes");
    assert_eq!(cases[2]["outcome"], "error");
    assert!(cases[2]["diagnostics"].is_array());
}

#[test]
fn test_malformed_entry_not_record_exits_two() {
    let file = TempFile::new("not-record", "42\n");

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 2);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert!(
        json.get("cases").is_none(),
        "unrunnable suite must not emit cases"
    );
    let code = json["diagnostics"][0]["code"].as_str().unwrap_or("");
    assert_eq!(code, "test.not-a-record");
}

#[test]
fn test_malformed_field_not_callable_exits_two() {
    let file = TempFile::new(
        "not-callable",
        r#"{
  "bad": 123,
  "good": () => @Ok({}),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 2);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert!(json.get("cases").is_none());
    assert_eq!(json["diagnostics"][0]["code"], "test.not-callable");
}

#[test]
fn test_malformed_field_non_zero_arity_exits_two() {
    let file = TempFile::new(
        "needs-args",
        r#"{
  "needs arg": (x) => @Ok(x),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 2);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert!(json.get("cases").is_none());
    assert_eq!(json["diagnostics"][0]["code"], "test.non-zero-arity");
}

#[test]
fn test_load_error_exits_two_with_check_style_json() {
    let file = TempFile::new("parse-fail", "{\n  \"oops\": () =>\n");

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 2);
    let json = parse_json(&output);
    assert_eq!(json["ok"], false);
    assert!(json.get("cases").is_none());
    assert!(
        json["diagnostics"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
    );
}

#[test]
fn test_chains_assertions_with_propagate() {
    let file = TempFile::new(
        "chain",
        r#"test = import("std/test")

{
  "chains ok": () =>
    test.expectEq(2, 2)?^
    test.expectEq(0, 0)
  "chains fail early": () =>
    test.expectEq(1, 2)?^
    test.expectEq(0, 0)
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["total"], 2);
    assert_eq!(json["passed"], 1);
    assert_eq!(json["failed"], 1);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[0]["outcome"], "pass");
    assert_eq!(cases[1]["outcome"], "fail");
    assert_eq!(cases[1]["message"], "expected 2, got 1");
}

#[test]
fn test_std_test_helpers_via_suite() {
    let file = TempFile::new(
        "std-test-helpers",
        r#"test = import("std/test")

{
  "eq": () => test.expectEq(1, 1),
  "ne": () => test.expectNe(1, 2),
  "true": () => test.expectTrue(true),
  "false": () => test.expectFalse(false),
  "ok": () => test.expectOk(@Ok(1)),
  "err": () => test.expectErr(@Err("x")),
  "pass": () => test.pass,
  "fail intentionally": () => test.fail("nope"),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["total"], 8);
    assert_eq!(json["passed"], 7);
    assert_eq!(json["failed"], 1);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[7]["outcome"], "fail");
    assert_eq!(cases[7]["message"], "nope");
}

/// `aven check` and `aven test` must agree on quoted sentence case names.
/// Before the fix, check rejected them as type exports while test ran them.
#[test]
fn check_and_test_agree_on_quoted_uppercase_case_names() {
    let file = TempFile::new(
        "quoted-case-names",
        r#"test = import("std/test")

{
  "Zero is fine at runtime": () => test.pass,
}
"#,
    );

    let checked = run_aven(["check"], file.path());
    assert_exit(&checked, 0);

    let tested = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&tested, 0);
    let json = parse_json(&tested);
    assert_eq!(json["ok"], true);
    assert_eq!(json["total"], 1);
    assert_eq!(json["passed"], 1);
}

/// Identifier-shaped quoted uppercase names (`"Yacht"`) are value fields, not
/// type exports. check and test must both accept them.
#[test]
fn check_and_test_agree_on_quoted_identifier_uppercase_case_names() {
    let file = TempFile::new(
        "quoted-yacht-case",
        r#"test = import("std/test")

{
  "Yacht": () => test.pass,
}
"#,
    );

    let checked = run_aven(["check"], file.path());
    assert_exit(&checked, 0);

    let tested = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&tested, 0);
    let json = parse_json(&tested);
    assert_eq!(json["ok"], true);
    assert_eq!(json["total"], 1);
    assert_eq!(json["passed"], 1);
}

#[test]
fn test_expect_approx_eq_within_outside_and_default_tolerance() {
    let file = TempFile::new(
        "approx-eq",
        r#"test = import("std/test")

{
  "within tolerance": () => test.expectApproxEq(1.0, 1.001, 0.01),
  "outside tolerance": () => test.expectApproxEq(1.0, 2.0, 0.01),
  "default tolerance near equal": () => test.expectApproxEq(1.0, 1.0 + 1.0e-12),
  "default tolerance far": () => test.expectApproxEq(1.0, 1.001),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    assert_eq!(json["total"], 4);
    assert_eq!(json["passed"], 2);
    assert_eq!(json["failed"], 2);
    let cases = json["cases"].as_array().expect("cases array");
    assert_eq!(cases[0]["outcome"], "pass");
    assert_eq!(cases[1]["outcome"], "fail");
    let outside = cases[1]["message"].as_str().expect("fail message");
    assert!(
        outside.contains("1") && outside.contains("2") && outside.contains("0.01"),
        "failure must name actual, expected, and tolerance; got: {outside}"
    );
    assert_eq!(cases[2]["outcome"], "pass");
    assert_eq!(cases[3]["outcome"], "fail");
    let default_fail = cases[3]["message"].as_str().expect("default fail message");
    assert!(
        default_fail.contains("tolerance") && default_fail.contains("0.000000001"),
        "default-tolerance failure must report the default used; got: {default_fail}"
    );
}

#[test]
fn test_text_output_is_terse() {
    let file = TempFile::new(
        "text-out",
        r#"test = import("std/test")

{
  "ok case": () => test.pass,
  "fail case": () => test.expectEq(1, 2),
}
"#,
    );

    let output = run_aven(["test"], file.path());
    assert_exit(&output, 1);
    let stdout = stdout(&output);
    assert!(stdout.contains("ok  - ok case"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("FAIL - fail case: expected 2, got 1"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("1 passed, 1 failed"), "stdout:\n{stdout}");
}

#[test]
fn test_explain_knows_test_codes() {
    for code in [
        "test.not-a-record",
        "test.not-callable",
        "test.non-zero-arity",
        "test.not-a-result",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_aven"))
            .args(["explain", code])
            .output()
            .expect("failed to run aven explain");
        assert_exit(&output, 0);
        let text = stdout(&output);
        assert!(
            text.contains(code),
            "explain {code} should print the code, got:\n{text}"
        );
    }
}

fn run_aven<const N: usize>(args: [&str; N], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(args)
        .arg(path)
        .output()
        .expect("failed to run aven")
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

fn parse_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected valid JSON on stdout ({error})\nstdout:\n{}\nstderr:\n{}",
            stdout(output),
            stderr(output)
        )
    })
}

/// Pins the `(actual, expected)` argument order of the `std/test` helpers.
///
/// Every other fixture passes symmetric arguments, so none of them can tell the
/// two orders apart: swapping the parameters still passes and still fails on
/// exactly the same inputs, and only the *message* comes out inverted. This is
/// the one test that fails if the orientation flips, so the assertion messages
/// keep agreeing with what `aven explain test.not-callable` teaches.
#[test]
fn std_test_helper_messages_report_actual_and_expected_in_order() {
    let file = TempFile::new(
        "orientation",
        r#"test = import("std/test")

{
  "eq reports expected then actual": () => test.expectEq(1, 2),
  "ok reports the err payload": () => test.expectOk(@Err("boom")),
  "err reports the ok payload": () => test.expectErr(@Ok(7)),
}
"#,
    );

    let output = run_aven(["test", "--format", "json"], file.path());
    assert_exit(&output, 1);
    let json = parse_json(&output);
    let cases = json["cases"].as_array().expect("cases array");
    // `expectEq(actual, expected)`: 1 is what we got, 2 is what we wanted.
    assert_eq!(cases[0]["message"], "expected 2, got 1");
    assert_eq!(cases[1]["message"], "expected @Ok, got @Err(boom)");
    assert_eq!(cases[2]["message"], "expected @Err, got @Ok(7)");
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
            "aven-test-cmd-{label}-{}-{unique}.av",
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
