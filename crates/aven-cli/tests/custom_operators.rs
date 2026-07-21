use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST: &str = "[operators]\n\"**\" = { precedence = \"^\", associativity = \"right\" }\n";

#[test]
fn manifest_declared_custom_infix_checks_and_runs() {
    let dir = TempDir::new("manifest");
    dir.write("Aven.toml", MANIFEST);
    let entry = dir.write("main.av", &operator_program(""));

    let checked = aven(["check"], &entry);
    assert_success(&checked);
    assert!(
        stdout(&checked)
            .contains("ok (parse, name, annotation, and partial monomorphic inference checks)"),
        "expected successful check output, got:\n{}",
        stdout(&checked)
    );

    let ran = aven(["run"], &entry);
    assert_success(&ran);
    assert_eq!(stdout(&ran), "8.0\n");
}

#[test]
fn portable_and_absolute_shebang_fixities_check_and_run_without_a_manifest() {
    for (label, shebang) in [
        (
            "env-s",
            "#!/usr/bin/env -S aven run --operator=**:^:right\n",
        ),
        ("absolute", "#!/abs/path/aven run --operator=**:^:right\n"),
    ] {
        let dir = TempDir::new(label);
        let entry = dir.write("script.av", &operator_program(shebang));

        let checked = aven(["check"], &entry);
        assert_success(&checked);

        let ran = aven(["run"], &entry);
        assert_success(&ran);
        assert_eq!(stdout(&ran), "8.0\n", "shebang form: {shebang}");
    }
}

#[test]
fn command_line_operator_fixity_checks_and_runs_without_other_configuration() {
    let dir = TempDir::new("argv");
    let entry = dir.write("script.av", &operator_program(""));

    let checked = aven(["check", "--operator=**:^:right"], &entry);
    assert_success(&checked);

    let ran = aven(["run", "--operator=**:^:right"], &entry);
    assert_success(&ran);
    assert_eq!(stdout(&ran), "8.0\n");
}

#[test]
fn manifest_and_shebang_conflict_reports_both_origins_before_parsing() {
    let dir = TempDir::new("conflict");
    let manifest = dir.write("Aven.toml", MANIFEST);
    let entry = dir.write(
        "main.av",
        concat!(
            "#!/usr/bin/env -S aven run --operator=**:^:right\n",
            "value = )\n",
        ),
    );

    let output = aven(["check"], &entry);

    assert_failure(&output);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("config.operator-fixity-conflict"),
        "expected config conflict, got:\n{stderr}"
    );
    assert!(
        stderr.contains(&manifest.display().to_string()),
        "expected manifest origin, got:\n{stderr}"
    );
    assert!(
        stderr.contains("first-line shebang"),
        "expected shebang origin, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("parse.unexpected-delimiter"),
        "configuration must fail before parsing, got:\n{stderr}"
    );
}

#[test]
fn fmt_round_trips_configured_custom_infix_and_preserves_the_shebang() {
    let dir = TempDir::new("fmt");
    let shebang = "#!/usr/bin/env -S aven run --operator=**:^:right";
    let entry = dir.write("script.av", &format!("{shebang}\nanswer=left ** right\n"));

    let first = aven(["fmt"], &entry);
    assert_success(&first);
    let formatted = fs::read_to_string(&entry).expect("formatted source should be readable");
    assert_eq!(formatted.lines().next(), Some(shebang));
    assert!(
        formatted.contains("answer = left ** right"),
        "custom infix should survive formatting:\n{formatted}"
    );

    let checked = aven(["fmt", "--check"], &entry);
    assert_success(&checked);
    let second = aven(["fmt"], &entry);
    assert_success(&second);
    assert_eq!(
        fs::read_to_string(&entry).expect("formatted source should remain readable"),
        formatted
    );
}

#[test]
fn configuration_diagnostics_render_against_their_own_sources() {
    let manifest_dir = TempDir::new("manifest-diagnostic");
    let manifest = manifest_dir.write(
        "Aven.toml",
        "[operators]\n\"**\" = { precedence = \"tight\", associativity = \"right\" }\n",
    );
    let manifest_entry = manifest_dir.write("main.av", "value = 1\n");
    let manifest_output = aven(["check"], &manifest_entry);
    assert_failure(&manifest_output);
    let manifest_stderr = stderr(&manifest_output);
    assert!(
        manifest_stderr.contains(&manifest.display().to_string())
            && manifest_stderr.contains("tight"),
        "manifest diagnostic should render against Aven.toml:\n{manifest_stderr}"
    );

    let shebang_dir = TempDir::new("shebang-diagnostic");
    let shebang_entry = shebang_dir.write(
        "script.av",
        "#!/usr/bin/env -S aven run --operator=**:^:sideways\nvalue = 1\n",
    );
    let shebang_output = aven(["check"], &shebang_entry);
    assert_failure(&shebang_output);
    let shebang_stderr = stderr(&shebang_output);
    assert!(
        shebang_stderr.contains(&shebang_entry.display().to_string())
            && shebang_stderr.contains("sideways"),
        "shebang diagnostic should render against the entry:\n{shebang_stderr}"
    );

    let argv_dir = TempDir::new("argv-diagnostic");
    let argv_entry = argv_dir.write("script.av", "value = 1\n");
    let argv_output = aven(["check", "--operator=**:tight:right"], &argv_entry);
    assert_failure(&argv_output);
    let argv_stderr = stderr(&argv_output);
    assert!(
        argv_stderr.contains("<command-line operator declaration 1>")
            && argv_stderr.contains("tight"),
        "argv diagnostic should render against its atom, not the entry:\n{argv_stderr}"
    );
}

fn operator_program(first_line: &str) -> String {
    format!(
        concat!(
            "{}",
            "Scalar = {{\n",
            "  value: Float\n",
            "  **(other: Scalar): Scalar =>\n",
            "    Scalar({{ value: .value ^ other.value }})\n",
            "}}\n",
            "left = Scalar({{ value: 2.0 }})\n",
            "right = Scalar({{ value: 3.0 }})\n",
            "(left ** right).value\n",
        ),
        first_line
    )
}

fn aven<const N: usize>(args: [&str; N], path: &Path) -> Output {
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
            "aven-custom-operators-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temp directory");
        Self { path }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create parent directory");
        }
        fs::write(&path, source).expect("failed to write test fixture");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
