# Claude handoff — fixes only

Please implement the fixes below. Language proposals are deferred for the user's
review: do not add guard/return syntax, change shadowing semantics, implement new
string literal forms, or expand comptime behavior as part of this task.

Read `/home/dave/w/clex/.ai/core.md` first. Review evidence was collected at
`12547d7`; check current code before changing it. Never push.

## 1. Bash completion: skip redirections when reconstructing arguments

Source: `crates/aven-host/std/cli.av`, `bashStartLines` and its generated scanner.
Use the existing `completion_tool.av` fixture and real Readline harness in
`crates/aven-cli/tests/cli_args.rs`.

Confirmed failure through real interactive Readline, TAB at the end:

```bash
tool add >out --
```

Expected: `--verbose`, `--chatty`, `--quiet`, `--out`, `--output`.
Actual: no candidates. The scanner treats `>out` as an argv element.

Recognize redirection syntax and exclude operators and their operands from argv.
Preserve quoted or escaped literal metacharacters. Cover attached and separated
operands and file-descriptor prefixes with focused tests. If a construct cannot
be handled safely, use a conservative explicit boundary instead of executing it.

## 2. Bash completion: handle ANSI-C quoting

Confirmed failure through real interactive Readline:

```bash
tool $'add' --
```

Expected: the same options as `tool add --`. Actual: no candidates.
The scanner appends `$` literally and then uses ordinary single-quote handling,
producing `$add`.

Implement the applicable ANSI-C escape semantics; merely dropping `$` is
insufficient. Verify literal and escaped command spellings and incomplete input.
Never use `eval` on command-line text.

Reference: https://www.gnu.org/software/bash/manual/html_node/ANSI_002dC-Quoting.html

## 3. Bash completion: remove phantom words from line continuations

Confirmed by direct invocation of the generated callback. Establish end-to-end
coverage through multiline Readline before treating these as interactive
regressions. Both examples contain a physical backslash followed by a newline.

```bash
tool add --out \
 --ver
```

Actual callback result: `--verbose`. Expected: no candidates, because `--ver`
occupies the pending `--out` value position.

```bash
tool \
 add --
```

Actual callback result: no candidates. Expected: add's options.

Cause: the backslash sets `started=1`; the newline is discarded; the next space
emits a phantom empty word. A removed continuation must not itself start a word.
Preserve genuine empty arguments from `''` and `""`, and continuations within
existing words and double quotes.

## 4. Correct the unavailable-syntax recommendation

At the reviewed commit, `push_unterminated_string` in
`crates/aven-parser/src/lexer.rs` recommends raw strings for multiline content,
but raw and triple-quoted strings are not implemented by the lexer.

Recheck current support. If still unavailable, change the diagnostic to recommend
a supported remedy. Do not implement new literal syntax in this fixes-only task.
Update affected diagnostic expectations.

## Investigate, but do not expand scope automatically

This exact input was not tested during review:

```bash
tool add --out $(printf "value") --
```

It produces a single output value when executed. Static inspection predicts that
the scanner splits the substitution's source at its internal space and rejects
the resulting tokens. Reproduce it using real Readline. Decide and document a
conservative policy for unresolved expansions rather than trying to recover
arbitrary expanded argv. Do not execute substitutions during completion.

The tested `$(printf "two words")` variant returned no candidates, but its
unquoted expansion also introduces an unsupported positional argument. It is
not evidence of an observable completion failure.

## Validation and boundaries

- `:; tool add --` and `true && tool add --` both work through real Readline.
  Do not fix purported separator failures inferred from direct callback calls.
- Preserve the existing shell matrix, hostile-name/description escaping, empty
  values, attached `=`, aliases, duplicate detection, pending-value handling,
  and disabled filename fallback.
- Keep completion generation on the explicit runtime route. TAB never starts
  Aven. No value domains, positionals, or short bundles in this slice.
- Keep focused behavior coverage in the existing harness. Verify supported Bash
  versions where available and report any version coverage not run.
- Run appropriate tests and required formatting/lint/workspace gates after the
  fixes. Report actual results, remaining limitations, and any unconfirmed case.
- The broader proposals in `cli-language-priority-handoff.md` are background,
  not instructions to implement language changes now.
