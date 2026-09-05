use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn cli_library_aven_suite_checks_and_runs() {
    for suite in [
        include_str!("fixtures/cli/parse.av"),
        include_str!("fixtures/cli/commands.av"),
    ] {
        let script = Script::new(suite);
        assert_success(&script.aven(&["check"], &[]));
        let tested = script.aven(&["test"], &[]);
        assert_success(&tested);
    }
}

#[test]
fn cli_library_reads_forwarded_arguments() {
    let script = Script::new(
        "cli = import(\"std/cli\")\n\
         spec = cli.define({ verbose: cli.flag(), jobs: cli.option(cli.int, { default: 1 }) })\n\
         parsed = cli.parse(spec, args)?^\n\
         writeLine(\"verbose=${parsed.verbose}; jobs=${parsed.jobs}\")\n",
    );
    assert_success(&script.aven(&["check"], &[]));
    let output = script.aven(&["run"], &["--", "--verbose", "--jobs", "3"]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "verbose=true; jobs=3\n"
    );
}

#[test]
fn cli_library_rejects_wrong_fields_types_and_argv() {
    for tail in [
        "cli.parse(spec, [])?^.jbos\n",
        "value: Int = cli.parse(spec, [])?^.verbose\n",
        "cli.parse(spec, \"--verbose\")\n",
        "cli.option(cli.int, { default: \"many\" })\n",
    ] {
        let script = Script::new(&format!(
            "cli = import(\"std/cli\")\n\
             spec = cli.define({{ verbose: cli.flag(), jobs: cli.option(cli.int, {{ default: 1 }}) }})\n{tail}"
        ));
        let output = script.aven(&["check"], &[]);
        assert!(!output.status.success(), "unexpectedly checked: {tail}");
    }
}

#[test]
fn command_handlers_are_exhaustive_and_use_their_own_args() {
    let prefix = concat!(
        "cli = import(\"std/cli\")\n",
        "add = cli.define({ path: cli.required(cli.text) })\n",
        "commit = cli.define({ jobs: cli.option(cli.int, { default: 1 }) })\n",
        "tool = cli.app({ add: cli.command(add, (a) => @Add(a)), commit: cli.command(commit, (a) => @Commit(a)) })\n",
    );
    let script = Script::new(&format!(
        "{prefix}parsed = cli.parse(tool, args)?^\nparsed ?>\n  @Add(a) => writeLine(a.path)\n  @Commit(c) => writeLine(\"jobs=${{c.jobs}}\")\n"
    ));
    assert_success(&script.aven(&["check"], &[]));
    for (args, expected) in [
        (vec!["--", "add", "--path=file"], "file\n"),
        (vec!["--", "commit", "--jobs=3"], "jobs=3\n"),
    ] {
        let ran = script.aven(&["run"], &args);
        assert_success(&ran);
        assert_eq!(String::from_utf8_lossy(&ran.stdout), expected);
    }
    for tail in [
        "cli.parse(tool, [])?^ ?> @Add(a) => a.path\n",
        "cli.parse(tool, [])?^ ?> @Add(a) => a.jobs, @Commit(c) => c.jobs\n",
        "cli.command(add, (a: { path: Int }) => @Add(a))\n",
    ] {
        let script = Script::new(&format!("{prefix}{tail}"));
        assert!(
            !script.aven(&["check"], &[]).status.success(),
            "incorrectly checked {tail}"
        );
    }
}

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
        for args in [
            vec!["--help"],
            vec!["--format", "fish"],
            vec!["--verbose", "a b", "--format", "fish"],
        ] {
            let output = script.aven(&prefix, &args);
            assert_success(&output);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("[{}]\n", args.join(", "))
            );
        }
    }
}

#[test]
fn shebang_transport_separator_preserves_every_script_argument() {
    for (shebang, prefix) in [
        ("#!/opt/aven/bin/aven run --", vec!["run --"]),
        ("#!/usr/bin/env -S aven run --", vec!["run", "--"]),
        (
            "#!/usr/bin/env -S aven run --operator=**:^:right --",
            vec!["run", "--operator=**:^:right", "--"],
        ),
    ] {
        let script = Script::new(&format!("{shebang}\nwriteLine(\"${{args}}\")\n"));
        assert_success(&script.aven(&["check"], &[]));
        let output = script.aven(&prefix, &["--", "-file", "--help", ""]);
        assert_success(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "[--, -file, --help, ]\n"
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

#[test]
fn integer_entries_choose_exit_status_without_printing() {
    for (source, code) in [("0\n", 0), ("1 + 1\n", 2), ("255\n", 255)] {
        let script = Script::new(source);
        let output = script.aven(&["run"], &[]);
        assert_eq!(output.status.code(), Some(code));
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn invalid_integer_exit_codes_are_reported_without_truncation() {
    for source in ["-1\n", "256\n", "999999999999999999999999999\n"] {
        let script = Script::new(source);
        let output = script.aven(&["run"], &[]);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("exit code must be an integer from 0 to 255")
        );
    }
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
