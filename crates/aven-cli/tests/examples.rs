use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const CHECK_ONLY: &[&str] = &[
    // HTTP examples require network access to run; they are verified by
    // `aven check` only so the test suite stays hermetic.
    "http-fetch.av",
    "http-post.av",
];

#[test]
fn examples_directory_is_not_empty() {
    let examples = discover_examples();
    assert!(
        !examples.is_empty(),
        "examples/ directory must contain at least one .av file"
    );
}

#[test]
fn all_examples_check_cleanly() {
    let examples = discover_examples();
    assert!(!examples.is_empty());

    for example in &examples {
        let output = run_aven_check(example, &[]);
        assert_success(
            &output,
            &format!(
                "aven check failed for {}\nstderr:\n{}",
                example.display(),
                stderr(&output)
            ),
        );
    }
}

#[test]
fn hermetic_examples_run_successfully() {
    let examples = discover_examples();

    for example in &examples {
        let name = file_name(example);
        if CHECK_ONLY.contains(&name.as_str()) {
            continue;
        }

        match name.as_str() {
            "file-pipeline.av" => run_file_pipeline(example),
            "errors.av" => run_errors(example),
            "logging.av" => run_logging(example),
            _ => run_with_expected_output(example, &[]),
        }
    }
}

fn run_file_pipeline(example: &Path) {
    let dir = TempDir::new("example-file-pipeline");
    fs::write(dir.path().join("input.txt"), "hello world").expect("failed to write input.txt");

    let output = run_aven_in_dir(example, dir.path());
    assert_success(
        &output,
        &format!(
            "aven run failed for file-pipeline.av\nstderr:\n{}",
            stderr(&output)
        ),
    );
    assert_eq!(stdout(&output), "copying...\ndone\n");

    let output_content =
        fs::read_to_string(dir.path().join("output.txt")).expect("failed to read output.txt");
    assert_eq!(output_content, "header: hello world");
}

fn run_errors(example: &Path) {
    let dir = TempDir::new("example-errors");
    fs::write(
        dir.path().join("config.json"),
        "{\"name\":\"myapp\",\"version\":\"1.0\"}",
    )
    .expect("failed to write config.json");

    let output = run_aven_in_dir(example, dir.path());
    assert_failure(
        &output,
        "errors.av should exit non-zero (the ?! panic operator fires)",
    );
    let out = stdout(&output);
    assert!(out.contains("error:"), "expected error output, got:\n{out}");
    assert!(
        out.contains("loaded:"),
        "expected loaded output, got:\n{out}"
    );
}

fn run_logging(example: &Path) {
    let output = run_aven_run(example, &[]);
    assert_success(
        &output,
        &format!(
            "aven run failed for logging.av\nstderr:\n{}",
            stderr(&output)
        ),
    );
    let out = stdout(&output);
    for line in out.lines() {
        let _: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON log: {e}\n{line}"));
    }
    assert!(
        out.contains("\"msg\":\"starting\""),
        "expected 'starting' log, got:\n{out}"
    );
    assert!(
        out.contains("\"requestId\":\"abc\""),
        "expected child logger field, got:\n{out}"
    );
}

fn run_with_expected_output(example: &Path, extra_args: &[&str]) {
    let output = run_aven_run(example, extra_args);
    assert_success(
        &output,
        &format!(
            "aven run failed for {}\nstderr:\n{}",
            example.display(),
            stderr(&output)
        ),
    );

    let name = file_name(example);
    let expected: &str = match name.as_str() {
        "hello.av" => "hello, Aven\n",
        "literal-modes.av" => "1\n",
        "records.av" => {
            "picked:\n\
            { name: \"Ada\", email: \"ada@x.dev\" }\n\
            omitted:\n\
            { name: \"Ada\", age: 36 }\n\
            deleted:\n\
            { name: \"Ada\", age: 36 }\n\
            renamed:\n\
            { fullName: \"Ada\", email: \"ada@x.dev\", age: 36 }\n\
            replaced:\n\
            { name: \"Ada\", email: \"ada@x.dev\", age: 38 }\n"
        }
        "json.av" => {
            "parsed: { name: \"Ada\", email: undefined, nick: null }\n\
            encoded: {\"name\":\"Ada\",\"nick\":null}\n"
        }
        "dynamic-json.av" => {
            "summary: object:Ada\n\
            encoded: {\"name\":\"Ada\",\"count\":3,\"nested\":{\"ok\":true},\"scores\":[1,2.5]}\n"
        }
        _ => return,
    };
    assert_eq!(stdout(&output), expected, "unexpected output for {name}");
}

fn discover_examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .parent()
        .expect("workspace root parent")
        .join("examples");

    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!(
                "failed to read examples directory at {}: {e}",
                dir.display()
            )
        })
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? == "av" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    entries.sort();
    entries
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("example path has no filename")
        .to_str()
        .expect("example filename is not valid UTF-8")
        .to_owned()
}

fn run_aven_check(path: &Path, extra_args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aven"));
    cmd.arg("check").arg(path).args(extra_args);
    cmd.output().expect("failed to run aven check")
}

fn run_aven_run(path: &Path, extra_args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aven"));
    cmd.arg("run").arg(path).args(extra_args);
    cmd.output().expect("failed to run aven run")
}

fn run_aven_in_dir(path: &Path, cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aven"))
        .arg("run")
        .arg(path)
        .current_dir(cwd)
        .output()
        .expect("failed to run aven run")
}

fn assert_success(output: &Output, message: &str) {
    assert!(output.status.success(), "{message}");
}

fn assert_failure(output: &Output, message: &str) {
    assert!(!output.status.success(), "{message}");
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
        let path = std::env::temp_dir().join(format!(
            "aven-examples-{label}-{}-{unique}",
            std::process::id()
        ));
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
