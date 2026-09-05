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

Completion scripts are not implemented yet. The required comptime string
evaluation and constant emission are recorded in [the implementation note](cli-args-done.md).
