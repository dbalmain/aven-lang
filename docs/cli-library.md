# CLI arguments

`std/cli` is written in Aven. A typed decoder determines each option's output
type; a record comprehension preserves those types in the parsed record.

```aven
cli = import("std/cli")

spec = cli.define({
  verbose: cli.flag({ short: "v", aliases: ["--chatty"], help: "Print each step" })
  jobs: cli.option(cli.int, { default: 1, short: "j", help: "Parallel jobs" })
  out: cli.required(cli.text, { help: "Output file", valueName: "PATH" })
}, { name: "tool", description: "Process files", examples: ["tool -j 4 --out=result"] })

parsed = cli.parse(spec, args)?^
writeLine("jobs=${parsed.jobs}, verbose=${parsed.verbose}, out=${parsed.out}")
```

Run with `aven run tool.av -- -v -j 4 --out=result`. `parsed.jobs` is `Int`,
`parsed.verbose` is `Bool`, and `parsed.out` is `Text`. Misspelled fields and
incompatible defaults are checker errors. A user decoder has type
`(Text) -> Result(a, Text)`; it needs no Rust registration.

`cli.define` prepares metadata and captures its key set once. Reusing the spec
does not rebuild metadata or recompute `keysOf` in each parser call.

Long and short aliases accept separate or attached values (`--jobs=4`, `-j=4`).
An option consumes its next token even when that token resembles a flag; an
attached empty value stays empty. Repeated arguments are errors. A final `--`
is accepted, and tokens following it are positional input. Positionals and
short bundles such as `-abc` are currently rejected.

`cli.help(spec)` returns usage, descriptions, aliases, value labels, and examples.
`cli.parse` returns `Result(args, Text)`; error text includes usage and examples.
Programs currently handle explicit help requests and choose how to print errors.
An `Int` entry value selects the process exit code, so usage failures can exit 2.

## Commands

```aven
cli = import("std/cli")
add = cli.define({ path: cli.required(cli.text) }, { name: "tool add" })
commit = cli.define({ jobs: cli.option(cli.int, { default: 1 }) }, { name: "tool commit" })

tool = cli.app({
  add: cli.command(add, (a) => @Add(a), { aliases: ["a"], help: "Add a file" })
  commit: cli.command(commit, (a) => @Commit(a), { help: "Commit changes" })
}, { name: "tool" })

cli.parse(tool, args)?^ ?>
  @Add(a) => writeLine(a.path)
  @Commit(c) => writeLine("jobs=${c.jobs}")
```

The result is the closed variant `@Add({ path: Text }) | @Commit({ jobs: Int })`.
The checker enforces exhaustive handling and command-specific field access.
The constructor callback supplies a tag explicitly; its input type is inferred
from the child parser. Command aliases are plain words. A child can itself be
an app, producing a nested variant. `cli.help(tool, ["add"])` selects child help;
parse errors also use the deepest matched command's usage.

## Shell completions

`cli.completions(spec, shell, program)` returns a completion script as `Text`
for `"bash"` or `"fish"`. `program` is the installed executable name, which is
independent of the `name` used in help output.

```aven
cli = import("std/cli")

tool = cli.app({ ... }, { name: "tool" })

args[0] ?>
  "completions" => args[1] ?>
    "bash" => writeLine(cli.completions(tool, "bash", "tool"))
    _ => writeLine(cli.completions(tool, "fish", "tool"))
  _ => run(cli.parse(tool, args)?^)
```

`shell` is the literal union `"bash" | "fish"`, so it takes a literal at the
call site rather than a `Text` read out of `args`.

Install it once — `tool completions fish > ~/.config/fish/completions/tool.fish`
— and the shell handles TAB itself. Completion never starts Aven.

The script follows the parser rather than approximating it: command aliases
select the same options, an option's value is consumed even when it looks like a
command or a flag, an option already supplied is not offered again under any of
its spellings, and nothing is offered after `--`. Fish completions carry each
option's help text as its description.

Option names are completed; option *values* are not. A decoder says that a value
is required, not what the valid values are, so the generator offers nothing
there rather than guessing — and it disables the shell's filename fallback, so a
value position stays empty instead of silently suggesting local files.
`Meta.valueCompletion` is the seam where explicit value domains would attach.

Fish declares short spellings with `-o`, because the parser accepts neither
bundles (`-vj`) nor attached short values (`-j3`); `-s` would advertise
completions the parser then rejects. A program name that cannot be written
unquoted reaches only fish: bash resolves a completion spec by the command word
exactly as typed and never removes quotes, so `'tool' --` matches no spec even
when the program really is `tool`.

The bash script reads the command line itself rather than trusting `COMP_WORDS`,
which splits on `=`. It reads only literal text: redirections and their operands
are skipped, `$'…'` is decoded, and a `$(…)` or `${…}` is kept whole as one word
and never run. Nested expansions retain their own quote context, including when
the outer expansion is double-quoted; an incomplete expansion offers nothing.
ANSI-C NUL escapes truncate only their quoted segment, preserving concatenated
text after its closing quote. An unresolved expansion can fill a value position
but never matches an option or command name. Two shapes are bash's own limits, not the
generator's: bash truncates the line at an `&` or an unquoted backtick, so it
never calls the completion for `tool add 2>&1 --`, and a continuation typed
across two lines starts a fresh completion context on the second (pasting the
same command works, because that keeps it one line).
