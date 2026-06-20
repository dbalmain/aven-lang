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

Status: done

Goal: make source files and diagnostics robust enough for parser work.

Progress: fixture-based parser diagnostic assertions landed alongside
Milestone 8 setup. The "tests assert structured diagnostics, not terminal
snapshots" done-when is satisfied. `aven check --format json` now emits
machine-readable diagnostics with severity, code, message, labels, byte spans,
and notes, so tools do not need to scrape terminal output. `aven-core` now owns
the shared `LineIndex` stored on `SourceFile`; the LSP uses that structural
source/index pair for offset/range conversion instead of rescanning the source
for every request. Parser output now carries a `FileId`, and `parse_source`
threads the id from `SourceFile` into `ParseOutput`; LSP documents keep stable
file ids across edits to the same URI. Diagnostics stay file-agnostic while
they are produced; `DiagnosticReport` attaches the stable `FileId` at the CLI
rendering boundary, while the LSP stores a merged diagnostics vector on each
parsed document and publishes it by URI without per-publish cloning.
The LSP now uses a single `DocumentStore` mutex that keeps a URI-to-`FileId`
table and parsed documents; each `ParsedDocument` owns its `SourceFile`.
`SourceMap` remains core infrastructure for future multi-file work, but the
single-file CLI and Arc-per-document LSP path do not store sources in it yet.
Lexer and layout stay string/token utilities; `parse_source(&SourceFile)` is the
file-aware parser entry point. `aven explain <code>` looks up a short
diagnostic explanation from the shared core table, so humans and AI agents can
get repair context without scraping terminal output. Emitted diagnostic codes
now come from an `aven-core` registry, and the explanation table has a coverage
test against that registry.

Even though incremental compilation is deferred, this milestone should make the
data-shape decisions that keep incremental tooling possible:

- diagnostics carry stable `FileId`s, not only paths or LSP URLs
- parser output is per-file and immutable after construction
- LSP stores source text, line indexes, diagnostics, and parse results keyed by
  `FileId`
- public APIs pass structured values rather than terminal-rendered strings

Tasks:

- defer full `SourceMap` integration until multi-file parsing needs shared
  source ownership
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
aven check --format json examples/hello.av
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

- uppercase-tag enforcement for variant members: `@{@ok(Text)}` parses without
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
until module/export semantics exist. The LSP now advertises full-document
semantic tokens and serves them from the cached raw token stream plus a small
declaration overlay. This first slice highlights comments, literals, paths,
regexes, labels, operators, runtime names, comptime names, top-level
definitions, and lambda parameters without adding a Tree-sitter or TextMate
grammar.

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
  - semantic tokens (syntax highlighting)

Done when:

- local scripts get useful name diagnostics
- basic editor navigation works before type checking

AST-walk note: the third structural `ExprKind` pass triggered the earlier watch
item, so boring child traversal now lives in one shared expression walker.
Resolver and name analysis keep only their scope-specific `Match`/`Lambda`/
`Block` behavior local.

## Milestone 7: Type Skeleton

Status: done

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
top-level bindings and lambda parameters. M7 is complete without normalized or
computed hover output; those belong to inference and comptime work.

## Milestone 8: Test Harness And Fixtures

Status: done

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

Progress: M8 has fixture-driven coverage for parser diagnostics, AST summaries,
lexer streams, layout streams, formatter output, declarations, name analysis,
and semantic check diagnostics. CLI integration tests cover `check`, `fmt`,
`tokens`, `layout`, JSON diagnostic output, and write/no-write behavior. The LSP
has a protocol smoke test that drives `initialize`, `textDocument/didOpen`, and
`textDocument/documentSymbol` through `tower-lsp`.

## Milestone 9: Incremental Tooling

Status: in progress

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
- draw the synchronous/debounced line by cost, not by phase. The per-edit
  (synchronous) path may carry the artifact bookkeeping and *cheap structural*
  per-declaration results — declaration keys, fingerprints, dependency edges,
  and declared-annotation `Type` lowering (a structural `Expr -> Type` walk).
  *Expensive* analysis — name resolution and type inference — stays in the
  debounced semantic pass and reuses prior results across revisions via the
  `invalidated_declarations` set. The `DeclarationArtifact` is the invalidation
  *key*; it is not where expensive analysis runs. Inference is the slice that
  finally consumes the invalidation closure, in the debounced lane.
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

Progress: the first M9 slice made the LSP document cache version-aware. Repeated
updates for the same URI, version, and text reuse the existing `ParsedDocument`;
new versions replace it while preserving the URI's stable `FileId`. LSP
formatting now calls `aven_fmt::format_parsed_source` with the cached
`ParseOutput`, so formatting no longer reparses the document through
`format_source`. The second M9 slice split cheap `parse.diagnostics` from
expensive `semantic_diagnostics`; LSP publishing still streams the combined
diagnostics, but later debounce/cancellation can now skip or delay semantic
work without changing the parse cache shape. The third M9 slice added
`aven check --timings`, which reports parse/name/check/total timings in text
mode and a `timingsMs` object in JSON mode. The fourth M9 slice made LSP
documents parse-first: `didOpen`/`didChange` publish parse diagnostics
immediately, schedule semantic diagnostics behind a short debounce, abort the
previous pending semantic task for the same URI, and reject stale semantic
results by document version before publishing.
The fifth M9 slice introduced `aven-compiler` as the thin compiler database
boundary: it owns immutable `DocumentSnapshot`s, parse/name/check timing,
semantic diagnostic analysis, and a generic revision-keyed document cache. The
CLI and LSP now consume that API instead of each reassembling the
parse/name/check pipeline. The sixth M9 slice added declaration-keyed artifacts:
each top-level declaration now has a stable name/phase/ordinal key, a source
fingerprint, and conservative dependency edges to other top-level declarations.
The compiler database reuses unchanged artifacts across document revisions only
when both the declaration's source fingerprint and dependency list match.
Snapshots also expose the dependency-aware invalidation closure: if a changed
artifact has unchanged dependents, those dependents are marked invalidated too,
including transitively. Whole-module semantic diagnostics still run as before;
this only establishes the invalidation unit future inference/comptime caches can
use. The closure is still not a recomputation order or a complete semantic cache
key: future slices that reuse declaration-level semantic results must account for
the invalidated set and compute an explicit dependency-aware schedule.

## Milestone 10: Type IR And Annotation Lowering

Status: done

Goal: introduce the first real semantic type representation without starting
unification yet. Milestone 7 deliberately kept annotations as parser `Expr`
terms because hover and simple diagnostics did not need a semantic `Type`. The
inference phase does need one, so the next step is to lower written annotations
into a compact `Type` IR while continuing to validate the original syntax through
the same path.

Tasks:

- define a semantic `Type` model for named types, type variables, type
  application, function types, nullable types, tuples, record rows, and variant
  rows
- lower annotation `Expr` terms into `Type` values without introducing a
  separate syntactic type parser
- keep diagnostics behavior stable: unknown type names, lowercase variant tags,
  and value/type record-entry errors should still surface through `aven check`
- leave computable/comptime type expressions as deferred nodes until comptime
  evaluation exists
- add unit coverage for the lowered shapes before inference consumes them

Done when:

- written annotations have a semantic representation usable by inference
- `check_module` and standalone annotation lowering share one implementation
- existing CLI, fixture, and LSP diagnostics stay unchanged

Progress: the first M10 slice added `Type`, `TypeRowEntry`, and `TypeLowering`
to `aven-check`. Annotation validation now flows through lowering, records and
variants share one row-entry representation, and tests lock lowered
function/application/nullable, record-row, and variant-row shapes. The second
M10 slice made that IR non-inert: `aven-check::AnnotationLowerer` lowers one
declared annotation on demand, and `aven-compiler` stores those types plus their
annotation-lowering diagnostics on each `DeclarationArtifact`. The compiler
builds the lowerer once per module but calls it only for new/recomputed
artifacts; unchanged artifacts reuse the cached type `Arc` without rerunning
annotation lowering. Dependency-resolution changes rebuild the artifact and
refresh annotation diagnostics. The third M10 slice opened value/annotation
agreement checking with a deliberately narrow debounced check: literal binding
values are compared against bare scalar annotations and report `type.mismatch`
only on definitive incompatibilities. Number literals synthesize as `Int` for
now; inferred `Int` still flows into `Float` contexts because the scalar
mismatch rule treats `Int`/`Float` as a deferred numeric-promotion case. That
default is revisable once numeric metavariable defaulting exists. That check now
runs at the declaration level, so inline annotations and adjacent
signature-plus-binding declarations share the same declared annotation lookup
instead of drifting by surface syntax. The value check is now recursive in the
checking direction: literals and tuple elements are checked against expected
types, and nullable values are accepted when they are `Nil` or satisfy the inner
type. Identifier values are checked when a top-level declaration value
references another top-level binding with a single clean declared annotation or
synthesized concrete type. Direct applications written under an annotation are
also synthesized and compared when inference produces a concrete type. Local
checking uses scoped known/unknown entries instead of a blanket scope-depth
gate. Annotated lambda parameters and local bindings carry their normalized
types, and unannotated sequential locals acquire a concrete type when the
monomorphic inference engine can synthesize one. Unsolved locals, unannotated
parameters, and match-pattern binders remain explicit unknown entries.
Nearest-scope lookup can therefore check inferred locals without ever borrowing
a same-named top-level type. Expected function annotations now seed
unannotated lambda parameters and check lambda bodies against expected return
types, so `(x) => x` can be checked as `(Int) -> Text` without full
generalization. Ambiguous overloads, unsolved identifier-valued bindings,
unsupported operator shapes, match-bodied values, recursive/generic bindings,
and full unification remain deferred. Literal record value checking now covers rows of
only fields and the open marker: wrong field types, missing required fields, and
unexpected fields on closed records. The open/closed rule is fixed by the spec
(records are closed by default; `.._` opens them, lowered to
`TypeRowEntry::Open`). The same field-set comparator handles literal record
values and record-type comparisons, so a synthesized or declared record type can
be checked structurally against an expected record type. Rows carrying spreads,
deletes, renames, or overwrites defer until row computation, and checking
explicit fields through a value spread is a follow-up. Open actual record types
and optional-field subtyping also defer until the row engine. Variant value arms
start in Milestone 11 with direct constructor values against literal variant
rows; row-computed variants still defer. Transparent comptime aliases are now
normalized before value checking, including alias chains and nested aliases.
`opaque(...)` lowers to an irreducible deferred type until comptime evaluation
and module-aware opacity exist. Cyclic aliases terminate silently for now;
reporting cycles and validating type-definition bodies are separate follow-up
slices.

## Milestone 11: Monomorphic Value Inference

Status: in progress

Goal: solve enough value types to feed the existing checking direction without
committing to full Hindley-Milner. A private unification engine assigns
metavariables to unknown binders and results, unifies them structurally, and
hands back a concrete type or defers. Diagnostics still come from the checker
comparing the synthesized type against an expected annotation; inference itself
never reports, so an unsolved or unsupported shape stays silent rather than risk
a false positive.

Tasks:

- add a `Type::Meta` unification variable, distinct from rigid annotation
  variables, that never escapes a public API or a checked output
- back top-level value synthesis with a unifier: literals, tuples, arrays, sets,
  literal records, blocks, lambdas, and applications infer a concrete type when
  every meta solves; generic top-level functions instantiate freshly at each use
- share a scoped known/unknown environment between checking and inference:
  annotated locals are checked, concrete unannotated local values are
  synthesized in source order, and every unsolved binder still shadows
  top-level declarations
- check lambda values contextually against expected function types: seed
  unannotated params from the expected type, compare explicit param and return
  annotations, and check the body against the expected result
- check block values contextually against expected types: check prefix bindings
  once while entering them into scope, then check the final expression
- guard recursive references and run an occurs-check so inference always
  terminates
- infer the first built-in operator subset while leaving custom operators and
  unsupported operand shapes deferred
- defer (synthesize nothing) for unsupported operator/match bodies, tag-sets,
  row-computed collections, and anything that leaves an unsolved meta

Done when:

- a binding whose value applies an inferred lambda is checked against its
  annotation through the shared comparator
- inference produces no diagnostics and never a false positive on deferred shapes
- full Hindley-Milner (let-generalization, full row unification, numeric
  defaulting) remains explicitly deferred to later milestones

Progress: a private unifier now backs monomorphic synthesis for top-level
declaration values. Literals, tuples, arrays, sets, literal records, blocks,
lambdas, and applications infer a concrete type when all metas solve, and
top-level inferred functions instantiate freshly at each use, so a generic
function can be applied at more than one type without leaking solutions between
uses. Direct array and set literals are checked per element against
`Array[element]` or `Set[element]`, giving the same per-element recovery as
tuples. Array- and set-valued identifiers still compare through synthesis plus
the applied-type comparator, so empty collections, heterogeneous collection
bindings, tag-sets, and set spreads leave an unsolved meta or row computation
and defer. `Set` is seeded as a builtin until import resolution exists. Block
inference extends a local environment in source order and uses the final
expression as the block type. The checking direction now tracks local scopes
through the parser's shared `merged_items` and `pattern_bindings` views.
Annotated lambda parameters, adjacent or inline local annotations, and
standalone local signatures enter the nearest scope with known normalized
types. Unannotated sequential bindings are synthesized against the same
nearest-scope environment; concrete literal, tuple, record, collection, block,
lambda, and application results become known to later items and nested scopes.
Unsolved bindings, unannotated parameters, and match-pattern binders remain
explicitly unknown, so both checking and inference stop before any same-named
top-level declaration. Block scope spreads and other destructuring block items
are not represented in the parser AST yet, so they remain deferred. Metas never
escape into
`value_types`: synthesis resolves a value to a concrete type or defers. Direct
applications written under annotations now use the same synthesis engine and are
compared against the declared type when the call result is concrete. Direct
lambda values are checked contextually against function annotations: expected
parameter types seed unannotated lambda parameters, explicit parameter
annotations are compared contravariantly, and return annotations plus bodies are
checked covariantly against the expected result. Contextual block checking now
uses the same bidirectional checker path as ordinary local bindings: prefix
bindings are checked once, entered into the nearest scope, and then the final
expression is checked against the expected type. The checker owns the unifier,
top-level memo, and local known/unknown environment directly, so contextual
checks and synthesis no longer thread a separate inference object through
record, block, lambda, call, and collection walks. Match expressions now carry
the expected result type into each arm body, and guards are checked against
`Bool`. When the match subject has a known literal variant row, simple
constructor patterns such as `@Ok(value)` give their payload binders known types
inside guards, arm bodies, and match-result synthesis; otherwise pattern binders
still enter as explicit unknown locals.
Unannotated match expressions also synthesize a result when all arm bodies
unify to one concrete type, so match-valued bindings can feed later identifier
checks. Direct variant constructor values such as `@Ok(1)` and nullary tags such
as `@Done` are checked against literal variant rows, and inferred singleton
variant constructor types feed the identifier path. Variant rows carrying
spreads, deletes, renames, or other row computation still defer. Function
comparison is structural: arity mismatches report `type.mismatch`, parameters
compare contravariantly, and results compare covariantly.
The first built-in operator subset now synthesizes concrete results for numeric
arithmetic, text `+`, numeric comparisons, equality over concrete compatible
operands, boolean `&&`/`||`, and unary numeric `-`. Unknown operands and
deliberately unknown binders resolve to deferred types rather than bindable
metas, so an expression such as `missing + 1` or a match-pattern binder used in
an operator body cannot fabricate a concrete type.
Applied types compare structurally when their arities match, so `Array[Int]` vs
`Array[Text]` reports through the same recursive comparator that handles tuples
and records. Recursive bindings and
self-application terminate through an in-progress guard and the occurs-check.
Custom operators, unsupported operand shapes, general match subject/pattern
typing, mixed or unknown match-arm results, tag-sets, row-computed collections,
and recursive or still-generic results defer. The shared
`map_type`/`visit_type` traversals back substitution, instantiation, and the
occurs/concreteness predicates so the engine grows with the `Type` grammar in
one place.
Consolidation C1 surfaces per-binder inferred types from `aven-check` and adds a
stable `Type` renderer, enabling inference-driven LSP hover and completion.

## Milestone 12: Hindley-Milner Generalization

Status: complete

Goal: turn the monomorphic engine from Milestone 11 into real Hindley-Milner
let-polymorphism. M11 synthesizes a concrete type or defers; its only
"polymorphism" is the heuristic that any metavariable left in a memoized
top-level type is treated as generic and freshened at each use, and
`resolve_if_concrete` then drops anything with a leftover meta — so a polymorphic
value such as `id = (x) => x` defers instead of being usable. M12 replaces that
heuristic with principled generalization over type schemes, so polymorphic values
are accepted and reusable, and adds numeric-literal defaulting so an
unconstrained number resolves instead of blocking.

Design decisions locked before starting:

- Generalization strategy: free-variable-set generalization, not levels. At
  embedded-script sizes the `ftv(env)` scan is cheap and far clearer than
  level-tracking; revisit only if profiling shows generalization dominates.
- Scheme representation: a `TypeScheme { vars: Vec<u32>, ty: Type }` (quantified
  metavariable ids), instantiated to fresh metas at each use. `Type` itself gains
  no quantifier node; quantification lives only in the scheme, so the checker's
  structural comparators are unchanged.
- A generalized scheme is "concrete enough" to feed the checking direction even
  when it has quantified vars: `id = (x) => x` becomes `forall a. (a) -> a` and
  is usable at `(Int) -> Int` and `(Text) -> Text` instead of deferring.
- Numeric defaulting: a number literal gets a fresh numeric metavariable that
  defaults to `Int`; if it unifies with `Float` it becomes `Float`, otherwise it
  defaults to `Int` at finalization. This is the one place `Int`/`Float` stop
  being treated as freely interchangeable.

Slices:

- 12.1 — type schemes + top-level generalization: introduce `TypeScheme`,
  generalize each top-level binding's inferred type over metas not free in the
  (empty, at top level) environment, instantiate at each use, and accept
  generalized functions into `value_types`. Replaces the leftover-meta heuristic
  in `infer_top_level`/`instantiate`. Numbers stay hard `Int`.
- 12.2 — local let-generalization: generalize unannotated local block bindings
  over metas not free in the enclosing scope, so a locally-defined polymorphic
  helper can be used at multiple types within its block.
- 12.3 — numeric defaulting: numeric metavariables with an `Int` default; number
  literals stop synthesizing a hard `Int`, unify across `Int`/`Float`, and
  default at finalization. Removes the M11 "Int flows into Float" special case.

Done when:

- a polymorphic binding (`id = (x) => x`) is inferred as a scheme and checks at
  two distinct instantiations in the same module without leaking solutions
- an unannotated local helper generalizes and is reused polymorphically
- an unconstrained numeric literal defaults instead of deferring, and a literal
  constrained to `Float` is `Float`; fixtures lock both
- full row-polymorphic record/variant solving remains deferred to its own
  milestone; generalization here is over ordinary (non-row) metavariables

## Milestone 13: Row Polymorphism

Status: complete

Goal: make records and variants row-polymorphic, the language's headline
differentiator. Follow the chosen design in `docs/row-polymorphism.md`: **Leijen
scoped labels** (duplicates legal internally, leftmost wins, no `Lacks`
constraint; surface enforces uniqueness), records and `@{...}` variants as the
two row domains, Roc-style defaults (record literals closed, record-consuming
function args open, tag literals open).

Design decisions locked before starting:

- Semantic row representation: introduce a normalized `Row { fields, tail }` in
  the type IR, where `tail` is `Closed` or a row metavariable `Var(u32)`
  (distinct from ordinary `Type::Meta`). `Type::Record`/`Type::Variant` carry a
  `Row`. This replaces the surface-flavoured `Vec<TypeRowEntry>` for *lowered*
  types; the parser AST and `RecordEntry` are unchanged. Selection, extension,
  and restriction are primitive on this row; there is no `Lacks` predicate.
- Lowering normalizes only the simple forms first (plain fields, plus the `.._`
  open marker producing a fresh row var). Surface transforms (spreads, deletes,
  renames, overwrites) are *row computation* and are lowered in slice 13.4; until
  then a transform row lowers to a deferred row tail rather than being silently
  accepted.
- Row unification is the Leijen rewrite algorithm: to unify rows, match each
  field of one against a same-label field of the other (rewriting the other's
  tail to surface a missing label), then unify the tails. A closed tail unified
  with a row var binds the var to the remaining fields; two closed rows must have
  equal field sets.
- Duplicate surface labels stay a parser/elaborator error (unchanged); the
  scoped-label leftmost-wins rule only governs the internal solver.

Slices:

- 13.1 — checking-direction open records (done): open-record width subtyping
  already worked — `compare_record` skips unexpected-field reports when the
  expected row is open, so `{ .._, name: Text }` accepts any record with at least
  `name: Text`, while a missing required field and an extra field on a closed
  record still error. This slice only locked that behavior with fixtures; the
  row-variable machinery for the *inference* direction is 13.2.
- 13.2a — structured row IR migration: replace `Type::Record`/`Type::Variant`'s
  `Vec<TypeRowEntry>` with a normalized `Type::Record(Row)` / `Type::Variant(Row)`
  where `Row { entries, tail }`, `entries` are labelled `RowEntry::Field`/`Tag`,
  and `tail` is `Closed` or `Open` (anonymous open marker, preserving today's
  `open: bool` semantics). Surface transforms (spreads, deletes, renames,
  overwrites) and any non-normalizable row lower to `Type::Deferred` while still
  walking children for nested diagnostics — preserving today's behavior exactly.
  Mechanical, behavior-preserving, guarded by the existing suite.
- 13.2b — row variable + open-row inference: refine `RowTail::Open` into a row
  metavariable `Var(u32)` with a row substitution in the unifier; implement Leijen
  record-row unification; make field access `r.x` constrain `r` to an open row
  containing `x`, so `length = (p) => sqrt(p.x * p.x + p.y * p.y)` infers a
  polymorphic open-record parameter. Record literals stay closed.
- 13.3 — variant rows: the same row machinery for `@{...}` tagged variants — open
  variant requirements `@{ ..r, @Circle(Float), ... }`, constructor checking
  against open variant rows, and match exhaustiveness that requires a `_` arm on
  an open variant.
- 13.4 — record transforms as row computation (done): 13.4a lowers
  closed record and variant row transforms — spreads (`..source`/`:..source`),
  adds, replaces, deletes (`-field`), and renames (`old -> new`) — when every
  source row is statically known and closed, with structured diagnostics for
  closed-row conflicts. 13.4b adds the A-lite path for extension and update over
  open or row-variable-shaped sources, absorbing the abstract remainder as an
  open tail. 13.4c adds value-direction record-literal transform inference for
  closed sources and the same A-lite extension/update behavior for open inferred
  sources. 13.4d seeds record-pattern binder types from known subject rows,
  including closed residual records for field-rest patterns like
  `{ x, ..rest }`. Open-row field-rest restriction remains deferred to the
  comptime era, so open or row-variable `..rest` binders stay unconstrained
  without a diagnostic.

Done when:

- an open record requirement accepts a superset record and rejects a record
  missing a required field, both by unification with structured diagnostics
- an unannotated function that selects fields infers an open-record parameter
- open variant requirements check and an open `?>` match requires a default arm
- record transforms type-check through row computation in both directions, and
  field-rest patterns bind closed residual records; fixtures lock each slice
- duplicate-label and missing-field diagnostics remain structured and recover

## Milestone 14: Comptime (Tooling-First Slices)

Status: in progress

Goal: advance the comptime surface *without* committing the evaluation engine.
Comptime evaluation proper (a staged compile-time interpreter, types as
first-class values) is the architectural keystone and stays deferred until these
low-lock-in slices land and the evaluation model is reviewed separately. Each
slice here is design-neutral on staging: it classifies, diagnoses, or parses
comptime surface that the eventual evaluator will reuse, and defers (no
false-positive) anything that genuinely needs evaluation — matching M11's
"synthesize or defer, never guess" discipline.

Design context: `../../docs/language-spec.md` → "Type and Comptime Bindings"
(non-liftable comptime artifacts; the Zig-style "only runtime-representable
values cross into runtime" rule). The full liftable-value lattice is deferred
until the evaluator exists.

Slices:

- 14.1 — comptime RHS artifact detection: detect when a capitalized
  (comptime) binding's right-hand side certainly denotes a **non-liftable
  comptime artifact** (record/variant *types*, type aliases, modules), and
  treat everything else as unknown until evaluation exists. Use the artifact
  result to diagnose the liftability errors the spec specifies — a lowercase
  runtime binding cannot hold a non-liftable artifact (`config = User`,
  `userType = User`) — while runtime bindings initialized from non-artifact or
  deferred comptime values remain accepted (`httpOk = HttpOk`). No evaluator:
  detection is structural plus alias-following across top-level comptime
  bindings; ambiguous RHSs defer silently.
  Done: `aven-check` detects non-liftable comptime artifacts and emits
  `comptime.non-liftable-into-runtime` for lowercase runtime bindings holding
  type artifacts. The liftable-value lattice is deferred to the evaluator.
- 14.2 — comptime-binding surface + honest diagnostics: ensure capitalized
  bindings whose RHS needs evaluation (rather than a structural type/value)
  produce an honest "comptime evaluation not yet supported" diagnostic with a
  Milestone 14 reference, instead of silently passing.
  Done: `aven-check` now emits `comptime.evaluation-unsupported` for top-level
  comptime RHS computation forms; nested computations inside aggregate literals
  such as `{ port: getPort() }` are a deliberate follow-up gap outside this
  shallow trigger.
- 14.3 — comptime utility-type *terms*: parse `Pick`/`Omit`/`Merge`/`Partial`
  applications as ordinary type terms (the unified grammar already parses the
  application shape) and lower them to `Type::Deferred` with a structured
  "evaluated once comptime lands" note — locking the surface and diagnostics
  before the evaluator exists.

Done when:

- a lowercase runtime binding holding a non-liftable comptime artifact is
  diagnosed with a structured code; non-artifact/deferred comptime RHSs do not
  produce this diagnostic; fixtures lock both directions
- comptime RHSs requiring evaluation produce an honest deferred diagnostic, not
  silent acceptance
- the full staged evaluator remains explicitly deferred to a later milestone

## Milestone 15: Literal Types

Status: in progress

Goal: string and number literal types (`@{'waiting', 'running'}`, `@{0, 1, 2}`)
reusing the **variant-row machinery** — closed singleton rows that widen by the
same boundary-subtyping rule as tags (see `../../docs/language-spec.md` →
"Comptime, literal types, and labels" and "Assignment and subtyping"). A new row
*entry kind*, not a new type system; only tags carry payloads.

Scope decision: this milestone covers the **type/annotation and checking
directions only**. Number/text literal *value* inference stays at the base type
(`Int`/`Text`) — making a bare literal infer its singleton would break ordinary
arithmetic and reimport TypeScript's widening rules. Literal-union types arise
from annotations; a fresh literal value checks against them by membership. Value
inference producing literal singletons is deferred (it only matters once
`keysOf`/comptime lands).

Slices:

- 15.1 — literal-union types + checking: lower `@{ <string/number literals> }` in
  type position to a `Type::Variant` row of literal entries (closed); a fresh
  literal value checks against a literal-union annotation by **membership**
  (reusing the fresh-literal path), and literal-union vs literal-union widens by
  **subset** (reusing variant widening). A wide base-typed value (`Text`/`Int`)
  into a narrower literal union is rejected with a structured diagnostic. Mixed
  tag+literal entries in one set get an honest diagnostic (homogeneous for now).
  Bare-literal value inference is unchanged.

- 15.2 — literal-union match exhaustiveness: when a `?>` match subject has a
  closed literal-union type, require each member literal to be covered by a
  literal-pattern arm or a `_` catch-all, reusing the existing closed-variant
  exhaustiveness path (`type.non-exhaustive-match`). A literal-pattern arm covers
  its member; an arm matching a literal outside the subject union is reported as
  unreachable. Open literal unions require `_` (same as open variants). Scope is
  exhaustiveness/coverage only — no new inference.

Done when:

- a binding annotated with a literal union accepts a member literal and rejects a
  non-member literal and a wide base-typed value, all with structured diagnostics
- a narrower literal union widens into a wider one at a boundary; fixtures lock
  each direction
- bare number/text literal inference still yields `Int`/`Text` (no singletons)
- a `?>` match on a closed literal union is non-exhaustive unless every member or
  a `_` is covered; an out-of-union literal arm is reported unreachable; fixtures
  lock both

## Milestone 16: Comptime Evaluator

Status: in progress

Goal: the comptime evaluator that M14.2's `comptime.evaluation-unsupported`
diagnostic stands in for — the engine that resolves `Type::Deferred` sites by
running comptime-position expressions at check time. Design: `../../docs/
language-spec.md` → "Comptime, literal types, and labels" and the
`comptime-evaluator-design` notes (two-stage staging; `ComptimeValue` domain
whose liftable arm is `Value`; types reify as the checker's `crate::ty::Type`
IR — no second representation; camelCase reflection `typeOf`/`keysOf`/`fieldsOf`/
`tagsOf`; specialization-time/demand-driven evaluation reusing `Deferred`). The
evaluator lives as a module **inside `aven-check`** (checker → evaluator →
checker for types) until it earns its own crate.

Built tooling-first as the smallest honest engine first, growing one comptime
position at a time. No `@param` specialization or monomorphization in the first
slice.

Slices:

- 16.1 — thinnest reflection slice: resolve the simplest `Type::Deferred` /
  `comptime.evaluation-unsupported` case — a **capitalized binding whose RHS is a
  reflection call on a concrete type**, starting with `keysOf` on a record. The
  evaluator runs `keysOf` at check time, reads the record's field labels, and
  reifies the result as a literal-union `Type::Variant` (the type-position face
  of a comptime label set), so the binding gets a concrete type with no
  `Deferred` and no unsupported diagnostic. A new `ComptimeValue` domain
  (minimal: enough for a label set and a reified `Type`) and an evaluator module
  in `aven-check`. `keysOf` on a non-record concrete type is a structured
  diagnostic; a non-concrete argument defers (M11 discipline). No `@param`, no
  specialization, no other reflection functions yet.
  Done: `aven-check` now evaluates `keysOf(<closed record type>)` in comptime
  type position, reifies the sorted field-name set as a closed literal-union
  variant, defers non-concrete subjects without diagnostics, and reports
  `comptime.reflection-type-mismatch` for concrete non-record subjects.

- 16.2 — comptime function application + specialization (type position only):
  extend the evaluator from the built-in `keysOf` to **user-defined comptime
  functions applied in a type position**, leaning on positional comptime (a call
  in a type position is comptime; **no `@param` syntax, no parser changes**).
  This is where the specialization machinery lands. When a type-position call's
  callee resolves to a user-defined function binding (a lambda), evaluate it by
  binding parameters to the evaluated comptime arguments and evaluating the body
  in that environment. Supported arguments: a **type** (e.g. `User` → reified
  `Type`); non-concrete arguments defer (M11 discipline). The body grammar is
  small and honest — a parameter reference, a reflection built-in call (`keysOf`)
  on an in-scope value, a nested comptime-function call, or a literal type term;
  anything else flows to the existing deferred / `comptime.evaluation-unsupported`
  path. **Specialize (memoize) per distinct `(function, comptime-arg-tuple)`** —
  the monomorphization point and the cycle key. Recursion is bounded two ways: a
  visited-set over `(fn, args)` catches a specialization that depends on its own
  in-progress result (`comptime.evaluation-cycle`), and a fuel budget bounds
  deep-but-finite evaluation (`comptime.evaluation-limit`); either reports a
  structured diagnostic and recovers by treating the site as `Deferred`. The
  body's `ComptimeValue` reifies into `crate::ty::Type` (reuse
  `reify_type_position`). Out of scope: `@param` marker, parser changes,
  runtime-position specialization, computed keys, comprehensions, general
  value-parameter specialization (all → M16.3).
  Done: `aven-check` now specializes top-level lambda bindings in type-position
  calls, threads a minimal comptime parameter environment through bodies,
  memoizes by function and comptime argument tuple, reports bounded recursion
  with `comptime.evaluation-cycle` / `comptime.evaluation-limit`, and reifies
  `keyUnion(User)`-style results to concrete literal unions.

- 16.3 — `@param` marker + runtime-position specialization (single computed-key
  access): the first **runtime-position** comptime slice. Thinnest end-to-end
  target:
  ```
  get = (o: {..r}, @key: keysOf(r)) => o[key]
  get(user, "name")    # result type: the type of user's `name` field
  get(user, "phone")   # error: "phone" is not a key of r
  ```
  Pieces:
  - **Parser (`aven-parser`):** lex `@<lowercase>` in parameter position as a
    comptime-param marker (repurposing the now-dead `@lowercase` `LabelPath`
    label-literal production; `@` outside a parameter declaration is a
    diagnostic, not a silent label). Add a `comptime` flag to `Param` set when
    `@` prefixes the name. Body references stay ordinary names (`@` is
    **declaration-only**, decided 2026-06-20). Update `walk`/`resolve`/`fmt` for
    the flag — `aven-fmt` prints `@` before a comptime param.
  - **Checker (`aven-check`):** at a call to a function with comptime params,
    evaluate each `@param` argument to a `ComptimeValue` and **check it against
    its declared (specialized) `@param` type**, reusing M15.1 literal-union
    membership (`"name" ∈ keysOf(r)`); out-of-domain reports a structured
    diagnostic. **Specialize** the call per comptime-arg tuple (reuse the M16.2
    `SpecializationKey` machinery) to compute the result type. Handle
    `ExprKind::Index` in **value** position (`o[k]`) where the callee infers to a
    concrete record and the single arg is a **comptime-known label** → that
    field's type; defer otherwise (do not disturb the type-position `Array[Int]`
    `Type::Apply` meaning of the same node). The result type flows into inference
    (M11). Membership guarantees the field exists, so access is exact (no
    nullability in this slice).
	  - Out of scope (later slices): record comprehensions (`{ keys -> k; ... }`),
	    `pick`/`omit`, key-**union** access (`o[k]` over a key set → field-type
	    union), runtime-`Text`-key access (→ nullable), computed transforms
	    (`[k]=v`, `-[k]`, `[k]->[k2]`), other reflection functions.
	  Done: `aven-parser` now treats `@lowercase` as a declaration-only comptime
	  parameter marker with structured recovery, `aven-fmt` round-trips `@key`,
	  and `aven-check` evaluates literal comptime arguments, specializes
	  `keysOf(r)` domains from runtime argument types, reuses literal-union
	  membership for out-of-domain keys, and infers exact field types for
	  single computed-key record access when the key is comptime-known.

- 16.4 — record comprehension + comptime unrolling (thinnest: `pick`): the first
  comprehension slice. Thinnest end-to-end target:
  ```
  pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }
  pick(user, @{"name", "email"})    # result type: { name: Text, email: Text }
  ```
  Pieces:
  - **Parser (`aven-parser`):** add a record-body **iteration** item
    `source -> binder; body` as `RecordEntry::Iteration { source, binder, body:
    Vec<RecordEntry> }` — `body` reuses `RecordEntry` recursively (iteration
    repeats sub-items; no parallel tree). Disambiguate from the existing rename
    `from -> to`: a trailing `;` (with sub-items) marks iteration, bare `a -> b`
    stays a rename. A `(k, v)` tuple in a record/comprehension body is an
    **add-entry** item (reuse the tuple `Element`; the checker interprets a
    2-tuple as add-field). Thread `walk`/`resolve`/`names` (the binder is an
    ordinary binder) and `aven-fmt` (round-trip the iteration form).
  - **Checker (`aven-check`):** extend `@param` to a key **set**
    (`@keys: keysOf(r)@{}`): the comptime argument is a set literal
    (`@{"name","email"}`) → a `LabelSet`, each member checked against the domain
    by literal-union membership (reuse M15.1/M16.3). **Comptime-unroll** the
    iteration over the comptime key set: bind the binder to each member and
    evaluate the body items; a `(k, v)` add-entry contributes a field named by
    `k`'s comptime label (reuse `comptime_known_label`) with the type of `v`
    (reusing M16.3 `o[k]` for `o[k]`). Build the result **record type** and feed
    it to inference (M11). Non-concrete key set → defer (no diagnostic).
  - Out of scope (later slices): `omit` and the `[k]` computed-key **syntax**
    (`-[k]`, `[k]=v`, `[k]->[k2]`); comprehension **guards/filters**
    (`!keys.has(k)`); tuple/destructuring binders (`object -> (k, v)`); set
    comprehensions; key-union / runtime-`Text`-key access; other reflection.

  Done: `aven-parser` now parses `source -> binder; body` as
  `RecordEntry::Iteration` while preserving bare `a -> b` renames, `aven-fmt`
  round-trips the comprehension form, and `aven-check` accepts concrete
  label-set comptime arguments for `keysOf(r)@{}`, checks each member against the
  literal-union domain, and unrolls record iterations so `pick(user,
  @{"name", "email"})` infers `{ name: Text, email: Text }` while non-concrete
  key sets defer.

- 16.5 — postfix collection-type sugar `X[]` / `X@{}`: a trailing **empty** `[]`
  or `@{}` after a type is sugar for the named collection generic (decided
  2026-06-20) — `X[]` ≡ `Array[X]`, `X@{}` ≡ `Set[X]` (`Set`/`Array` are already
  builtin types). Non-empty `X[a]` stays type application. Desugar to the
  named-generic application (reuse the `Array[a]`/`Set[a]` path; **no new Type IR
  variant**), which also fixes the current loose `X[]` → `Apply{X, []}` lowering.
  `pick`/`omit`'s key parameter becomes `@keys: keysOf(r)@{}` (== `Set[keysOf(r)]`),
  matching the `@{...}` set value; update the checker's `literal_union_domain_row`
  to unwrap `Set[<literal union>]`. Parser + fmt round-trip the postfix forms;
  the `@{}` postfix is the empty set adjacent to a type (mirroring the empty `[]`
  postfix), distinct from a `@{...}` set literal. `aven-parser` + `aven-fmt` +
  `aven-check`.

  Done: `aven-parser` desugars empty postfix `[]` and adjacent empty postfix
  `@{}` to existing `Array[...]`/`Set[...]` applications, `aven-fmt` round-trips
  both postfix spellings, and `aven-check` unwraps `Set[<literal union>]` for
  comptime key-set domains used by `pick`.

- 16.6 — `omit` via bulk computed delete `-keys`: the `pick` dual for closed
  record transforms. A bare delete name that resolves to a comptime label set
  deletes every member from the current closed row, while ordinary static deletes
  like `-password` keep their existing single-field behavior. Out-of-domain key
  sets remain rejected by the existing `@param` literal-union membership check.

  Done: `aven-check` now resolves bare delete names to in-scope comptime label
  sets before falling back to static delete, applies the existing closed-row and
  absent-field rules to bulk deletion, preserves static field delete behavior,
  and locks `omit(user, @{"name"})` plus out-of-domain `omit` fixtures.

- 16.8 — comprehension guards for filtered record unrolling:
  `source -> binder, guard; body` evaluates a small comptime predicate language
  per unrolled member. `set.has(k)`, `!`, `&&`, and `||` produce internal
  comptime `Bool` values; `true` folds the body, `false` skips it, and anything
  unsupported or not comptime-known defers through the existing row-entry path.

  Done: `aven-parser` carries `guard: Option<Expr>` on
  `RecordEntry::Iteration` and preserves rename disambiguation, shared AST
  walkers/name resolution include the guard in binder scope, `aven-fmt`
  round-trips guarded comprehensions, and `aven-check` filters unrolled record
  members so `omit2(user, @{"name"})` infers `{ email: Text }`.

	Done when:

- `Keys = keysOf(SomeRecord)` lowers to the literal union of that record's field
  names, usable as a type, with no `Deferred` and no
  `comptime.evaluation-unsupported`; a fixture locks it
- `keysOf` on a concrete non-record type produces a structured diagnostic; a
  `keysOf` call whose argument is not yet concrete defers without diagnostic
- the evaluator is a self-contained module in `aven-check`; reified types are
  the checker's own `Type` IR (no parallel representation)
- `keyUnion = (r) => keysOf(r)` with `Keys = keyUnion(User)` lowers `Keys` to the
  literal union of `User`'s field names, usable as a type, with no `Deferred` and
  no `comptime.evaluation-unsupported`; a fixture locks it
- a comptime function applied to a non-concrete type argument defers without a
  diagnostic; a self- or mutually-recursive comptime function that cannot resolve
  is bounded and reports `comptime.evaluation-cycle` (or
  `comptime.evaluation-limit`); fixtures lock both
- `@key` parses as a declaration-only comptime parameter (a `Param` comptime
  flag), `aven-fmt` round-trips it, and `@` outside a parameter declaration
  diagnoses; fixtures lock parse + fmt
- `get = (o: {..r}, @key: keysOf(r)) => o[key]` with `get(user, "name")` types as
  the `name` field's type; `get(user, "phone")` reports the out-of-domain
  membership diagnostic; `o[k]` for a comptime-known label on a concrete record
  yields the field type and defers otherwise; fixtures lock each
- a record-body iteration `source -> binder; body` parses to
  `RecordEntry::Iteration` (distinct from rename), `aven-fmt` round-trips it, and
  the binder resolves as an ordinary binder; fixtures lock parse + fmt
- `pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }` with
  `pick(user, @{"name", "email"})` types as `{ name: Text, email: Text }`; an
  out-of-domain key in the set reports the membership diagnostic; a non-concrete
  key set defers; fixtures lock each
- `dropKey = (object: {..r}, @key: keysOf(r)) => { ..object, -[key] }` with
  `dropKey(user, "name")` types as `{ email: Text }`; out-of-domain single
  labels reuse the literal-union membership diagnostic and defer the result;
  parser, formatter, checker fixture, and focused unit coverage lock M16.7

## Remaining Phase 2 Scope

Status: later

The tooling skeleton is in place, the semantic type IR and value-inference engine
landed (M10, M11), Hindley-Milner generalization is complete (M12), row
polymorphism is complete (M13), comptime tooling-first slices are underway (M14),
and literal types are underway (M15). The remaining hard semantic systems are
still deliberately out of scope for this plan.

Phase 2 work not planned here:

- comptime evaluation *engine* (the staged interpreter; M14 covers only the
  tooling-first surface/classification slices ahead of it)
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
- LSP document storage is consolidated behind one `DocumentStore` mutex, with
  stable URI-to-`FileId` allocation and parsed documents owning their sources
- an LSP protocol smoke test drives `initialize`, `textDocument/didOpen`, and
  `textDocument/documentSymbol` through `tower-lsp`, covering service
  registration and cached document state
- Milestone 10 lowered written annotations into an `aven-check::Type` IR and
  stored declaration-level declared types on compiler artifacts, with unchanged
  artifacts skipping annotation lowering; it is done. Milestone 11 has started
  monomorphic value inference: a private unifier synthesizes top-level value
  types (arrays, sets, blocks, lambdas, and applications included) or defers,
  feeding the existing checking direction. Direct applications, block-bodied
  values, arrays, sets, and supported built-in operator expressions written
  under annotations are checked when synthesis or structural literal checking
  produces a concrete result. Function types compare structurally with arity
  diagnostics, contravariant parameters, and covariant results; applied types
  compare structurally when their arities match. Local checking
  and inference now share parser-backed scoped known/unknown bindings.
  Unannotated sequential locals acquire concrete synthesized types when
  possible; unresolved locals and pattern binders still block top-level
  fallback. Expected function annotations seed unannotated lambda parameters and
  check lambda return values. Contextual block checking now uses prefix locals
  to check final expressions, including final calls. Contextual match checking
  now pushes the expected result type into each arm body; guarded match arms
  check each guard against `Bool`. Simple variant patterns use a known literal
  variant subject type to seed payload binders, direct constructor values check
  against literal variant rows, and unannotated match expressions synthesize a
  concrete type when their arm body types agree. At embedded-script sizes
  whole-module re-inference is cheap, so consuming artifact invalidation for
  inferred results stays deferred until profiling shows it pays off.
- Consolidation C2: LSP hover now shows inferred types for unannotated bindings,
  sourced from compiler snapshots and building on C1's inferred-types API.
- Consolidation C3: LSP completion now offers identifier names from in-scope
  locals, top-level declarations with inferred-type detail, and builtin type
  names. Type-directed field, record-label, and tag completion remains a later
  slice.
- Consolidation C6: goto-definition and completion local-scope queries now
  share one position-scoped traversal that yields visible bindings plus the
  binder, if any, under the cursor.
- Consolidation C5: variant constructors now infer closed singleton rows, match
  expressions infer the closed union of variant-valued arms, and variant
  assignment widens by requiring the actual tags to be a subset of the expected
  row; superset values and genuinely open values assigned into closed
  annotations now diagnose instead of silently passing.
- Record expected-type boundaries now use width subtyping: bound/computed record
  values may have extra fields, while fresh record literals assigned directly to
  closed annotations keep the excess-property check. The variant half of
  boundary subtyping is now covered by closed constructors, match row-union, and
  widening at assignment boundaries.
- Milestone 15.1 done: string and number literal-union annotations lower to
  literal entries in the variant row, fresh literals check by membership,
  literal-union rows widen by subset at boundaries, wide `Text`/`Int` values are
  rejected against narrower literal unions, and mixed tag/literal rows diagnose
  as unsupported for now. Bare string/number literal inference remains `Text` and
  `Int`.
- Milestone 15.2 done: closed literal-union matches reuse the variant
  exhaustiveness path, literal arms cover matching members, open literal unions
  require a default arm, and out-of-union literal arms report
  `type.unreachable-match-arm`.

## To investigate later

- **Braceless multiline set/record literals.** Allow dropping the braces on
  multiline shapes using a trailing sigil that opens a layout block: `@>` for
  sets/variants and `{>` for records, e.g.

  ```aven
  Result[t, e] = @>
    @Ok(t)
    @Err(e)
  ```

  Purely additive surface syntax over the existing `@{...}`/`{...}` forms (no
  change to the tag representation), so it carries no lock-in and can land
  whenever `@{...}` becomes tedious. Scope when picked up: a layout-block opener
  after `@>`/`{>` in the parser plus the matching `fmt` rendering; assess
  whether the layout pass needs a dedicated open/close pairing.

- **Warn on inert record `..` (comptime era).** A record parameter open marker
  (`{ x: Int, .. }` / `{ x: Int, ..r }`) requests comptime specialisation so the
  body can inspect/forward the caller's extra fields. When the body never
  performs a whole-row operation on that parameter (reflection, `{ ..val, ... }`
  spread, or forwarding to another row-generic), the marker is inert and the
  parameter is equivalent to its closed form — emit a warning. Requires the
  comptime specialisation machinery to exist (today every record `..` is inert,
  so the check would be universal noise); the "rest is observed" analysis falls
  out of detecting whether specialisation is observable. See
  `../../docs/language-spec.md` → Assignment and subtyping.

The tooling skeleton is far enough ahead of semantics for now; avoid spending
more time on temporary parser/tooling code unless a new semantic slice needs it.
