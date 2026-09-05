use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn script_arguments_check_and_run() {
    let script = Script::new("writeLine(programName)\nwriteLine(\"${args}\")\n");
    let checked = script.aven(&["check"], &[]);
    assert_success(&checked);
    for arguments in [vec![], vec!["--", "--verbose", "two words", "", "--", "-x"]] {
        let output = script.aven(&["run"], &arguments);
        assert_success(&output);
        let expected = if arguments.is_empty() {
            "tool.av\n[]\n"
        } else {
            "tool.av\n[--verbose, two words, , --, -x]\n"
        };
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    }
}

#[test]
fn shebang_arguments_reach_the_script() {
    // Linux passes a direct interpreter's optional shebang argument as one
    // blob; env -S passes its words separately. Exercise both real CLI paths.
    for (shebang, prefix) in [
        (
            "#!/opt/aven/bin/aven run --operator=**:^:right",
            vec!["run --operator=**:^:right"],
        ),
        (
            "#!/usr/bin/env -S aven run --operator=**:^:right",
            vec!["run", "--operator=**:^:right"],
        ),
    ] {
        let script = Script::new(&format!("{shebang}\nwriteLine(\"${{args}}\")\n"));
        let output = script.aven(&prefix, &["--verbose", "a b", "--format", "fish"]);
        assert_success(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "[--verbose, a b, --format, fish]\n"
        );
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Script {
    path: PathBuf,
}

impl Script {
    fn new(source: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("aven-cli-args-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).expect("create script directory");
        let path = dir.join("tool.av");
        fs::write(&path, source).expect("write script");
        Self { path }
    }

    fn aven(&self, prefix: &[&str], arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aven"))
            .args(prefix)
            .arg(&self.path)
            .args(arguments)
            .output()
            .expect("run aven")
    }
}

impl Drop for Script {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path.parent().expect("script directory"));
    }
}
