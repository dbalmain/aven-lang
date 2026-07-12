//! The embedded `std` library through the real binary: bare specifiers resolve
//! without any filesystem beside the entry file.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn std_time_type_exports_check() {
    let dir = TempDir::new("std-time-check");
    let entry = dir.write(
        "main.av",
        "time = import(\"std/time\")\n\
         { Instant } = import(\"std/time\")\n\
         start : time.Instant = Instant.parse(\"2026-01-01T00:00:00Z\")?!\n\
         { start }\n",
    );

    let output = aven(&["check", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven check failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn std_time_instant_parse_runs() {
    let dir = TempDir::new("std-time-run");
    let entry = dir.write(
        "main.av",
        "std = import(\"std\")\n\
         time = import(\"std/time\")\n\
         { Instant } = import(\"std/time\")\n\
         viaBinding = Instant.parse(\"2026-01-01T00:00:00Z\")?!\n\
         viaMember = time.Instant.parse(\"2026-07-11T12:30:00Z\")?!\n\
         writeLine(std.version)\n\
         writeLine(viaBinding.format())\n\
         writeLine(viaMember.format())\n",
    );

    let output = aven(&["run", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "0.1.0\n2026-01-01T00:00:00Z\n2026-07-11T12:30:00Z\n"
    );
}

#[test]
fn clock_and_zones_capabilities_are_imported_as_modules() {
    let dir = TempDir::new("std-capabilities");
    let entry = dir.write(
        "main.av",
        "{ now } = import(\"std/clock\")\n\
         { zone } = import(\"std/zones\")\n\
         writeLine(now().format())\n\
         zone\n",
    );
    let path = entry.to_str().expect("temp path is UTF-8");

    let checked = aven(&["check", path]);
    assert!(
        checked.status.success(),
        "aven check failed:\n{}\n{}",
        stdout(&checked),
        stderr(&checked)
    );

    let output = aven(&["run", path]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stdout(&output)
            .lines()
            .next()
            .is_some_and(|line| line.ends_with('Z'))
    );
}

#[test]
fn std_array_type_exports_check() {
    let dir = TempDir::new("std-array-check");
    let entry = dir.write(
        "main.av",
        r#"{ length, isEmpty, first, last, fold, sum, count, all, any, find, indexOf } = import("std/array")
xs = [10, 20, 30]
empty = []
zero: Int = 0
len = length(xs)
emptyFlag = isEmpty(empty)
head = first(xs)
tail = last(xs)
folded = fold(xs, zero, (acc, x) => acc + x)
total = sum([1, 2, 3])
n = count(xs, (x) => x > 15)
allPos = all(xs, (x) => x > 0)
has20 = any(xs, (x) => x == 20)
hit = find(xs, (x) => x == 20)
miss = find(xs, (x) => x == 99)
idx = indexOf(xs, 20)
{ length, isEmpty, first, last, fold, sum, count, all, any, find, indexOf, len, emptyFlag, head, tail, folded, total, n, allPos, has20, hit, miss, idx }
"#,
    );

    let output = aven(&["check", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven check failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn std_array_combinators_run() {
    let dir = TempDir::new("std-array-run");
    let entry = dir.write(
        "main.av",
        r#"{ length, isEmpty, first, last, fold, sum, count, all, any, find, indexOf } = import("std/array")
xs = [10, 20, 30]
empty = []
zero: Int = 0
writeLine("${length(xs)}")
writeLine("${isEmpty(xs)}")
writeLine("${isEmpty(empty)}")
writeLine("${first(xs)}")
writeLine("${first(empty)}")
writeLine("${last(xs)}")
writeLine("${last(empty)}")
writeLine("${fold(xs, zero, (acc, x) => acc + x)}")
writeLine("${sum([1, 2, 3])}")
writeLine("${count(xs, (x) => x > 15)}")
writeLine("${all(xs, (x) => x > 0)}")
writeLine("${any(xs, (x) => x == 20)}")
writeLine("${find(xs, (x) => x == 20)}")
writeLine("${find(xs, (x) => x == 99)}")
writeLine("${indexOf(xs, 20)}")
writeLine("${indexOf(xs, 99)}")
writeLine("${indexOf(empty, 1)}")
"#,
    );

    let output = aven(&["run", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "3\nfalse\ntrue\n10\nundefined\n30\nundefined\n60\n6\n2\ntrue\ntrue\n20\nundefined\n1\nundefined\nundefined\n"
    );
}

#[test]
fn std_result_type_exports_check() {
    let dir = TempDir::new("std-result-check");
    let entry = dir.write(
        "main.av",
        r#"{ mapErr, orElse, map, unwrapOr, isOk, isErr } = import("std/result")
ok : Result(Int, Text) = @Ok(1)
err : Result(Int, Text) = @Err("x")
zero: Int = 0
mappedOk = mapErr(ok, (e) => "wrap: ${e}")
mappedErr = mapErr(err, (e) => "wrap: ${e}")
recovered = orElse(err, (_) => @Ok(0))
passed = orElse(ok, (_) => @Ok(0))
mapped = map(ok, (v) => v + 1)
fallback = unwrapOr(err, zero)
okFlag = isOk(ok)
errFlag = isErr(err)
chain = (r: Result(Int, Text)) =>
  value = mapErr(r, (e) => "step failed: ${e}")?^
  @Ok(value)
{ mapErr, orElse, map, unwrapOr, isOk, isErr, mappedOk, mappedErr, recovered, passed, mapped, fallback, okFlag, errFlag, chain }
"#,
    );

    let output = aven(&["check", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven check failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn std_result_map_err_and_or_else_run() {
    let dir = TempDir::new("std-result-run");
    let entry = dir.write(
        "main.av",
        r#"{ mapErr, orElse } = import("std/result")
show = (r) => r ?> @Ok(v) => writeLine("Ok(${v})"), @Err(e) => writeLine("Err(${e})")
show(mapErr(@Ok(7), (e) => "wrap: ${e}"))
show(mapErr(@Err("boom"), (e) => "wrap: ${e}"))
show(orElse(@Err("boom"), (_) => @Ok(0)))
show(orElse(@Ok(3), (_) => @Ok(0)))
chain = (r) =>
  value = mapErr(r, (e) => "step failed: ${e}")?^
  @Ok(value)
show(chain(@Ok(9)))
show(chain(@Err("nope")))
"#,
    );

    let output = aven(&["run", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "Ok(7)\nErr(wrap: boom)\nOk(0)\nOk(3)\nOk(9)\nErr(step failed: nope)\n"
    );
}

#[test]
fn std_result_combinators_run() {
    let dir = TempDir::new("std-result-combinators-run");
    let entry = dir.write(
        "main.av",
        r#"{ map, unwrapOr, isOk, isErr } = import("std/result")
ok : Result(Int, Text) = @Ok(7)
err : Result(Int, Text) = @Err("boom")
zero: Int = 0
show = (r) => r ?> @Ok(v) => writeLine("Ok(${v})"), @Err(e) => writeLine("Err(${e})")
show(map(ok, (v) => v + 1))
show(map(err, (v) => v + 1))
writeLine("${unwrapOr(ok, zero)}")
writeLine("${unwrapOr(err, zero)}")
writeLine("${isOk(ok)}")
writeLine("${isOk(err)}")
writeLine("${isErr(ok)}")
writeLine("${isErr(err)}")
"#,
    );

    let output = aven(&["run", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        "Ok(8)\nErr(boom)\n7\n0\ntrue\nfalse\nfalse\ntrue\n"
    );
}

#[test]
fn unregistered_library_and_missing_std_module_diagnose() {
    let dir = TempDir::new("std-time-errors");
    let entry = dir.write(
        "main.av",
        "a = import(\"nolib\")\nb = import(\"std/nope\")\n{ a, b }\n",
    );

    let output = aven(&["check", entry.to_str().expect("temp path is UTF-8")]);

    assert!(!output.status.success(), "check should fail");
    let out = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        out.contains("module.unsupported-root"),
        "expected unsupported-root, got:\n{out}"
    );
    assert!(
        out.contains("module.not-found"),
        "expected not-found, got:\n{out}"
    );
    assert!(
        out.contains("tried `std/nope` in library `std`"),
        "expected tried note, got:\n{out}"
    );
}

fn aven(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aven"))
        .args(args)
        .output()
        .expect("failed to run aven")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
            std::env::temp_dir().join(format!("aven-cli-{label}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).expect("failed to create temp directory");
        Self { path }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.path.join(relative);
        fs::write(&path, source).expect("failed to write source file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
