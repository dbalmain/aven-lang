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
- `aven-parser` with a raw lexer, layout pass, and core expression parser
- `aven-fmt` with a minimal whitespace formatter
- `aven-lsp` with diagnostics and document formatting
- `aven` CLI with `check`, `tokens`, `layout`, `fmt`, and `lsp`

This is enough to exercise the toolchain shape, but it is not yet a complete
language parser. `aven check` currently validates lexical, layout, and core
parse structure, not name resolution, types, or runtime semantics.

## Library Direction

Use three layers:

- a hand-written raw lexer for byte-accurate spans, source trivia, and layout
  inputs
- a small hand-written layout pass that turns indentation widths into block
  tokens
- a parser implementation chosen at Milestone 4a
- `ariadne` for terminal diagnostic rendering
- `aven-core` as the stable internal diagnostic/source model

Parser implementation is deliberately deferred. The default assumption is a
hand-written recursive descent parser over tokens because it keeps errors,
recovery, and layout handling explicit. Parser-combinator libraries such as
`chumsky` can still be evaluated when Milestone 4a starts, but they should not
be added as dependencies until production parser code uses them.

`ariadne` should not leak through public crate boundaries unless there is a
strong reason. A future parser replacement should not force a rewrite of the
CLI, LSP, formatter, or test harness.

Current starting versions to evaluate:

- `ariadne = "0.6.0"`

This is a pragmatic starting choice, not permanent architecture. Keep third
party parser choices behind `aven-parser` so the CLI, LSP, formatter, and test
harness do not depend on the parsing library.

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

Status: in progress

Progress: a first hand-written raw lexer emits owned tokens for names,
literals, paths, labels, operators, delimiters, newlines, indentation widths,
comments, and basic lexer diagnostics. The lexer feeds the layout pass, which
in turn feeds the Milestone 4a core parser. The starter parser has been
replaced.

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

- keep the raw lexer hand-written unless a library demonstrably improves
  clarity
- defer the parser-library decision until the Milestone 4a parser starts
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

Status: in progress

Progress: a first hand-written layout pass converts raw lexer indentation and
newline trivia into parser-facing `Indent`, `Dedent`, and `Newline` tokens. It
skips blank/comment-only lines, emits EOF dedents, reports inconsistent
dedents, and is exposed through `aven layout` for debugging. The core parser now
consumes the layout stream.

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

Status: done

Progress: `aven check` now parses through the lexer/layout pipeline and builds
real AST nodes for bindings, literal expressions, names, parenthesized function
calls, lambdas, and block-bodied lambdas. Unsupported operator syntax produces
an explicit recovery diagnostic and remains scheduled for Milestone 4c.

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

Status: done

Progress: the first structural parser slice handles array literals, tuple
literals, record literals/transforms, and `@{...}` set/variant-set literals.
Anonymous one-item tuples produce a targeted diagnostic.

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
  - shorthands
- preserve enough delimiter and trivia information for later formatting

Done when:

- representative record, array, set, tuple, and variant examples parse
- unsupported record-transform forms get honest diagnostics

## Milestone 4c: Operators, Access, And Branching Forms

Status: done

Progress: the expression parser now handles the 4c expression subset:
Pratt-parsed binary operators, pipelines, field access, nil-safe field access,
nil coalescing, postfix `?^`/`?!` propagation forms, and newline `?` match
expressions with literals, names, tuples, and constructor patterns. Record
patterns and guarded match arms are not part of this subset; they have targeted
diagnostics and are deferred to Milestone 4e.

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

- use a hand-written Pratt parser for expression precedence by default
- evaluate parser-combinator libraries only if handwritten recovery becomes
  worse than the dependency cost
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
- operator precedence is tested with AST-shape assertions. For now, parser unit
  tests are the AST-shape assertion mechanism; parser fixtures stay focused on
  parse-clean and diagnostic coverage.

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

## Milestone 4e: Pattern Syntax Completion

Status: later

Goal: complete pattern syntax after the expression and type parsers have
settled.

Tasks:

- parse record patterns
- parse guarded match arms
- decide whether patterns need their own AST-shape fixture harness
- keep semantic validation for later type checking

Done when:

- record-pattern examples parse
- guarded-arm examples parse
- unsupported pattern forms have targeted diagnostics

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

Completed parser groundwork:

- Milestone 4a: bindings, literals, calls, lambdas, and blocks
- Milestone 4b: structural collections
- Milestone 4c: operators, access, propagation, and the implemented `?` match
  subset

The next few queued changes should be:

1. finish review cleanup for Milestone 4c and commit it
2. implement Milestone 4d type syntax
3. implement Milestone 4e pattern syntax completion for record patterns and
   guarded match arms
4. decide whether parser tests need AST-shape fixtures beyond unit tests
5. decide the CST/trivia strategy before formatter work expands
6. start Milestone 5 formatter work
7. expand LSP from diagnostics/formatting to document symbols
8. start Milestone 6 name resolution skeleton

This keeps tooling ahead of semantics without spending too long on temporary
parser code.
