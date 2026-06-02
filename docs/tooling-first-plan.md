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

Progress: a hand-written raw lexer emits owned tokens for names, literals,
paths, labels, delimiters, comma/semicolon separators, newlines, indentation
widths, comments, and basic lexer diagnostics. Operators use maximal-munch
symbolic runs so custom operators can be tokenized without feeding declarations
back into the lexer. The language-reserved operator starts `=`, `:`, `.`, `?`,
and `@` reject unknown symbolic runs instead of silently splitting them. The
lexer feeds the layout pass, which in turn feeds the Milestone 4 parser.

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
  - maximal-munch operators
  - comma/semicolon separators
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
- keep lexing context-free: custom operator fixity belongs to parser/semantic
  phases, not the lexer; custom operators cannot start with `=`, `:`, `.`,
  `?`, or `@`

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
nil coalescing, postfix `?^`/`?!` propagation forms, and newline `?>` match
expressions with literals, names, tuples, and constructor patterns. Record
patterns and guarded match arms were deliberately deferred to Milestone 4e.

Goal: parse the expression syntax that controls execution order.

Tasks:

- parse:
  - pipelines
  - field access
  - `?>` match expressions
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
  - `term`
  - `module`
- parser tree/trivia decision:
  - Phase 1 uses an AST plus two token streams, not a full CST dependency.
  - `ParseOutput` carries raw lexer tokens, including comments and raw
    newline/indent trivia, for source-preserving tools.
  - `ParseOutput` also carries parser-facing layout tokens, so diagnostics,
    formatting, and LSP features can share the same layout view as the parser.
  - The formatter should use tokens for trivia preservation and the AST only
    where syntax shape matters. Revisit a real CST once formatter edits require
    parent/child token navigation rather than flat token walks.

Done when:

- representative pipeline, access, operator, and `?`-family examples parse
- operator precedence is tested with AST-shape assertions. Parser unit tests
  cover local invariants, and `parser/ast/valid` golden fixtures lock
  precedence-facing tree shape.
- watch item: before custom fixity grows, extract one shared operator
  classification source so parser precedence and formatter spacing do not drift.

## Milestone 4d: Type Syntax Parser

Status: done

Goal: parse type annotations without implementing full type inference yet.

Progress: the type-syntax slice uses a unified term grammar — type expressions
reuse `Expr`/`ExprKind` rather than a separate `TypeExpr` tree. Function arrows,
`[]` applications, `T?` nullable, record/variant rows, and tuple forms are all
parsed as ordinary expressions. Annotations and signatures are `:` ascriptions
whose right-hand side is an ordinary term. Record, set, and variant brace forms
share one entry parser (spreads, deletes, renames, open-row `.._`, optional-field
markers), matching the language's concept-reuse goal.

Deferred to semantics: two checks that would be parse-time errors in a
separate-`TypeExpr` design are deferred to later semantic phases (no semantic
phase exists yet), because the unified grammar accepts them as well-formed
syntax. The difference is handled by context in evaluation, not at the parser
level:

- uppercase-tag enforcement for variant members: `@{ok(Text)}` parses without
  error; a lowercase tag is a resolver-phase error, not a parser error.
- type-vs-value legality of record entries: an `optional` field marker or the
  `.._` open-row marker is syntactically accepted in any record context; whether
  it is meaningful is a semantic concern.

Tasks:

- parse binding and argument type annotations
- parse primitive, function, tuple, array, record, variant, nullable, and
  singleton-marker type syntax
- parse requirement/interface headers only as syntax if needed for examples
- keep semantic validation for Milestone 7

Done when:

- parser fixtures cover the type syntax used in the language spec, parsing clean
  with no diagnostics
- forms that remain syntactically malformed (e.g. `{ 1 = Text }`, a missing term
  after `:`) produce structured parser diagnostics

## Milestone 4e: Pattern Syntax Completion

Status: done

Progress: match arms now parse pattern terms through the ordinary expression
grammar, matching the type-syntax fold from Milestone 4d. Constructor patterns
are calls, nullary tags are comptime names, tuple/group patterns are ordinary
tuple/group expressions, and record patterns use the existing `RecordEntry`
parser. Guard expressions follow the pattern with comma syntax, matching the
language spec's comprehension-style guard shape. Parser unit tests cover local
invariants, and `parser/ast/valid` golden fixtures lock the reusable
pattern-position tree shape.

Goal: complete pattern syntax after the expression and type parsers have
settled.

Tasks:

- parse record patterns
- parse guarded match arms
- fold pattern syntax into the ordinary expression AST
- decide whether pattern-position terms need AST-shape fixtures beyond unit
  tests
- keep semantic validation for later type checking

Done when:

- record-pattern examples parse
- guarded-arm examples parse
- remaining pattern legality checks are deferred to semantic validation

## Milestone 5: Formatter

Status: in progress

Progress: the first formatter slice is layout-depth-driven line reindentation.
It trims trailing whitespace, normalizes newlines to `\n`, normalizes layout
indentation to two spaces per layout depth, preserves comments and blank lines,
refuses to format sources with parse errors, and has an idempotence regression
test. Formatter golden fixtures and CLI `fmt --check` integration tests cover
the current behavior. Expression-level spacing and multi-line collection layout
have started: the formatter now uses a raw-token emitter for simple intra-line
spacing around bindings, calls, commas, field access, pipelines, records, and
sets. Multi-line collection layout is still untouched.

Goal: make formatting useful before semantics are complete.

Tasks:

- preserve comments and blank lines through the raw token stream
- use AST shape where formatting needs syntax context
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

  Idempotence and totality are load-bearing beyond formatter hygiene: a total,
  idempotent formatter is the canonical-form bridge that lets agents emit an
  unambiguous explicit-delimiter form and have it normalized back to layout. See
  [`agent-syntax-ergonomics.md`](agent-syntax-ergonomics.md).

- add `aven fmt --check` tests
- wire LSP formatting to the same formatter

Done when:

- common examples can be formatted without losing comments
- formatting unsupported syntax reports a diagnostic instead of rewriting badly

Note: Phase 1 deliberately uses a token-backed AST model rather than a full CST.
That is enough for predictable early formatting while avoiding a parser rewrite
today. If formatting starts needing nested token ownership or comment attachment
rules that are hard to express over flat tokens, revisit a CST then.

Decision rule: expression spacing belongs in the raw-token-driven emitter, not
in a line-string rewrite pass. The emitter can normalize intra-line trivia, but
line-break and reflow decisions need AST context; do not add those to the
token-only spacing pass.

## Milestone 6: Name Resolution Skeleton

Status: in progress

Progress: the LSP now advertises document symbols and extracts a top-level
outline from the parser AST. Adjacent `signature + binding` pairs are merged
into one symbol so ordinary annotated functions do not appear twice. The
signature/binding walk now lives in `aven-parser` as a shared declaration
collection layer, so go-to-definition and phase diagnostics can build on the
same model. The LSP document store now caches parsed documents and their
declarations, avoiding a fresh parse for each diagnostics or document-symbol
request. Go-to-definition now resolves lambda parameters, sequential block
bindings, and match-arm pattern binders before falling back to top-level
declarations within the same file, using `aven-parser`'s first local definition
resolver rather than walking the AST in the LSP. Cached documents are stored
behind `Arc`, so LSP requests do not deep-clone the parse tree when retrieving
cached state. Top-level declarations now carry a parser-level overload shape,
recording arity plus whether parameter and result annotations are present. This
is intentionally not type identity; typed overload disjointness waits for M7
normalization. Name analysis now emits first-pass duplicate and accidental
shadowing diagnostics for top-level declarations, local bindings, lambda
parameters, and match-arm pattern binders; the CLI and LSP publish these
diagnostics alongside parse diagnostics. For now, name analysis runs only after
a clean parse, and local bindings are allowed to shadow top-level declarations;
both choices are covered by fixtures so they can be revisited deliberately.
The LSP rename provider now renames same-file local bindings by reusing the
parser's local-reference resolver; top-level rename is intentionally deferred
until module/export semantics exist.

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

AST-walk note: the third structural `ExprKind` pass triggered the earlier watch
item, so boring child traversal now lives in one shared expression walker.
Resolver and name analysis keep only their scope-specific `Match`/`Lambda`/
`Block` behavior local.

## Milestone 7: Type Skeleton

Status: in progress

Goal: introduce types in a way that powers tooling early. This is a semantics
milestone, not a syntax one: Milestone 4d already parses type annotations as
ordinary `Expr` terms (function arrows, `[]` applications, `T?`, record/variant
rows, tuples) and deferred their semantic validation here. M7 validates and
renders those annotation terms; it does not add a parsing pass and must not
introduce a parallel `TypeExpr` tree.

Decisions to lock before starting:

- crate placement: semantic validation lives in a new `aven-check` crate
  (`aven-parser` <- `aven-check` <- `aven-lsp`/`aven-cli`), so type machinery
  does not bloat the parser and later type milestones have a home. If a semantic
  `Type` is ever introduced it would define + render in `aven-core`, with
  elaboration in `aven-check`.
- no semantic `Type` yet: hover and unknown-name are both reachable directly on
  the annotation `Expr`, so the real `Type` (with unification variables) is
  deferred to the inference milestone, where its shape is driven by unification
  rather than by hover. Revisit only if hover must show normalized/computed
  types rather than the author's written annotation.
- builtin type set: M7 knows a fixed primitive set (`Int`, `Text`, `Bool`,
  `Unit`, `Nil`, ...) and nothing else. Records-as-types, variants/rows, `[a]`
  application semantics, and comptime type computation stay deferred to their
  milestones.

Tasks:

- add an `aven-check` crate for semantic validation over the parser AST (no new
  syntactic tree; annotations stay `Expr`)
- define the builtin type-name set; treat lowercase names in type position as
  type variables, not unknown types (same phase subtlety as the uppercase-runtime
  check — design it in, do not patch it later)
- validate annotation terms: report `type.unknown-name` for unresolved uppercase
  type names, reusing top-level declaration collection for the in-scope comptime
  set
- pick up the two checks Milestone 4d deferred to here: lowercase variant tags in
  `@{ ... }`, and type-vs-value legality of record entries (`optional` marker,
  `.._` open row)
- add LSP hover that renders an annotated binding's type by reusing the local
  resolver (`identifier_at_position` + `resolve_local_definition`) plus
  annotation pretty-printing
- emit structured `type.*` diagnostics with codes, spans, and repair notes;
  recover rather than abort
- create fixtures for hover output and each type diagnostic

Done when:

- hover shows the declared type for annotated bindings; unannotated bindings
  hover cleanly as unknown (no inference yet)
- unknown type names and the deferred semantic checks produce clear `type.*`
  diagnostics, locked by fixtures
- no separate syntactic type tree was introduced; annotations remain `Expr`

Progress: the first M7 slice added `aven-check` as the semantic-validation
crate. It validates written annotations without a `TypeExpr` or semantic `Type`
tree, knows a fixed builtin type-name set, treats lowercase names in type
position as type variables, reports `type.unknown-name`, and owns the two
deferred M4d checks for lowercase variant tags in variant type annotations and
type-only record entries in value records. `aven check` and the LSP now publish
these diagnostics, and LSP hover shows the written annotation for annotated
top-level bindings and lambda parameters.

## Milestone 8: Test Harness And Fixtures

Status: in progress

Goal: make syntax and diagnostic changes reviewable.

Fixture mechanism: hand-rolled goldenfile assertions against structured
diagnostic summaries. Parser fixtures live under
`crates/aven-parser/tests/fixtures/parser/`.

Tasks:

- choose the fixture mechanism early. Use small hand-rolled goldenfile
  assertions for structured diagnostics, token streams, layout streams, and
  compact AST summaries before parser tests grow large.
- add fixture-driven tests:

```text
tests/fixtures/parser/valid/*.av
tests/fixtures/parser/ast/valid/*.av
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

Strategy:

"Partial parsing" is three separate problems, and they matter very differently
for an embedded language:

1. incremental re-parsing — reuse the old tree, re-parse only the edited region
   (tree-sitter, Roslyn)
2. error-tolerant parsing — produce a useful tree from the broken input that
   exists while someone is mid-edit
3. lazy parsing — defer work (e.g. function bodies) until something needs it

Aven targets embedded scripts (Lua/Wren scale): hundreds to a few thousand
lines, not 100k-line files. At that size a hand-written recursive-descent parse
is well under a millisecond, so (1) optimizes the cheap half. The latency a
developer feels after an edit comes from name resolution, HM inference, and
comptime evaluation — not the parse. The incrementality that pays is therefore
memoized *computation* plus strong error recovery (2), not an incremental
parser.

Decision: do not build tree-sitter/Roslyn-grade incremental re-parsing. Two
reasons beyond file size. First, an incremental GLR engine or a red-green CST
is a large amount of code that works against the "compiler fits in an agent's
context" budget. Second, the layout pass makes subtree reuse unsound-prone:
editing indentation can change `Indent`/`Dedent`/`Newline` tokens and block
structure far from the edit, so a reused subtree's boundaries are not local to
the change. Indentation-sensitive languages are exactly where tree-sitter needs
a hand-written external scanner.

Approach, in order:

- cache `ParseOutput` per document version in the LSP so one parse backs
  diagnostics, symbols, go-to-definition, and formatting — no per-request
  re-parse
- make the top-level declaration the unit of incrementality: full-reparse the
  file (cheap) but key downstream analysis on individual declarations, so an
  edit to one binding invalidates only that binding's analysis. The
  `collect_declarations` pass is the seam for this.
- keep investing in error recovery; recovery quality drives per-keystroke DX
  more than reparse speed
- only if profiling shows the parse itself is the bottleneck: add incremental
  *lexing* (re-lex from the edit until the token stream resynchronizes, which
  is layout-pass friendly), and adopt a red-green / lossless CST (e.g. rowan)
  only if the CST is un-deferred for other reasons

Revisit triggers: real scripts routinely exceed ~10k lines; profiling shows
parse latency (not semantic analysis) dominating edit-to-feedback time; or a
host needs single-keystroke latency on large generated sources.

If a memoized-query layer (Salsa-style demand-driven computation, as in
rust-analyzer) is adopted for the second step, weigh it against "third-party
libraries stay behind crate boundaries": a query framework is a deep
architectural commitment that is hard to hide behind a boundary, unlike
chumsky/ariadne. A thin hand-rolled version keyed on document version plus
declaration identity may be the smaller first step.

Background, if a trigger is hit: red-green trees (Roslyn) and rowan are the
position-independent, structurally-shared "rope equivalent" for syntax trees;
tree-sitter descends from Wagner's incremental-GLR work; Salsa is the
demand-driven memoization model.

A lossless CST is also the prerequisite for any agent-facing projectional
editing (translate to a brace-explicit view for an LLM, translate back to
layout). That idea is deferred for the same reason and on the same trigger; the
cheaper near-term substitute is optional explicit-delimiter input plus the
idempotent formatter, not a translator. See
[`agent-syntax-ergonomics.md`](agent-syntax-ergonomics.md).

Tasks:

- store per-file parse results in the LSP backend, keyed on document version
- separate cheap lexer/parser diagnostics from expensive semantic diagnostics
- cache line indexes for span conversion
- add debounce/cancellation around document changes
- design the compiler database interface (declaration-keyed memoization) before
  imports/type inference

Done when:

- LSP does not rederive every intermediate artifact by hand
- a single parse backs every per-document request
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
- Milestone 4d: unified-grammar type syntax slice (types parse as `Expr`; no
  separate `TypeExpr`)
- Milestone 4e: record patterns and guarded match arms
- AST-shape golden fixtures for precedence, type terms, and match patterns
- token-backed trivia strategy: `ParseOutput` carries raw lexer tokens and
  parser-facing layout tokens alongside the AST
- Milestone 5 first slice: formatter normalizes layout indentation while
  preserving comments and blank lines
- formatter fixtures and CLI `fmt --check` integration coverage
- formatter raw-token emitter handles simple expression spacing
- lexer uses maximal-munch operators with comma/semicolon as dedicated
  separators, so custom operators do not require lexer registration
- reserved operator starts `=`, `:`, `.`, `?`, and `@` produce lexer
  diagnostics for unknown runs instead of silently splitting
- LSP document symbols expose top-level bindings/signatures, merging adjacent
  annotated bindings into one outline entry
- `aven-parser` exposes a first declaration collection pass for top-level
  bindings/signatures and the uppercase/lowercase phase split
- declaration golden fixtures lock the top-level declaration model before
  diagnostics and local scope resolution grow
- LSP go-to-definition resolves top-level runtime/comptime declarations in the
  current file using the cached declaration list
- declaration collection shares the lexer's uppercase/lowercase identifier rule
  instead of reimplementing the phase split
- `aven-parser` exposes a first local definition resolver for lambda parameters,
  sequential block bindings, and match-arm pattern binders; LSP
  go-to-definition uses it before falling back to the top-level declaration list
- declaration fixtures include shallow parser-level callable shapes, giving
  duplicate/shadowing diagnostics enough information to avoid false positives on
  plausible typed overloads while deferring overload disjointness to M7
- `aven-parser::analyze_names` emits first-pass duplicate declaration, duplicate
  local, and accidental-shadowing diagnostics; `aven check` and the LSP publish
  them
- local `signature + binding` pairs are treated as one binder for name
  diagnostics, matching top-level declaration collection
- shallow phase diagnostics reject uppercase value parameters, because function
  parameters are runtime binders in the current syntax. RHS classification for
  uppercase bindings, including liftable values like `HttpOk = 200`, is deferred
  to the M7 comptime/liftability phase.
- unused local binding warnings now cover lambda parameters, sequential block
  bindings, and match pattern binders. The pass suppresses unused warnings when
  the same name-analysis run has errors, keeping recovery noise low.
- LSP rename edits cover same-file local bindings and reject invalid identifier
  targets. Top-level and cross-file rename are deferred.

The next few queued changes should be:

1. add fixture/protocol coverage for hover if needed, then mark M7 done unless
   normalized/computed hover output becomes necessary before inference

This keeps tooling ahead of semantics without spending too long on temporary
parser code.
