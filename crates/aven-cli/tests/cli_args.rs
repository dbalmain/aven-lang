use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn cli_library_aven_suite_checks_and_runs() {
    for suite in [
        include_str!("fixtures/cli/parse.av"),
        include_str!("fixtures/cli/commands.av"),
        include_str!("fixtures/cli/completions.av"),
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
        "cli.completions(spec, \"zsh\", \"tool\")\n",
    ] {
        let script = Script::new(&format!(
            "cli = import(\"std/cli\")\n\
             spec = cli.define({{ verbose: cli.flag(), jobs: cli.option(cli.int, {{ default: 1 }}) }})\n{tail}"
        ));
        let output = script.aven(&["check"], &[]);
        assert!(!output.status.success(), "unexpectedly checked: {tail}");
    }
}

// The matrix describes the CLI grammar, not the generator's string layout.
// Both shells execute the output of the real Aven library. Bash is driven by
// Readline TAB presses so the test cannot fake away COMP_WORDBREAKS or quoting.
#[test]
fn generated_completions_follow_the_parser_in_real_shells() {
    let add: &[&str] = &["--chatty", "--out", "--output", "--quiet", "--verbose"];
    let after_out: &[&str] = &["--chatty", "--quiet", "--verbose"];
    let cases: &[(&str, &[&str])] = &[
        (
            "tool ",
            &[
                "a",
                "add",
                "branch",
                "commit",
                "custom",
                "empty",
                "odd",
                "r",
                "remote",
                "we'ird$(touch INJECTED)",
            ],
        ),
        ("tool --", &[]),
        ("tool comm", &["commit"]),
        ("tool add --", add),
        ("tool a --", add),
        ("tool commit --", &["--jobs"]),
        ("tool remote ", &["add"]),
        ("tool remote add --", &["--force"]),
        ("tool r add --", &["--force"]),
        ("tool odd --", &["--safe"]),
        ("tool 'we'\\''ird$(touch INJECTED)' --", &["--safe"]),
        ("tool empty ", &[]),
        ("tool branch ", &[]),
        ("tool custom --", &[]),
        ("tool add --out commit --", after_out),
        ("tool add --out ", &[]),
        ("tool add --out --", &[]),
        ("tool add --out=", &[]),
        ("tool add --out=a=b --", after_out),
        ("tool add --out= --", after_out),
        ("tool add --out = --", after_out),
        ("tool add -o --chatty --", after_out),
        ("tool add --output out --", after_out),
        ("tool add --chatty --", &["--out", "--output", "--quiet"]),
        ("tool add -v --chatty --", &[]),
        ("tool add --out=x --output=y --", &[]),
        ("tool add --verbose=false --", &[]),
        ("tool add --unknown --", &[]),
        ("tool add -- ", &[]),
        ("tool add -- --ver", &[]),
        ("tool add -v", &["-v"]),
        ("tool add -vq", &[]),
        ("tool add -ostuff --", &[]),
        ("tool 'add' --out 'two words' --", after_out),
        ("tool add --out two\\ words --", after_out),
        ("tool add --out \"two \\\"words\\\"\" --", after_out),
        ("tool add --out '' --", after_out),
        ("tool add --out \"\" --", after_out),
        ("tool add --ver", &["--verbose"]),
        (
            "tool add --out --chatty --quiet --",
            &["--chatty", "--verbose"],
        ),
        ("tool add --jobs=2 --", &[]),
        ("tool commit --jobs -2 --", &[]),
        ("tool completions ", &[]),
    ];
    for shell in ["fish", "bash"] {
        let fixture = CompletionFixture::new(shell, "tool");
        let inputs: Vec<_> = cases.iter().map(|(input, _)| *input).collect();
        let actual = fixture.query(shell, "tool", &inputs);
        for ((input, expected), mut found) in cases.iter().zip(actual) {
            found.sort();
            let mut expected: Vec<_> = expected.iter().map(|s| (*s).to_owned()).collect();
            expected.sort();
            assert_eq!(found, expected, "{shell}: {input}");
        }
        assert!(!fixture.directory().join("INJECTED").exists());
        assert!(!fixture.directory().join("TOOL_RAN").exists());
    }
}

#[test]
fn fish_completion_descriptions_preserve_shell_metacharacters() {
    let fixture = CompletionFixture::new("fish", "tool");
    let output = fixture.fish_query("tool add --out");
    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("--out\tDave's $HOME `quoted` $(touch INJECTED) \\ destination")
    );
    assert!(!fixture.directory().join("INJECTED").exists());
}

#[test]
fn completion_program_names_are_literal_and_registrations_coexist() {
    for shell in ["fish", "bash"] {
        let first = CompletionFixture::new(shell, "tool-name");
        let second = CompletionFixture::from_source(
            shell,
            "tool_name",
            include_str!("fixtures/cli/completion_leaf.av"),
        );
        let odd_program = "tool_'$(touch INJECTED)_文";
        let odd = CompletionFixture::from_source(
            shell,
            odd_program,
            include_str!("fixtures/cli/completion_leaf.av"),
        );
        let combined = [&first, &second, &odd]
            .iter()
            .map(|f| fs::read_to_string(&f.generated).expect("read generated script"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&first.generated, combined).expect("combine registrations");
        for (program, input, expected) in [
            ("tool-name", "tool-name commit --", "--jobs"),
            ("tool_name", "tool_name --", "--other"),
        ] {
            assert_eq!(
                first.query(shell, program, &[input]),
                vec![vec![expected.to_owned()]],
                "{shell}: {program}"
            );
        }
        // A name that cannot be written unquoted reaches only fish. Bash looks a
        // `complete` spec up by the command word exactly as typed and never
        // removes quotes, so even `'tool-name' --` finds no spec; there the
        // generator's obligation is the registration itself, which is what a
        // mangled or globbed name would break. Verified against bash 5.3.9 and
        // fish 4.7.1.
        if shell == "fish" {
            assert_eq!(
                first.query(shell, odd_program, &["'tool_'\\''$(touch INJECTED)_文' --"]),
                vec![vec!["--other".to_owned()]],
                "fish: {odd_program}"
            );
        } else {
            // `complete -p -- <name>` resolves the spec bash holds for exactly
            // that name, so it fails outright on a globbed or mangled
            // registration. Bash prints the name back in its own quoted form,
            // which is why the spec is not compared to the raw spelling.
            let registered = first.bash_registration(odd_program);
            assert_success(&registered);
            let registered = String::from_utf8(registered.stdout).expect("bash UTF-8");
            assert!(
                registered.starts_with("complete -F "),
                "bash did not register a function for the literal name: {registered}"
            );
        }
        assert!(!first.directory().join("INJECTED").exists());
    }
}

struct CompletionFixture {
    script: Script,
    generated: PathBuf,
}

impl CompletionFixture {
    fn new(shell: &str, program: &str) -> Self {
        Self::from_source(
            shell,
            program,
            include_str!("fixtures/cli/completion_tool.av"),
        )
    }

    fn from_source(shell: &str, program: &str, source: &str) -> Self {
        // Missing shells are hard failures, including on developer machines.
        let version = Command::new(shell)
            .arg("--version")
            .output()
            .unwrap_or_else(|error| panic!("required completion test shell {shell}: {error}"));
        assert_success(&version);
        let script = Script::new(source);
        let generated = script.path.with_extension(shell);
        let output = script.aven(&["run"], &["--", "completions", shell, program]);
        assert_success(&output);
        fs::write(&generated, &output.stdout).expect("write generated completions");
        let fixture = Self { script, generated };
        // A missing -f must not pass merely because the working directory is empty.
        fs::write(fixture.directory().join("tempting.txt"), "file").expect("write file candidate");
        // If a completion launches the tool, leave evidence even if it ignores status.
        fs::write(
            fixture.directory().join("tool"),
            "#!/bin/sh\ntouch TOOL_RAN\nexit 99\n",
        )
        .expect("write tool spy");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                fixture.directory().join("tool"),
                fs::Permissions::from_mode(0o755),
            )
            .expect("make tool spy executable");
        }
        let checked = fixture
            .command(shell)
            .arg("-n")
            .arg(&fixture.generated)
            .output()
            .expect("check shell syntax");
        assert_success(&checked);
        fixture
    }

    fn directory(&self) -> &std::path::Path {
        self.script.path.parent().expect("fixture directory")
    }

    fn command(&self, shell: &str) -> Command {
        let mut command = Command::new(shell);
        match shell {
            "fish" => command.args(["--no-config", "--private"]),
            "bash" => command.args(["--noprofile", "--norc"]),
            _ => panic!("unexpected test shell"),
        };
        let path = std::env::join_paths(std::iter::once(self.directory().to_owned()).chain(
            std::env::split_paths(&std::env::var_os("PATH").expect("PATH")),
        ))
        .expect("test PATH");
        command
            .current_dir(self.directory())
            .env("PATH", path)
            .env("LC_ALL", "C.UTF-8")
            .env("INPUTRC", "/dev/null")
            .env("HISTFILE", "/dev/null")
            .env("TERM", "dumb")
            .env("PS1", "")
            .env("XDG_CONFIG_HOME", self.directory().join("config"))
            .env("XDG_CACHE_HOME", self.directory().join("cache"))
            .env("AVEN_COMPLETION_SCRIPT", &self.generated);
        command
    }

    /// Source the generated script and print the `complete` spec bash holds for
    /// `program`, plus proof that the named function was actually declared.
    fn bash_registration(&self, program: &str) -> Output {
        self.command("bash")
            .args([
                "-c",
                r#"source "$AVEN_COMPLETION_SCRIPT" || exit 91
spec=$(complete -p -- "$AVEN_COMPLETION_PROGRAM") || exit 92
function=${spec#* -F }
function=${function%% *}
declare -F "$function" >/dev/null || exit 93
printf '%s' "$spec""#,
            ])
            .env("AVEN_COMPLETION_PROGRAM", program)
            .output()
            .expect("read bash completion registration")
    }

    fn fish_query(&self, input: &str) -> Output {
        self.command("fish")
            .args(["-c", include_str!("fixtures/cli/complete.fish")])
            .env("AVEN_COMPLETION_INPUT", input)
            .output()
            .expect("query fish completions")
    }

    fn query(&self, shell: &str, program: &str, inputs: &[&str]) -> Vec<Vec<String>> {
        if shell == "fish" {
            return inputs
                .iter()
                .map(|input| {
                    let output = self.fish_query(input);
                    assert_success(&output);
                    assert!(
                        output.stderr.is_empty(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    String::from_utf8(output.stdout)
                        .expect("fish UTF-8")
                        .lines()
                        .map(|line| {
                            line.split_once('\t')
                                .map_or(line, |(word, _)| word)
                                .to_owned()
                        })
                        .collect()
                })
                .collect();
        }
        let mut input = include_str!("fixtures/cli/complete.bash").to_owned();
        for line in inputs {
            input.push_str(line);
            // TAB invokes the real Readline callback; Ctrl-U then clears the
            // edited line, so no candidate or test command gets executed.
            input.push_str("\t\u{15}\n");
        }
        input.push_str("exit\n");
        let mut child = self
            .command("bash")
            .arg("-i")
            .env("AVEN_COMPLETION_PROGRAM", program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start bash Readline");
        child
            .stdin
            .take()
            .expect("bash stdin")
            .write_all(input.as_bytes())
            .expect("type completion queries");
        let output = child.wait_with_output().expect("wait for bash");
        assert_success(&output);
        let decoded = String::from_utf8(output.stdout).expect("bash UTF-8");
        let mut words = decoded.split_terminator('\0');
        let result = inputs
            .iter()
            .map(|line| {
                let count: usize = words
                    .next()
                    .unwrap_or_else(|| {
                        panic!(
                            "no Readline callback for {line}: {}",
                            String::from_utf8_lossy(&output.stderr)
                        )
                    })
                    .parse()
                    .expect("completion count");
                (0..count)
                    .map(|_| words.next().expect("completion candidate").to_owned())
                    .collect()
            })
            .collect();
        assert!(words.next().is_none(), "unexpected extra Readline callback");
        result
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
