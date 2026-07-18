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
r#"{ range } = import("std/array")
xs = [10, 20, 30]
empty = []
zero: Int = 0
len = xs.length()
emptyFlag = empty.isEmpty()
head = xs.first()
tail = xs.last()
folded = xs.fold(zero, (acc, x) => acc + x)
total = [1, 2, 3].sum()
n = xs.count((x) => x > 15)
allPos = xs.all((x) => x > 0)
has20 = xs.any((x) => x == 20)
hit = xs.find((x) => x == 20)
miss = xs.find((x) => x == 99)
idx = xs.indexOf(20)
mapped = xs.map((x) => x + 1)
flatMapped = xs.flatMap((x) => [x, x + 1])
filtered = xs.filter((x) => x > 15)
rev = xs.reverse()
joined = [1].concat([2, 3])
composed = xs.filter((x) => x > 15).map((x) => x / 10)
taken = xs.take(2)
dropped = xs.drop(1)
sliced = xs.slice(1, 3)
zipped = [1, 2, 3].zip([10, 20])
flat = [[1], [2, 3]].flatten()
nums = range(1, 4)
sorted = [3, 1, 2].sortWith((a, b) => a < b)
users = [{name: "bob", age: 30}, {name: "alice", age: 25}, {name: "carol", age: 30}]
byAge = users.sortBy((u) => u.age)
lo = xs.minimum()
hi = xs.maximum()
{ range, len, emptyFlag, head, tail, folded, total, n, allPos, has20, hit, miss, idx, mapped, flatMapped, filtered, rev, joined, composed, taken, dropped, sliced, zipped, flat, nums, sorted, byAge, lo, hi }
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
        r#"{ range } = import("std/array")
xs = [10, 20, 30]
empty = []
emptyNested: Array(Array(Int)) = []
zero: Int = 0
writeLine("${xs.length()}")
writeLine("${xs.isEmpty()}")
writeLine("${empty.isEmpty()}")
writeLine("${xs.first()}")
writeLine("${empty.first()}")
writeLine("${xs.last()}")
writeLine("${empty.last()}")
writeLine("${xs.fold(zero, (acc, x) => acc + x)}")
writeLine("${[1, 2, 3].sum()}")
writeLine("${xs.count((x) => x > 15)}")
writeLine("${xs.all((x) => x > 0)}")
writeLine("${xs.any((x) => x == 20)}")
writeLine("${xs.find((x) => x == 20)}")
writeLine("${xs.find((x) => x == 99)}")
writeLine("${xs.indexOf(20)}")
writeLine("${xs.indexOf(99)}")
writeLine("${empty.indexOf(1)}")
writeLine("${xs.map((x) => x + 1)}")
writeLine("${empty.map((x) => x + 1)}")
writeLine("${xs.flatMap((x) => [x, x + 1])}")
writeLine("${empty.flatMap((x) => [x])}")
writeLine("${xs.flatMap((_) => [])}")
writeLine("${xs.filter((x) => x > 15)}")
writeLine("${empty.filter((x) => x > 15)}")
writeLine("${xs.reverse()}")
writeLine("${empty.reverse()}")
writeLine("${[1].concat([2, 3])}")
writeLine("${empty.concat(xs)}")
writeLine("${xs.concat(empty)}")
writeLine("${xs.filter((x) => x > 15).map((x) => x / 10)}")
writeLine("${xs.take(2)}")
writeLine("${xs.take(0)}")
writeLine("${xs.take(-1)}")
writeLine("${xs.take(99)}")
writeLine("${empty.take(2)}")
writeLine("${xs.drop(2)}")
writeLine("${xs.drop(0)}")
writeLine("${xs.drop(-1)}")
writeLine("${xs.drop(99)}")
writeLine("${empty.drop(2)}")
writeLine("${xs.slice(1, 3)}")
writeLine("${xs.slice(2, 2)}")
writeLine("${xs.slice(-5, 2)}")
writeLine("${xs.slice(1, 99)}")
writeLine("${xs.slice(0, -1)}")
writeLine("${xs.slice(-2, 99)}")
writeLine("${xs.slice(-99, 2)}")
writeLine("${xs.slice(3, 2)}")
writeLine("${xs.slice(-1, -3)}")
writeLine("${empty.slice(0, 1)}")
writeLine("${empty.slice(-1, 0)}")
writeLine("${xs[-1]}")
writeLine("${xs[-3]}")
writeLine("${xs[-4]}")
writeLine("${empty[-1]}")
writeLine("${[1, 2, 3].zip([10, 20])}")
writeLine("${empty.zip(xs)}")
writeLine("${xs.zip(empty)}")
writeLine("${[[1, 2], [3], [], [4]].flatten()}")
writeLine("${emptyNested.flatten()}")
writeLine("${range(1, 5)}")
writeLine("${range(3, 3)}")
writeLine("${range(5, 1)}")
writeLine("${[3, 1, 2].sortWith((a, b) => a < b)}")
writeLine("${empty.sortWith((a, b) => a < b)}")
pairs = [{k: 2, id: 1}, {k: 1, id: 2}, {k: 2, id: 3}]
writeLine("${pairs.sortWith((a, b) => a.k < b.k)}")
users = [{name: "bob", age: 30}, {name: "alice", age: 25}, {name: "carol", age: 30}]
writeLine("${users.sortBy((u) => u.age)}")
writeLine("${[{age: 1}, {age: 2}].sortBy((u) => u.age)}")
emptyUsers: Array({age: Int}) = []
writeLine("${emptyUsers.sortBy((u) => u.age)}")
writeLine("${pairs.sortBy((u) => u.k)}")
writeLine("${xs.minimum()}")
writeLine("${empty.minimum()}")
writeLine("${xs.maximum()}")
writeLine("${empty.maximum()}")
"#,
    );

    let output = aven(&["run", entry.to_str().expect("temp path is UTF-8")]);

    assert!(
        output.status.success(),
        "aven run failed:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    // Hand-verified: slice negatives wrap then clamp; xs[-1]/[-3] wrap; xs[-4]/empty[-1] undefined.
    // sortBy: by age; already sorted; empty; equal keys keep input order (stable).
    assert_eq!(
        stdout(&output),
        "3\nfalse\ntrue\n10\nundefined\n30\nundefined\n60\n6\n2\ntrue\ntrue\n20\nundefined\n1\nundefined\nundefined\n[11, 21, 31]\n[]\n[10, 11, 20, 21, 30, 31]\n[]\n[]\n[20, 30]\n[]\n[30, 20, 10]\n[]\n[1, 2, 3]\n[10, 20, 30]\n[10, 20, 30]\n[2, 3]\n[10, 20]\n[]\n[]\n[10, 20, 30]\n[]\n[30]\n[10, 20, 30]\n[10, 20, 30]\n[]\n[]\n[20, 30]\n[]\n[10, 20]\n[20, 30]\n[10, 20]\n[20, 30]\n[10, 20]\n[]\n[]\n[]\n[]\n30\n10\nundefined\nundefined\n[(1, 10), (2, 20)]\n[]\n[]\n[1, 2, 3, 4]\n[]\n[1, 2, 3, 4]\n[]\n[]\n[1, 2, 3]\n[]\n[{ k: 1, id: 2 }, { k: 2, id: 1 }, { k: 2, id: 3 }]\n[{ name: \"alice\", age: 25 }, { name: \"bob\", age: 30 }, { name: \"carol\", age: 30 }]\n[{ age: 1 }, { age: 2 }]\n[]\n[{ k: 1, id: 2 }, { k: 2, id: 1 }, { k: 2, id: 3 }]\n10\nundefined\n30\nundefined\n"
    );
}

#[test]
fn std_result_type_exports_check() {
    let dir = TempDir::new("std-result-check");
    let entry = dir.write(
        "main.av",
r#"{ mapErr, orElse, map, andThen, unwrapOr, isOk, isErr } = import("std/result")
ok : Result(Int, Text) = @Ok(1)
err : Result(Int, Text) = @Err("x")
zero: Int = 0
mappedOk = mapErr(ok, (e) => "wrap: ${e}")
mappedErr = mapErr(err, (e) => "wrap: ${e}")
recovered = orElse(err, (_) => @Ok(0))
passed = orElse(ok, (_) => @Ok(0))
mapped = map(ok, (v) => v + 1)
chained = andThen(ok, (v) => @Ok(v + 1))
fallback = unwrapOr(err, zero)
okFlag = isOk(ok)
errFlag = isErr(err)
chain = (r: Result(Int, Text)) =>
  value = mapErr(r, (e) => "step failed: ${e}")?^
  @Ok(value)
{ mapErr, orElse, map, andThen, unwrapOr, isOk, isErr, mappedOk, mappedErr, recovered, passed, mapped, chained, fallback, okFlag, errFlag, chain }
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
        r#"{ map, andThen, unwrapOr, isOk, isErr } = import("std/result")
ok : Result(Int, Text) = @Ok(7)
err : Result(Int, Text) = @Err("boom")
zero: Int = 0
show = (r) => r ?> @Ok(v) => writeLine("Ok(${v})"), @Err(e) => writeLine("Err(${e})")
show(map(ok, (v) => v + 1))
show(map(err, (v) => v + 1))
show(andThen(ok, (v) => @Ok(v + 1)))
show(andThen(err, (v) => @Ok(v + 1)))
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
        "Ok(8)\nErr(boom)\nOk(8)\nErr(boom)\n7\n0\ntrue\nfalse\nfalse\ntrue\n"
    );
}

#[test]
fn std_map_helpers_check_and_run() {
    let dir = TempDir::new("std-map");
    let entry = dir.write(
        "main.av",
        r##"{ isEmpty, getOr, update, fromEntries, toEntries, mapValues, filter } = import("std/map")
empty: Map(Text, Int) = Map.empty()
entries: Array((Text, Int)) = [("one", 1), ("two", 2), ("one", 3)]
from: Map(Text, Int) = fromEntries(entries)
updated: Map(Text, Int) = update(from, "two", (n) => n + 10)
unchanged: Map(Text, Int) = update(from, "missing", (n) => n + 10)
mapped: Map(Text, Text) = mapValues(from, (n) => "#${n}")
filtered: Map(Text, Int) = filter(updated, (key, n) => key == "two" && n > 10)
writeLine("${isEmpty(empty)} ${getOr(empty, "missing", 99)} ${getOr(from, "one", 0)}")
writeLine("${toEntries(updated)}")
writeLine("${toEntries(unchanged)}")
writeLine("${toEntries(mapped)}")
writeLine("${toEntries(filtered)}")
"##,
    );
    let path = entry.to_str().expect("temp path is UTF-8");
    let checked = aven(&["check", path]);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let output = aven(&["run", path]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "true 99 3\n[(\"one\", 3), (\"two\", 12)]\n[(\"one\", 3), (\"two\", 2)]\n[(\"one\", \"#3\"), (\"two\", \"#2\")]\n[(\"two\", 12)]\n"
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
