use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
fn check_accepts_inline_match_arms() {
    let file = TempFile::new(
        "check-inline-match",
        "value = 1 ?> 0 => \"zero\", 1 => \"one\", _ => \"other\"\n",
    );

    let output = run_aven(["check"], file.path());

    assert_success(&output);
    assert!(
        stdout(&output)
            .contains("ok (parse, name, annotation, and partial monomorphic inference checks)"),
        "expected success message, got:\n{}",
        stdout(&output)
    );
}

/// Formatting must preserve quoted field names so a check-clean program stays
/// check-clean after `aven fmt` (quoted uppercase is a value field; bare is not).
#[test]
fn fmt_preserves_quoted_uppercase_field_and_stays_check_clean() {
    let file = TempFile::new("quoted-yacht-fmt", "value = 1\n{ \"Yacht\": value }\n");

    assert_success(&run_aven(["check"], file.path()));
    assert_success(&run_aven(["fmt"], file.path()));
    let formatted = fs::read_to_string(file.path()).expect("failed to read formatted source");
    assert!(
        formatted.contains("\"Yacht\""),
        "fmt must preserve quoted field name; got:\n{formatted}"
    );
    assert!(
        !formatted.contains("{ Yacht:") && !formatted.contains("{Yacht:"),
        "fmt must not rewrite quoted Yacht to bare; got:\n{formatted}"
    );
    assert_success(&run_aven(["check"], file.path()));
}

#[test]
fn fmt_preserves_parenthesized_inline_matches_that_exceed_the_line_width() {
    let file = TempFile::new(
        "parenthesized-inline-match",
        "f = (r: Result(Int, Text)) => r.map((n) => (n >= 0 ?> true => \"quite a long ok payload here\", false => \"quite a long error payload here\"))\n",
    );

    assert_success(&run_aven(["fmt"], file.path()));
    let once = fs::read_to_string(file.path()).expect("failed to read formatted source");
    assert_eq!(
        once,
        "f = (r: Result(Int, Text)) => r.map((n) => (n >= 0 ?> true => \"quite a long ok payload here\", false => \"quite a long error payload here\"))\n"
    );
    assert_success(&run_aven(["check"], file.path()));

    assert_success(&run_aven(["fmt"], file.path()));
    assert_eq!(
        fs::read_to_string(file.path()).expect("failed to reread formatted source"),
        once
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
fn check_timings_reports_semantic_phases_after_parse_errors() {
    let file = TempFile::new("parse-error-timings", "value = )\n");

    let output = run_aven(["check", "--timings"], file.path());

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("name:"),
        "expected name timing, got:\n{stderr}"
    );
    assert!(
        stderr.contains("check:"),
        "expected check timing, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("skipped"),
        "expected semantic timings to be recorded, got:\n{stderr}"
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
fn check_renders_diagnostics_after_multibyte_source_text() {
    let file = TempFile::new(
        "check-unicode-diagnostic-span",
        "# Strict record self-reference — unproductive\n\
         Tree = { value: Int, children: Tree }\n",
    );

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("Tree = { value: Int, children: Tree }")
            && stderr.contains("unproductive recursive type declared here"),
        "expected the definition and underline label, got:\n{stderr}"
    );
    assert!(
        stderr.contains(":2:1 ]"),
        "expected the diagnostic to start at line 2, column 1, got:\n{stderr}"
    );
}

#[test]
fn check_accepts_logger_call_with_optional_fields_omitted() {
    let file = TempFile::new("check-logger-one", "logger.info(\"hi\")\n");

    assert_success(&run_aven(["check"], file.path()));
}

#[test]
fn check_accepts_logger_call_with_optional_fields_supplied() {
    let file = TempFile::new("check-logger-two", "logger.info(\"hi\", { n: 1 })\n");

    assert_success(&run_aven(["check"], file.path()));
}

#[test]
fn check_rejects_logger_call_with_wrong_message_type() {
    let file = TempFile::new("check-logger-int", "logger.info(42)\n");

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("type.mismatch"),
        "expected type mismatch (Int vs Text), got:\n{}",
        stderr(&output)
    );
}

#[test]
fn check_rejects_logger_call_with_too_few_arguments() {
    let file = TempFile::new("check-logger-none", "logger.info()\n");

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("type.mismatch") && stderr.contains("between 1 and 2 arguments"),
        "expected a 1..=2 arity diagnostic, got:\n{stderr}"
    );
}

#[test]
fn check_accepts_bare_io_globals() {
    let file = TempFile::new(
        "check-io-globals",
        "write(\"a\")\nwriteLine(\"b\")\nline = readLine()\nall : Text = readAll()\n",
    );

    let output = run_aven(["check"], file.path());

    assert_success(&output);
}

#[test]
fn check_accepts_dbg_call() {
    let file = TempFile::new("check-dbg", "dbg(42)\n");

    let output = run_aven(["check"], file.path());

    assert_success(&output);
}

#[test]
fn check_accepts_dbg_result_matching_annotation() {
    let file = TempFile::new("check-dbg-int", "x : Int = dbg(42)\nx\n");

    let output = run_aven(["check"], file.path());

    assert_success(&output);
}

#[test]
fn check_rejects_dbg_result_mismatching_annotation() {
    let file = TempFile::new("check-dbg-text", "x : Text = dbg(42)\nx\n");

    let output = run_aven(["check"], file.path());

    assert_failure(&output);
    assert!(
        stderr(&output).contains("type.mismatch"),
        "expected type mismatch, got:\n{}",
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
fn check_json_timings_reports_semantic_phases_after_parse_errors() {
    let file = TempFile::new("json-parse-error-timings", "value = )\n");

    let output = run_aven(["check", "--format", "json", "--timings"], file.path());

    assert_failure(&output);
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("expected valid JSON diagnostics");

    assert_eq!(json["ok"], false);
    assert!(json["timingsMs"]["parse"].is_number());
    assert!(json["timingsMs"]["name"].is_number());
    assert!(json["timingsMs"]["check"].is_number());
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
fn check_and_run_agree_on_arbitrary_precision_integers() {
    let file = TempFile::new(
        "run-arbitrary-precision-int",
        "y = 99999999999999999999\n\
         x = 115132219018763992565095597973971522401\n\
         unsigned_edge = 18446744073709551615\n\
         signed_min = -9223372036854775808\n\
         grown = 9223372036854775807 + 1\n\
         writeLine(\"${y}\")\n\
         writeLine(\"${unsigned_edge}\")\n\
         writeLine(\"${signed_min}\")\n\
         writeLine(\"${grown}\")\n\
         x\n",
    );

    assert_success(&run_aven(["check"], file.path()));
    let output = run_aven(["run"], file.path());
    assert_success(&output);
    assert_eq!(
        stdout(&output),
        concat!(
            "99999999999999999999\n",
            "18446744073709551615\n",
            "-9223372036854775808\n",
            "9223372036854775808\n",
            "115132219018763992565095597973971522401\n",
        )
    );
}

#[test]
fn check_and_run_support_applied_recursive_type_values() {
    let file = TempFile::new(
        "run-applied-recursive-type",
        "Chain = (t: Type) => { value: t, next: ?Chain(t) }\n\
         target = Chain(Int)\n\
         target\n",
    );

    assert_success(&run_aven(["check"], file.path()));
    let output = run_aven(["run"], file.path());
    assert_success(&output);
    assert_eq!(stdout(&output), "Chain(Int)\n");
}

#[test]
fn applied_type_statics_check_and_run_agree() {
    let file = TempFile::new(
        "run-applied-type-statics",
        "applied_array = Array(Int).range(0, 5)\n\
         annotated_array: Array(Int) = Array(Int).range(0, 3)\n\
         bare_array = Array.range(0, 2)\n\
         applied_map = Map(Text, Int).empty()\n\
         annotated_map: Map(Text, Int) = Map(Text, Int).empty()\n\
         bare_map: Map(Text, Int) = Map.empty()\n\
         applied_array.length() + annotated_array.length() + bare_array.length() + applied_map.size() + annotated_map.size() + bare_map.size()\n",
    );

    let checked = run_aven(["check"], file.path());
    assert_success(&checked);
    assert!(!stderr(&checked).contains("runtime.missing-field"));

    let output = run_aven(["run"], file.path());
    assert_success(&output);
    assert_eq!(stdout(&output), "10\n");
    assert!(!stderr(&output).contains("runtime.missing-field"));
}

#[test]
fn check_and_run_support_applied_recursive_decode_targets() {
    let file = TempFile::new(
        "run-applied-recursive-decode",
        "Chain = (t: Type) => { value: t, next: ?Chain(t) }\n\
         decoded = Json.decode(\"{\\\"value\\\":1}\", Chain(Int))?!\n\
         decoded.value\n",
    );

    assert_success(&run_aven(["check"], file.path()));
    let output = run_aven(["run"], file.path());
    assert_success(&output);
    assert_eq!(stdout(&output), "1\n");
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
fn run_parses_text_numbers_with_optional_fallbacks() {
    let file = TempFile::new(
        "run-text-number-parsing",
        "[\"42\".toInt() ?? 0, \"not a float\".toFloat() ?? 1.5]\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "[42, 1.5]\n");
}

#[test]
fn run_bridges_optional_parse_to_result() {
    let file = TempFile::new(
        "run-optional-to-result",
        concat!(
            "parse = (raw: Text): Result(Int, Text) =>\n",
            "  n = raw.toInt().toResult(\"could not parse: ${raw}\")?^\n",
            "  @Ok(n)\n",
            "parse(\"12\")\n",
        ),
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(12)\n");
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
    assert_eq!(stdout(&output), "{ name: Ada, email: ada@x.dev }\n");
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
    assert_eq!(stdout(&output), "{ email: ada@x.dev }\n");
}

#[test]
fn run_uses_predefined_pick_and_omit_builtins() {
    let file = TempFile::new(
        "run-predefined-pick-omit",
        "user = { name: \"Ada\", email: \"ada@x.dev\", age: 3 }\n\
         pick(omit(user, @{\"age\"}), @{\"name\", \"email\"})\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ name: Ada, email: ada@x.dev }\n");
}

#[test]
fn run_dbg_writes_type_to_stderr_and_keeps_stdout_clean() {
    let file = TempFile::new("run-dbg-type", "User = { name: Text }\ndbg(User)\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    // Final program value still uses the display protocol on stdout.
    assert_eq!(stdout(&output), "{ name: Text }\n");
    // `dbg` writes only to stderr, with a `file:line: ` prefix and `repr` body.
    let file_name = file
        .path()
        .file_name()
        .expect("temp path has a file name")
        .to_string_lossy();
    assert_eq!(
        stderr(&output),
        format!("{file_name}:2: {{ name: Text }}\n")
    );
}

#[test]
fn run_dbg_renders_repr_and_returns_argument() {
    // Pins: quoted text (ambiguity fix), constructor wrap over toText override,
    // inline usability, stderr-only side effect, location prefix.
    let source = "\
Money = Int {
  toText(): Text => \"$${.}\"
}
m: Money = 2749
text = dbg(\"a, b\")
n = dbg(42)
arr = dbg([\"a\", \"b\"])
rec = dbg({ a: 1 })
money = dbg(m)
(text, n, arr, rec, money)
";
    let file = TempFile::new("run-dbg-repr", source);

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    // Returned values are unchanged (Money still brands); final print uses toText.
    assert_eq!(stdout(&output), "(a, b, 42, [a, b], { a: 1 }, $2749)\n");
    let file_name = file
        .path()
        .file_name()
        .expect("temp path has a file name")
        .to_string_lossy();
    let stderr = stderr(&output);
    let expected = [
        format!("{file_name}:5: \"a, b\""),
        format!("{file_name}:6: 42"),
        format!("{file_name}:7: [\"a\", \"b\"]"),
        format!("{file_name}:8: {{ a: 1 }}"),
        format!("{file_name}:9: Money(2749)"),
    ]
    .join("\n")
        + "\n";
    assert_eq!(stderr, expected, "dbg stderr:\n{stderr}");
}

#[test]
fn run_dbg_location_uses_imported_module_file() {
    let dir = TempDir::new("run-dbg-import");
    let lib_path = dir.path().join("lib.av");
    let main_path = dir.path().join("main.av");
    fs::write(&lib_path, "value = dbg(99)\n{ value }\n").expect("write lib");
    fs::write(&main_path, "{ value } = import(\"./lib\")\nvalue\n").expect("write main");

    let output = run_aven(["run"], &main_path);

    assert_success(&output);
    assert_eq!(stdout(&output), "99\n");
    assert_eq!(stderr(&output), "lib.av:1: 99\n");
}

#[test]
fn run_dbg_in_imported_closure_uses_defining_module_location() {
    let dir = TempDir::new("run-dbg-import-closure");
    let lib_path = dir.path().join("lib.av");
    let main_path = dir.path().join("main.av");
    fs::write(
        &lib_path,
        "helper = (value) =>\n  marker = \"library\"\n  dbg(value)\n{ helper }\n",
    )
    .expect("write lib");
    fs::write(
        &main_path,
        "padding = \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"\n\
         { helper } = import(\"./lib\")\n\
         helper(\"called from main\")\n",
    )
    .expect("write main");

    let output = run_aven(["run"], &main_path);

    assert_success(&output);
    assert_eq!(stdout(&output), "called from main\n");
    assert_eq!(stderr(&output), "lib.av:3: \"called from main\"\n");
}

#[test]
fn run_dbg_without_native_source_omits_location_prefix() {
    let file = TempFile::new(
        "run-dbg-no-source",
        "value = @Ok(\"from callback\").map(dbg)\nvalue\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(from callback)\n");
    assert_eq!(stderr(&output), "\"from callback\"\n");
}

#[test]
fn run_write_line_writes_to_stdout() {
    let file = TempFile::new("run-write-line", "ignored = writeLine(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi\n");
}

#[test]
fn run_does_not_print_trivial_unit_result() {
    // A bare effect call as the final expression returns `()`; that trivial
    // value must not be printed after the effect's own output.
    let file = TempFile::new("run-trivial-result", "writeLine(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi\n");
}

#[test]
fn run_write_writes_to_stdout_without_newline() {
    let file = TempFile::new("run-write", "ignored = write(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi");
}

#[test]
fn run_read_line_and_read_all_consume_stdin() {
    let file = TempFile::new(
        "run-read-line-all",
        "first = readLine()\nrest = readAll()\nfirst + \"|\" + rest\n",
    );

    let output = run_aven_with_stdin(["run"], file.path(), "one\ntwo\nthree");

    assert_success(&output);
    assert_eq!(stdout(&output), "one|two\nthree\n");
}

#[test]
fn run_read_line_at_eof_returns_undefined() {
    let file = TempFile::new("run-read-line-eof", "readLine()\n");

    let output = run_aven_with_stdin(["run"], file.path(), "");

    assert_success(&output);
    assert_eq!(stdout(&output), "undefined\n");
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
fn run_log_file_writes_structured_json_line() {
    let file = TempFile::new("run-log-file-source", "logger.info(\"hello\", { n: 1 })\n");
    let log_file = TempFile::new("run-log-file-output", "");
    let log_path = log_file.path().to_string_lossy().into_owned();

    let output = run_aven(["run", "--log", log_path.as_str()], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "");
    let log_output = fs::read_to_string(log_file.path()).expect("failed to read log file");
    assert!(
        log_output.contains("\"msg\":\"hello\""),
        "expected log message, got:\n{log_output}"
    );
    assert!(
        log_output.contains("\"n\":1"),
        "expected numeric attribute, got:\n{log_output}"
    );
    let records = json_log_lines(&log_output);
    assert_eq!(
        records.len(),
        1,
        "expected one log line, got:\n{log_output}"
    );
    assert_w3c_trace_context(&records[0], &log_output);
}

#[test]
fn run_log_format_text_writes_one_line_record() {
    let file = TempFile::new(
        "run-text-log",
        "logger.warn(\"careful\", { n: 2, user: \"ada\" })\n",
    );

    let output = run_aven(["run", "--log-format", "text"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "WARN careful n=2 user=ada\n");
}

#[test]
fn run_log_syslog_reports_not_implemented() {
    let file = TempFile::new("run-syslog", "1\n");

    let output = run_aven(["run", "--log", "syslog"], file.path());

    assert_failure(&output);
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("--log syslog is unavailable"),
        "expected syslog stub error, got:\n{}",
        stderr(&output)
    );
}

#[test]
fn run_ambient_log_and_child_log_share_trace_context() {
    let file = TempFile::new(
        "run-shared-structured-log",
        "logger.info(\"hello\", { n: 1 })\nchild = logger.child({ requestId: \"r1\" })\nchild.info(\"child\", { n: 2 })\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    let stdout = stdout(&output);
    let records = json_log_lines(&stdout);
    assert_eq!(records.len(), 2, "expected two log lines, got:\n{stdout}");

    let ambient = &records[0];
    let child = &records[1];
    assert_eq!(ambient["msg"], "hello");
    assert_eq!(child["msg"], "child");
    assert_eq!(child["requestId"], "r1");
    for field in ["traceId", "spanId", "traceFlags", "traceState"] {
        assert_eq!(
            ambient[field], child[field],
            "expected shared trace `{field}`, got:\n{stdout}"
        );
    }
    assert_w3c_trace_context(ambient, &stdout);
    assert_w3c_trace_context(child, &stdout);
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
fn run_prints_string_interpolation_value() {
    let file = TempFile::new(
        "run-interpolation",
        "count = 3\n\"${count} files copied\"\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "3 files copied\n");
}

#[test]
fn run_final_value_uses_display_protocol() {
    let file = TempFile::new("run-display-protocol", "[\"a\", \"b\"]\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "[a, b]\n");
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
fn checked_integer_division_program_checks_and_runs() {
    let file = TempFile::new(
        "checked-int-division",
        concat!(
            "x : Int = 7\n",
            "divide = x.div\n",
            "{\n",
            "  operator: x / 2,\n",
            "  checked: x.div(2),\n",
            "  zeroDiv: x.div(0),\n",
            "  zeroMod: x.mod(0),\n",
            "  remainder: x % 2,\n",
            "  bound: divide(2),\n",
            "}\n",
        ),
    );

    assert_success(&run_aven(["check"], file.path()));
    let output = run_aven(["run"], file.path());
    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "{ operator: 3, checked: 3, zeroDiv: undefined, zeroMod: undefined, remainder: 1, bound: 3 }\n"
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
fn run_loads_user_json_file_with_typed_decode_and_propagation() {
    let data = TempFile::new(
        "run-load-user-json-data",
        "{\"name\":\"Ada\",\"nick\":null}\n",
    );
    let data_path = data.path().to_string_lossy().into_owned();
    let source = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\nloadUser(\"{data_path}\")\n",
        "User = { name: Text, email: ?Text, nick: Text? }",
        "loadUser = (path: Text) =>",
        "  file = File.open(path, \"r\")?^",
        "  text = file.readAll()?^",
        "  user = Json.decode(text, User)?^",
        "  @Ok(user)",
    );
    let file = TempFile::new("run-load-user-json-source", &source);

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(
        stdout(&output),
        "@Ok({ name: Ada, email: undefined, nick: null })\n"
    );
}

#[test]
fn run_final_err_value_exits_non_zero_and_writes_stderr() {
    let file = TempFile::new("run-final-err", "@Err(\"boom\")\n");

    let output = run_aven(["run"], file.path());

    assert_failure(&output);
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "@Err(boom)\n");
}

#[test]
fn run_final_ok_value_exits_zero() {
    let file = TempFile::new("run-final-ok", "@Ok(\"fine\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(fine)\n");
    assert_eq!(stderr(&output), "");
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
fn run_stdout_write_handle_prints_and_returns_ok() {
    let file = TempFile::new("run-stdout-write", "stdout.write(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    // `write` adds no newline; the non-trivial `@Ok(())` value is then printed.
    assert_eq!(stdout(&output), "hi@Ok(())\n");
    assert_eq!(stderr(&output), "");
}

#[test]
fn run_stdout_write_line_handle_prints_and_returns_ok() {
    let file = TempFile::new("run-stdout-write-line", "stdout.writeLine(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi\n@Ok(())\n");
}

#[test]
fn run_stderr_write_handle_goes_to_stderr() {
    let file = TempFile::new("run-stderr-write", "stderr.write(\"oops\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(())\n");
    assert_eq!(stderr(&output), "oops");
}

#[test]
fn run_stdin_read_line_handle_returns_ok_line() {
    let file = TempFile::new("run-stdin-read-line", "stdin.readLine()\n");

    let output = run_aven_with_stdin(["run"], file.path(), "line\nrest\n");

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(line)\n");
}

#[test]
fn run_stdin_read_line_handle_at_eof_returns_ok_undefined() {
    let file = TempFile::new("run-stdin-read-line-eof", "stdin.readLine()\n");

    let output = run_aven_with_stdin(["run"], file.path(), "");

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(undefined)\n");
}

#[test]
fn run_bare_write_returns_unit_while_handle_write_returns_result() {
    // The boundary, locked at runtime: bare `write` evaluates to the trivial
    // `()` (not printed), while `stdout.write` evaluates to `@Ok(())`.
    let bare = TempFile::new("run-bare-write-shape", "write(\"x\")\n");
    let bare_output = run_aven(["run"], bare.path());
    assert_success(&bare_output);
    assert_eq!(stdout(&bare_output), "x");

    let handle = TempFile::new("run-handle-write-shape", "stdout.write(\"x\")\n");
    let handle_output = run_aven(["run"], handle.path());
    assert_success(&handle_output);
    assert_eq!(stdout(&handle_output), "x@Ok(())\n");
}

/// The canonical print-every-element loop, through the whole toolchain. `each`
/// is typed `(a) -> ()`, so this only checks if the writer's result is that
/// same `()`. It is exercised on both collections, and through `check` as well
/// as `run` — `run` never consulted the type, so a checker-only regression here
/// would otherwise still print the right thing.
#[test]
fn each_over_write_line_checks_and_runs_for_array_and_set() {
    for (label, source, expected) in [
        (
            "array-each-write-line",
            "[1, 2].each((x) => writeLine(\"${x}\"))\n",
            "1\n2\n",
        ),
        (
            "set-each-write-line",
            "@{ 1, 2 }.each((x) => writeLine(\"${x}\"))\n",
            "1\n2\n",
        ),
    ] {
        let file = TempFile::new(label, source);

        let checked = run_aven(["check"], file.path());
        assert_success(&checked);

        let ran = run_aven(["run"], file.path());
        assert_success(&ran);
        // Only the loop's own output: the `()` each call yields is trivial and
        // must not be printed after it.
        assert_eq!(stdout(&ran), expected, "{label}");
    }
}

/// A bare writer call as a statement stays clean. The spec reserves the right
/// to reject ignoring a non-`Unit` value; typing the writers `()` is what keeps
/// that rule from firing on the most common statement in the language, so this
/// pins that the rule is not on today.
#[test]
fn bare_write_line_statements_check_clean_in_a_block() {
    let file = TempFile::new(
        "bare-write-line-statements",
        "main = () =>\n  writeLine(\"a\")\n  writeLine(\"b\")\nmain()\n",
    );

    let checked = run_aven(["check"], file.path());
    assert_success(&checked);

    let ran = run_aven(["run"], file.path());
    assert_success(&ran);
    assert_eq!(stdout(&ran), "a\nb\n");
}

#[test]
fn explain_prints_diagnostic_explanations() {
    let output = run_aven_without_path(["explain", "type.unused-result"]);

    assert_success(&output);
    let stdout = stdout(&output);
    assert!(
        stdout.contains("type.unused-result"),
        "expected diagnostic code, got:\n{stdout}"
    );
    assert!(
        stdout.contains("assign it to `_`"),
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

fn run_aven_with_stdin<const N: usize>(args: [&str; N], path: &Path, stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(args)
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run aven");

    let mut child_stdin = child.stdin.take().expect("failed to open aven stdin");
    child_stdin
        .write_all(stdin.as_bytes())
        .expect("failed to write aven stdin");
    drop(child_stdin);

    child.wait_with_output().expect("failed to wait for aven")
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

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aven-fmt-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
