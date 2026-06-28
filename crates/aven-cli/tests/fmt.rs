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
fn run_uses_predefined_pick_and_omit_builtins() {
    let file = TempFile::new(
        "run-predefined-pick-omit",
        "user = { name: \"Ada\", email: \"ada@x.dev\", age: 3 }\n\
         pick(omit(user, @{\"age\"}), @{\"name\", \"email\"})\n",
    );

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ name: \"Ada\", email: \"ada@x.dev\" }\n");
}

#[test]
fn run_dbg_writes_type_to_stderr_and_keeps_stdout_clean() {
    let file = TempFile::new("run-dbg-type", "User = { name: Text }\ndbg(User)\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "{ name: Text }\n");
    assert_eq!(stderr(&output), "{ name: Text }\n");
}

#[test]
fn run_write_line_writes_to_stdout() {
    let file = TempFile::new("run-write-line", "ignored = writeLine(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi\n");
}

#[test]
fn run_does_not_print_trivial_empty_record_result() {
    // A bare effect call as the final expression returns `{}`; that trivial
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
        stderr(&output).contains("--log syslog is not yet implemented"),
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
fn run_final_err_value_exits_non_zero_and_writes_stderr() {
    let file = TempFile::new("run-final-err", "@Err(\"boom\")\n");

    let output = run_aven(["run"], file.path());

    assert_failure(&output);
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "@Err(\"boom\")\n");
}

#[test]
fn run_final_ok_value_exits_zero() {
    let file = TempFile::new("run-final-ok", "@Ok(\"fine\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(\"fine\")\n");
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
    // `write` adds no newline; the non-trivial `@Ok({})` value is then printed.
    assert_eq!(stdout(&output), "hi@Ok({})\n");
    assert_eq!(stderr(&output), "");
}

#[test]
fn run_stdout_write_line_handle_prints_and_returns_ok() {
    let file = TempFile::new("run-stdout-write-line", "stdout.writeLine(\"hi\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "hi\n@Ok({})\n");
}

#[test]
fn run_stderr_write_handle_goes_to_stderr() {
    let file = TempFile::new("run-stderr-write", "stderr.write(\"oops\")\n");

    let output = run_aven(["run"], file.path());

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok({})\n");
    assert_eq!(stderr(&output), "oops");
}

#[test]
fn run_stdin_read_line_handle_returns_ok_line() {
    let file = TempFile::new("run-stdin-read-line", "stdin.readLine()\n");

    let output = run_aven_with_stdin(["run"], file.path(), "line\nrest\n");

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(\"line\")\n");
}

#[test]
fn run_stdin_read_line_handle_at_eof_returns_ok_undefined() {
    let file = TempFile::new("run-stdin-read-line-eof", "stdin.readLine()\n");

    let output = run_aven_with_stdin(["run"], file.path(), "");

    assert_success(&output);
    assert_eq!(stdout(&output), "@Ok(undefined)\n");
}

#[test]
fn run_bare_write_returns_record_while_handle_write_returns_result() {
    // The boundary, locked at runtime: bare `write` evaluates to the trivial
    // `{}` (not printed), while `stdout.write` evaluates to `@Ok({})`.
    let bare = TempFile::new("run-bare-write-shape", "write(\"x\")\n");
    let bare_output = run_aven(["run"], bare.path());
    assert_success(&bare_output);
    assert_eq!(stdout(&bare_output), "x");

    let handle = TempFile::new("run-handle-write-shape", "stdout.write(\"x\")\n");
    let handle_output = run_aven(["run"], handle.path());
    assert_success(&handle_output);
    assert_eq!(stdout(&handle_output), "x@Ok({})\n");
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
