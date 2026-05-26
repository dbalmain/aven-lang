# Tooling-First Implementation Plan

Aven should have useful tooling before it has a complete language. The first
implementation goal is a stable feedback loop:

1. read source
2. produce structured diagnostics
3. render those diagnostics in the CLI
4. publish the same diagnostics through LSP
5. format source predictably

The parser, type checker, formatter, and LSP should grow behind this loop
without changing the user-facing commands every milestone.

## Principles

- Keep structured data internal. CLI output, LSP diagnostics, and future JSON
  output should all be renderings of `aven-core` data, not separate diagnostic
  systems.
- Prefer recovery over early aborts. Editors and AI agents need multiple
  diagnostics from one run.
- Parse enough to preserve intent. Placeholder AST nodes are fine early, but
  they must not silently accept unsupported syntax.
- Keep source mapping precise from day one. Spans, file IDs, and line/column
  conversion are not optional tooling features.
- Optimize for reviewability. Each milestone should produce a small visible
  improvement and a focused test surface.

## Current Baseline

The repository currently has:

- `aven-core` with `Span`, `SourceMap`, and structured diagnostics
- `aven-parser` with a line-oriented starter parser
- `aven-fmt` with a minimal whitespace formatter
- `aven-lsp` with diagnostics and document formatting
- `aven` CLI with `check`, `fmt`, and `lsp`

This is enough to exercise the toolchain shape, but it is not yet a real
language parser. `aven check` currently validates only lexical and starter
parse structure, not name resolution, types, or runtime semantics.

## Library Direction

Use three layers:

- `chumsky` for lexing/parsing and error recovery
- `ariadne` for terminal diagnostic rendering
- `aven-core` as the stable internal diagnostic/source model

`chumsky` and `ariadne` should not leak through public crate boundaries unless
there is a strong reason. A future parser replacement should not force a rewrite
of the CLI, LSP, formatter, or test harness.

Current starting versions to evaluate:

- `chumsky = "1.0.0-alpha.8"`
- `ariadne = "0.6.0"`

These are pragmatic starting choices, not permanent architecture. `chumsky`
has useful recovery and parser-combinator ergonomics, but its compile times,
generic-heavy Rust errors, and alpha API stability are real risks. Pin the exact
version when adding it, keep it behind `aven-parser`, and revisit the choice
once the grammar is large enough to judge compile time and error quality.

## Milestone 0: Stabilize The Starter Loop

Status: done

Goal: make the current skeleton honest and hard to misuse.

Tasks:

- implement any LSP capability that is advertised
- reject unsupported indentation with diagnostics
- avoid parsing `==`, `:=`, `=>`, `!=`, `<=`, or `>=` as binding assignment
- state clearly that `check` is not semantic yet
- add basic parser tests for starter diagnostics
- keep `cargo fmt`, `cargo test`, and `cargo clippy` clean

Done when:

- `aven check examples/hello.av` succeeds with an explicit parse-only message
- unsupported indentation produces a diagnostic
- LSP formatting round-trips through `aven-fmt`

Formatter scope for this milestone is deliberately narrow: preserve existing
leading indentation, trim trailing whitespace, and add a final newline. It does
not understand or normalize indentation yet; real indentation-aware formatting
belongs to the lexer/layout/parser milestones.

## Milestone 1: Source And Diagnostic Infrastructure

Status: in progress

Goal: make source files and diagnostics robust enough for parser work.

Progress: fixture-based parser diagnostic assertions landed alongside
Milestone 8 setup. The "tests assert structured diagnostics, not terminal
snapshots" done-when is satisfied.

Even though incremental compilation is deferred, this milestone should make the
data-shape decisions that keep incremental tooling possible:

- diagnostics carry stable `FileId`s, not only paths or LSP URLs
- parser output is per-file and immutable after construction
- LSP stores source text, line indexes, diagnostics, and parse results keyed by
  `FileId`
- public APIs pass structured values rather than terminal-rendered strings

Tasks:

- finish `SourceMap` integration in CLI and LSP paths
- add stable `FileId` handling to parser output
- define diagnostic categories:
  - lexer
  - parser
  - formatter
  - resolver
  - type
  - runtime or interpreter
- add machine-readable diagnostic codes and keep them stable
- add optional JSON diagnostic output for AI/editor integration:

```bash
aven check --json examples/hello.av
```

- add diagnostic explanation docs and a lookup command:

```bash
aven explain parse.unclosed-delimiter
```

  Each diagnostic code should have a short generated documentation paragraph.
  CLI diagnostics and LSP diagnostics should carry the code so humans and AI
  agents can look up the explanation.
- switch CLI rendering from `codespan-reporting` to `ariadne`
- keep LSP rendering independent of `ariadne`
- establish fixture assertions for structured diagnostic data: code, severity,
  span, primary message, and notes. Do not snapshot colored terminal output.

Done when:

- the same internal diagnostic can render as terminal output, JSON, and LSP
- tests assert structured diagnostics, not terminal snapshots
- multi-file source IDs are represented even if imports are not implemented yet

## Milestone 2: Lexer

Status: later

Goal: replace ad hoc string scanning with one tokenization pass.

Layout is part of the token design. The raw lexer should emit newlines,
indentation widths, comments/doc comments, and trivia spans. A small `layout`
module can then convert that token stream into parser-facing `Newline`,
`Indent`, and `Dedent` events. Avoid a tokenless indentation pass; it is harder
to test and tends to duplicate lexer logic.

Tasks:

- add token types for:
  - identifiers
  - uppercase comptime identifiers
  - labels and label paths
  - strings
  - regex literals
  - path literals
  - numbers
  - operators
  - delimiters
  - newlines and indentation markers
  - comments and doc comments
- decide whether comments are trivia on tokens or separate trivia records
- handle cross-platform source trivia:
  - treat `\r\n`, `\n`, and `\r` as logical newlines
  - make formatter output use `\n`
  - reject a leading UTF-8 BOM with a diagnostic for v0
- implement string interpolation token boundaries without fully parsing
  interpolated expressions yet
- implement regex/path disambiguation early enough to avoid syntax debt
- add lexical recovery for unterminated strings, regexes, and delimiters
- emit tokens with spans and trivia spans

Recommended approach:

- use `chumsky` for the lexer if it stays simple and readable
- keep the token representation owned by `aven-parser`
- expose a debug CLI command once useful:

```bash
aven tokens examples/hello.av
```

Done when:

- parser delimiter/comment/string scanning has no duplicated logic
- lexer diagnostics flow through CLI and LSP
- tests cover strings, regexes, paths, labels, comments, and operators

## Milestone 3: Layout And Blocks

Status: later

Goal: support meaningful whitespace before deep expression parsing.

Tasks:

- convert lexer indentation/newline tokens into parser-facing `Indent`,
  `Dedent`, and `Newline` events
- define tab handling; likely reject tabs in indentation for v0
- recover from inconsistent indentation
- parse top-level blocks as a sequence of block items
- distinguish:
  - binding
  - spread binding
  - expression item
  - final expression
- add diagnostics for non-final non-`Unit` expressions later when type checking
  exists

Done when:

- multi-line functions can be parsed structurally
- indented blocks are not silently ignored
- formatter can preserve or normalize simple block indentation

## Milestone 4a: Core Expression Parser

Status: later

Goal: replace source-slice expressions with the smallest real AST that supports
ordinary scripts and editor recovery.

Tasks:

- replace `Expr { text, span }` with real expression variants
- parse:
  - literals
  - names
  - function calls
  - lambdas
- parse incomplete syntax for editor support
- return partial ASTs with diagnostics

Done when:

- `aven check` can parse bindings, literals, calls, and lambdas into AST nodes
- syntax errors still produce a partial module
- LSP diagnostics remain responsive on incomplete files

## Milestone 4b: Structural Collections

Status: later

Goal: parse the data shapes that make Aven useful as a glue language.

Tasks:

- parse:
  - records
  - arrays
  - variants and sets
  - tuples
- represent record entries explicitly:
  - field definitions
  - spreads
  - overwrite spreads
  - deletes
  - renames
  - picks
- preserve enough delimiter and trivia information for later formatting

Done when:

- representative record, array, set, tuple, and variant examples parse
- unsupported record-transform forms get honest diagnostics

## Milestone 4c: Operators, Access, And Branching Forms

Status: later

Goal: parse the expression syntax that controls execution order.

Tasks:

- parse:
  - pipelines
  - field access
  - `?` match expressions
  - `?^`, `?!`, `?.`, `??`
- add Pratt parsing for operators
- keep operator declarations out of expression parsing until the module-level
  operator model is stable

Recommended approach:

- use `chumsky` for parser combinators and recovery
- use its Pratt support for expression precedence
- keep parser modules split by syntactic family:
  - `lexer`
  - `layout`
  - `expr`
  - `pattern`
  - `type_expr`
  - `module`
- decide before deep formatter work whether the parser produces:
  - a CST/token tree plus AST view, or
  - AST only with attached trivia

  Bias toward a CST/token tree plus AST view if formatter quality matters. An
  AST-only parser makes comment and blank-line preservation harder.

Done when:

- representative pipeline, access, operator, and `?`-family examples parse
- operator precedence is tested with fixtures

## Milestone 4d: Type Syntax Parser

Status: later

Goal: parse type annotations without implementing full type inference yet.

Tasks:

- parse binding and argument type annotations
- parse primitive, function, tuple, array, record, variant, nullable, and
  singleton-marker type syntax
- parse requirement/interface headers only as syntax if needed for examples
- keep semantic validation for Milestone 7

Done when:

- parser fixtures cover the type syntax used in the language spec
- invalid type syntax produces structured parser diagnostics

## Milestone 5: Formatter

Status: later

Goal: make formatting useful before semantics are complete.

Tasks:

- format from AST where possible
- preserve comments through trivia
- define stable formatting for:
  - bindings
  - function signatures
  - records and record transforms
  - arrays
  - match expressions
  - pipelines
- add idempotence tests:

```text
format(format(source)) == format(source)
```

- add `aven fmt --check` tests
- wire LSP formatting to the same formatter

Done when:

- common examples can be formatted without losing comments
- formatting unsupported syntax reports a diagnostic instead of rewriting badly

Note: full AST-driven formatting needs comment and blank-line preservation.
Before expanding this milestone, confirm the Milestone 4 parser has either a
CST/token tree or a deliberate trivia attachment model. Otherwise this milestone
will force a parser rewrite.

## Milestone 6: Name Resolution Skeleton

Status: later

Goal: enable editor features before full type inference.

Tasks:

- collect declarations per module
- detect duplicate bindings and accidental shadowing
- resolve references to local bindings
- support uppercase/lowercase phase diagnostics at a shallow level
- add unused-binding diagnostics where safe
- expose LSP:
  - document symbols
  - go-to definition for local bindings
  - rename for local bindings

Done when:

- local scripts get useful name diagnostics
- basic editor navigation works before type checking

## Milestone 7: Type Skeleton

Status: later

Goal: introduce types in a way that powers tooling early.

Tasks:

- define type AST
- parse type annotations
- add primitive types and function types
- add typed hover output for annotated bindings
- add type diagnostic plumbing before full inference
- create fixtures for expected type diagnostics

Done when:

- LSP hover can show parsed/known types
- invalid type syntax and unknown type names have clear diagnostics

## Milestone 8: Test Harness And Fixtures

Status: in progress

Goal: make syntax and diagnostic changes reviewable.

Fixture mechanism: hand-rolled goldenfile assertions against structured
diagnostic summaries. Parser fixtures live under
`crates/aven-parser/tests/fixtures/parser/`.

Tasks:

- choose the fixture mechanism early. Either use `insta` snapshots of structured
  JSON diagnostics/AST summaries or a small hand-rolled goldenfile assertion.
  Start this style before parser tests grow large.
- add fixture-driven tests:

```text
tests/fixtures/parser/valid/*.av
tests/fixtures/parser/invalid/*.av
tests/fixtures/formatter/*.av
```

- assert diagnostic code, span, and message summary
- avoid snapshotting full colored output
- add CLI smoke tests with `assert_cmd` once CLI behavior settles
- add a small LSP protocol smoke test when server behavior grows

Done when:

- syntax changes require updating intentional fixtures
- diagnostics are stable enough for AI agents to rely on codes

## Milestone 9: Incremental Tooling

Status: later

Goal: prepare for fast editor feedback.

Tasks:

- store per-file parse results in the LSP backend
- separate cheap lexer/parser diagnostics from expensive semantic diagnostics
- cache line indexes for span conversion
- add debounce/cancellation around document changes
- design the compiler database interface before imports/type inference

Done when:

- LSP does not rederive every intermediate artifact by hand
- parse/check latency is visible and measurable

## Phase 2 Scope

Status: later

This plan is Phase 1: the toolchain skeleton. It deliberately stops before the
hard semantic system.

Phase 2 work not planned here:

- Hindley-Milner inference
- row-polymorphic record and variant solving
- comptime evaluation
- requirement/interface resolution
- opaque types
- modules and imports
- package management
- bytecode/runtime execution

Those systems should plug into the same source, diagnostic, fixture, CLI, and
LSP infrastructure built in Phase 1.

## Review Gates

Before each commit:

- `cargo fmt --all --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- smoke-test the touched CLI command
- confirm no advertised LSP capability lacks an implementation
- confirm unsupported syntax gives an honest diagnostic

Before parser milestones:

- list syntax examples being targeted
- list syntax examples explicitly not supported yet
- add diagnostics for the unsupported examples if they could be mistaken for
  accepted code

## Near-Term Order

The next few queued changes should be:

1. commit the starter honesty fixes from Milestone 0
2. add `ariadne` and replace CLI diagnostic rendering
3. add fixture tests for structured diagnostics
4. add `chumsky` and implement a token lexer with newline/indent trivia
5. add the layout module that emits `Indent`/`Dedent`/`Newline`
6. replace the line parser with Milestone 4a: bindings, literals, calls, and
   lambdas
7. add Milestone 4b collections: arrays, records, sets, variants, and tuples
8. expand LSP from diagnostics/formatting to document symbols

This keeps tooling ahead of semantics without spending too long on temporary
parser code.
