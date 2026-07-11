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
