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

As of 2026-07-02 the workspace has:

- `aven-core` — spans, `SourceMap`, structured diagnostics, the diagnostic-code
  registry, and the `aven explain` texts
- `aven-parser` — raw lexer, layout pass, unified-grammar parser (types and
  patterns are ordinary `Expr` terms), declaration collection, name analysis
- `aven-fmt` — layout reindentation plus token-driven expression spacing
- `aven-check` — annotation lowering to a semantic `Type` IR, bidirectional
  checking, Hindley-Milner inference with let-generalization, row-polymorphic
  records/variants, literal types, the comptime evaluator slices, and the
  null/undefined model
- `aven-compiler` — the compiler database: document snapshots, timings,
  declaration-keyed artifacts and invalidation
- `aven-eval` — tree-walking evaluator (records, variants, collections, match,
  closures/recursion, `?^`/`?!`, structured logging, platform natives)
- `aven-host` — the typed host boundary: value+type registration, typed-fn
  adapter, host comptime resolvers, file/stream/HTTP capabilities
- `aven-lsp` — diagnostics, formatting, symbols, goto, rename, semantic tokens,
  hover, inlay hints, completion (identifier/field/label/literal), signature
  help, quick fixes
- `aven` CLI — `check` (text/JSON, timings), `fmt`, `tokens`, `layout`,
  `explain`, `run` (with log configuration), `lsp`

`aven check` covers parse, name, annotation, and inference checks; `aven run`
executes programs against the host prelude.

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

Progress: fixture-based parser diagnostic assertions landed alongside Milestone
8 setup. The "tests assert structured diagnostics, not terminal snapshots"
done-when is satisfied. `aven check --format json` now emits machine-readable
diagnostics with severity, code, message, labels, byte spans, and notes, so
tools do not need to scrape terminal output. `aven-core` now owns the shared
`LineIndex` stored on `SourceFile`; the LSP uses that structural source/index
pair for offset/range conversion instead of rescanning the source for every
request. Parser output now carries a `FileId`, and `parse_source` threads the id
from `SourceFile` into `ParseOutput`; LSP documents keep stable file ids across
edits to the same URI. Diagnostics stay file-agnostic while they are produced;
`DiagnosticReport` attaches the stable `FileId` at the CLI rendering boundary,
while the LSP stores a merged diagnostics vector on each parsed document and
publishes it by URI without per-publish cloning. The LSP now uses a single
`DocumentStore` mutex that keeps a URI-to-`FileId` table and parsed documents;
each `ParsedDocument` owns its `SourceFile`. `SourceMap` remains core
infrastructure for future multi-file work, but the single-file CLI and
Arc-per-document LSP path do not store sources in it yet. Lexer and layout stay
string/token utilities; `parse_source(&SourceFile)` is the file-aware parser
entry point. `aven explain <code>` looks up a short diagnostic explanation from
the shared core table, so humans and AI agents can get repair context without
scraping terminal output. Emitted diagnostic codes now come from an `aven-core`
registry, and the explanation table has a coverage test against that registry.

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

Each diagnostic code should have a short generated documentation paragraph. CLI
diagnostics and LSP diagnostics should carry the code so humans and AI agents
can look up the explanation.

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
lexer feeds the layout pass, which in turn feeds the Milestone 4 parser. Normal
double-quoted string interpolation is implemented with lexer-driven
`InterpolationStart`/`InterpolationMiddle`/`InterpolationEnd` tokens and a
dedicated `ExprKind::Interpolation` node; interpolation bodies are ordinary
expressions and are auto-stringified to `Text`. Triple-quoted/raw strings and
`@"..."` label interpolation remain deferred.

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
  phases, not the lexer; custom operators cannot start with `=`, `:`, `.`, `?`,
  or `@`

Recommended approach:

- keep the raw lexer hand-written unless a library demonstrably improves clarity
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
skips blank/comment-only lines, emits EOF dedents, reports inconsistent dedents,
and is exposed through `aven layout` for debugging. The core parser now consumes
the layout stream.

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
share one entry parser (spreads, deletes, renames, open-row `.._`,
optional-field markers), matching the language's concept-reuse goal.

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
both choices are covered by fixtures so they can be revisited deliberately. The
LSP rename provider now renames same-file local bindings by reusing the parser's
local-reference resolver; top-level rename is intentionally deferred until
module/export semantics exist. The LSP now advertises full-document semantic
tokens and serves them from the cached raw token stream plus a small declaration
overlay. This first slice highlights comments, literals, paths, regexes, labels,
operators, runtime names, comptime names, top-level definitions, and lambda
parameters without adding a Tree-sitter or TextMate grammar.

Scoping semantics firmed up later (2026-06-23): the top level is one
mutually-recursive scope with no shadowing; `:=` is the explicit rebind form and
is block-only (a top-level `:=` gets a dedicated diagnostic anchored on the
operator); unbound value names are reported at check time; and a
duplicate-declared name stays bound, so no unbound-name cascade follows.
`aven run` stays lenient (parse + eval), while check/LSP remain the static gate.

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
  `Unit`, `Undefined`, ...) and nothing else. Records-as-types, variants/rows,
  `[a]` application semantics, and comptime type computation stay deferred to
  their milestones.

Tasks:

- add an `aven-check` crate for semantic validation over the parser AST (no new
  syntactic tree; annotations stay `Expr`)
- define the builtin type-name set; treat lowercase names in type position as
  type variables, not unknown types (same phase subtlety as the
  uppercase-runtime check — design it in, do not patch it later)
- validate annotation terms: report `type.unknown-name` for unresolved uppercase
  type names, reusing top-level declaration collection for the in-scope comptime
  set
- pick up the two checks Milestone 4d deferred to here: lowercase variant tags
  in `@{ ... }`, and type-vs-value legality of record entries (`optional`
  marker, `.._` open row)
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
memoized _computation_ plus strong error recovery (2), not an incremental
parser.

Decision: do not build tree-sitter/Roslyn-grade incremental re-parsing. Two
reasons beyond file size. First, an incremental GLR engine or a red-green CST is
a large amount of code that works against the "compiler fits in an agent's
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
  (synchronous) path may carry the artifact bookkeeping and _cheap structural_
  per-declaration results — declaration keys, fingerprints, dependency edges,
  and declared-annotation `Type` lowering (a structural `Expr -> Type` walk).
  _Expensive_ analysis — name resolution and type inference — stays in the
  debounced semantic pass and reuses prior results across revisions via the
  `invalidated_declarations` set. The `DeclarationArtifact` is the invalidation
  _key_; it is not where expensive analysis runs. Inference is the slice that
  finally consumes the invalidation closure, in the debounced lane.
- keep investing in error recovery; recovery quality drives per-keystroke DX
  more than reparse speed
- only if profiling shows the parse itself is the bottleneck: add incremental
  _lexing_ (re-lex from the edit until the token stream resynchronizes, which is
  layout-pass friendly), and adopt a red-green / lossless CST (e.g. rowan) only
  if the CST is un-deferred for other reasons

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
diagnostics, but later debounce/cancellation can now skip or delay semantic work
without changing the parse cache shape. The third M9 slice added
`aven check --timings`, which reports parse/name/check/total timings in text
mode and a `timingsMs` object in JSON mode. The fourth M9 slice made LSP
documents parse-first: `didOpen`/`didChange` publish parse diagnostics
immediately, schedule semantic diagnostics behind a short debounce, abort the
previous pending semantic task for the same URI, and reject stale semantic
results by document version before publishing. The fifth M9 slice introduced
`aven-compiler` as the thin compiler database boundary: it owns immutable
`DocumentSnapshot`s, parse/name/check timing, semantic diagnostic analysis, and
a generic revision-keyed document cache. The CLI and LSP now consume that API
instead of each reassembling the parse/name/check pipeline. The sixth M9 slice
added declaration-keyed artifacts: each top-level declaration now has a stable
name/phase/ordinal key, a source fingerprint, and conservative dependency edges
to other top-level declarations. The compiler database reuses unchanged
artifacts across document revisions only when both the declaration's source
fingerprint and dependency list match. Snapshots also expose the
dependency-aware invalidation closure: if a changed artifact has unchanged
dependents, those dependents are marked invalidated too, including transitively.
Whole-module semantic diagnostics still run as before; this only establishes the
invalidation unit future inference/comptime caches can use. The closure is still
not a recomputation order or a complete semantic cache key: future slices that
reuse declaration-level semantic results must account for the invalidated set
and compute an explicit dependency-aware schedule.

## Milestone 10: Type IR And Annotation Lowering

Status: done

Goal: introduce the first real semantic type representation without starting
unification yet. Milestone 7 deliberately kept annotations as parser `Expr`
terms because hover and simple diagnostics did not need a semantic `Type`. The
inference phase does need one, so the next step is to lower written annotations
into a compact `Type` IR while continuing to validate the original syntax
through the same path.

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
types, and nullable values are accepted when they are `Undefined` or satisfy the
inner type. Identifier values are checked when a top-level declaration value
references another top-level binding with a single clean declared annotation or
synthesized concrete type. Direct applications written under an annotation are
also synthesized and compared when inference produces a concrete type. Local
checking uses scoped known/unknown entries instead of a blanket scope-depth
gate. Annotated lambda parameters and local bindings carry their normalized
types, and unannotated sequential locals acquire a concrete type when the
monomorphic inference engine can synthesize one. Unsolved locals, unannotated
parameters, and match-pattern binders remain explicit unknown entries.
Nearest-scope lookup can therefore check inferred locals without ever borrowing
a same-named top-level type. Expected function annotations now seed unannotated
lambda parameters and check lambda bodies against expected return types, so
`(x) => x` can be checked as `(Int) -> Text` without full generalization.
Ambiguous overloads, unsolved identifier-valued bindings, unsupported operator
shapes, match-bodied values, recursive/generic bindings, and full unification
remain deferred. Literal record value checking now covers rows of only fields
and the open marker: wrong field types, missing required fields, and unexpected
fields on closed records. The open/closed rule is fixed by the spec (records are
closed by default; `.._` opens them, lowered to `TypeRowEntry::Open`). The same
field-set comparator handles literal record values and record-type comparisons,
so a synthesized or declared record type can be checked structurally against an
expected record type. Rows carrying spreads, deletes, renames, or overwrites
defer until row computation, and checking explicit fields through a value spread
is a follow-up. Open actual record types and optional-field subtyping also defer
until the row engine. Variant value arms start in Milestone 11 with direct
constructor values against literal variant rows; row-computed variants still
defer. Transparent comptime aliases are now normalized before value checking,
including alias chains and nested aliases. `opaque(...)` lowers to an
irreducible deferred type until comptime evaluation and module-aware opacity
exist. Cyclic aliases terminate silently for now; reporting cycles and
validating type-definition bodies are separate follow-up slices.

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
  synthesized in source order, and every unsolved binder still shadows top-level
  declarations
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
- inference produces no diagnostics and never a false positive on deferred
  shapes
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
standalone local signatures enter the nearest scope with known normalized types.
Unannotated sequential bindings are synthesized against the same nearest-scope
environment; concrete literal, tuple, record, collection, block, lambda, and
application results become known to later items and nested scopes. Unsolved
bindings, unannotated parameters, and match-pattern binders remain explicitly
unknown, so both checking and inference stop before any same-named top-level
declaration. Block scope spreads and other destructuring block items are not
represented in the parser AST yet, so they remain deferred. Metas never escape
into `value_types`: synthesis resolves a value to a concrete type or defers.
Direct applications written under annotations now use the same synthesis engine
and are compared against the declared type when the call result is concrete.
Direct lambda values are checked contextually against function annotations:
expected parameter types seed unannotated lambda parameters, explicit parameter
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
still enter as explicit unknown locals. Unannotated match expressions also
synthesize a result when all arm bodies unify to one concrete type, so
match-valued bindings can feed later identifier checks. Direct variant
constructor values such as `@Ok(1)` and nullary tags such as `@Done` are checked
against literal variant rows, and inferred singleton variant constructor types
feed the identifier path. Variant rows carrying spreads, deletes, renames, or
other row computation still defer. Function comparison is structural: arity
mismatches report `type.mismatch`, parameters compare contravariantly, and
results compare covariantly. The first built-in operator subset now synthesizes
concrete results for numeric arithmetic, text `+`, numeric comparisons, equality
over concrete compatible operands, boolean `&&`/`||`, and unary numeric `-`.
Unknown operands and deliberately unknown binders resolve to deferred types
rather than bindable metas, so an expression such as `missing + 1` or a
match-pattern binder used in an operator body cannot fabricate a concrete type.
Applied types compare structurally when their arities match, so `Array[Int]` vs
`Array[Text]` reports through the same recursive comparator that handles tuples
and records. Recursive bindings and self-application terminate through an
in-progress guard and the occurs-check. Custom operators, unsupported operand
shapes, general match subject/pattern typing, mixed or unknown match-arm
results, tag-sets, row-computed collections, and recursive or still-generic
results defer. The shared `map_type`/`visit_type` traversals back substitution,
instantiation, and the occurs/concreteness predicates so the engine grows with
the `Type` grammar in one place. Consolidation C1 surfaces per-binder inferred
types from `aven-check` and adds a stable `Type` renderer, enabling
inference-driven LSP hover and completion.

## Milestone 12: Hindley-Milner Generalization

Status: complete

Goal: turn the monomorphic engine from Milestone 11 into real Hindley-Milner
let-polymorphism. M11 synthesizes a concrete type or defers; its only
"polymorphism" is the heuristic that any metavariable left in a memoized
top-level type is treated as generic and freshened at each use, and
`resolve_if_concrete` then drops anything with a leftover meta — so a
polymorphic value such as `id = (x) => x` defers instead of being usable. M12
replaces that heuristic with principled generalization over type schemes, so
polymorphic values are accepted and reusable, and adds numeric-literal
defaulting so an unconstrained number resolves instead of blocking.

Design decisions locked before starting:

- Generalization strategy: free-variable-set generalization, not levels. At
  embedded-script sizes the `ftv(env)` scan is cheap and far clearer than
  level-tracking; revisit only if profiling shows generalization dominates.
- Scheme representation: a `TypeScheme { vars: Vec<u32>, ty: Type }` (quantified
  metavariable ids), instantiated to fresh metas at each use. `Type` itself
  gains no quantifier node; quantification lives only in the scheme, so the
  checker's structural comparators are unchanged.
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
  `Row`. This replaces the surface-flavoured `Vec<TypeRowEntry>` for _lowered_
  types; the parser AST and `RecordEntry` are unchanged. Selection, extension,
  and restriction are primitive on this row; there is no `Lacks` predicate.
- Lowering normalizes only the simple forms first (plain fields, plus the `.._`
  open marker producing a fresh row var). Surface transforms (spreads, deletes,
  renames, overwrites) are _row computation_ and are lowered in slice 13.4;
  until then a transform row lowers to a deferred row tail rather than being
  silently accepted.
- Row unification is the Leijen rewrite algorithm: to unify rows, match each
  field of one against a same-label field of the other (rewriting the other's
  tail to surface a missing label), then unify the tails. A closed tail unified
  with a row var binds the var to the remaining fields; two closed rows must
  have equal field sets.
- Duplicate surface labels stay a parser/elaborator error (unchanged); the
  scoped-label leftmost-wins rule only governs the internal solver.

Slices:

- 13.1 — checking-direction open records (done): open-record width subtyping
  already worked — `compare_record` skips unexpected-field reports when the
  expected row is open, so `{ .._, name: Text }` accepts any record with at
  least `name: Text`, while a missing required field and an extra field on a
  closed record still error. This slice only locked that behavior with fixtures;
  the row-variable machinery for the _inference_ direction is 13.2.
- 13.2a — structured row IR migration: replace `Type::Record`/`Type::Variant`'s
  `Vec<TypeRowEntry>` with a normalized `Type::Record(Row)` /
  `Type::Variant(Row)` where `Row { entries, tail }`, `entries` are labelled
  `RowEntry::Field`/`Tag`, and `tail` is `Closed` or `Open` (anonymous open
  marker, preserving today's `open: bool` semantics). Surface transforms
  (spreads, deletes, renames, overwrites) and any non-normalizable row lower to
  `Type::Deferred` while still walking children for nested diagnostics —
  preserving today's behavior exactly. Mechanical, behavior-preserving, guarded
  by the existing suite.
- 13.2b — row variable + open-row inference: refine `RowTail::Open` into a row
  metavariable `Var(u32)` with a row substitution in the unifier; implement
  Leijen record-row unification; make field access `r.x` constrain `r` to an
  open row containing `x`, so `length = (p) => sqrt(p.x * p.x + p.y * p.y)`
  infers a polymorphic open-record parameter. Record literals stay closed.
- 13.3 — variant rows: the same row machinery for `@{...}` tagged variants —
  open variant requirements `@{ ..r, @Circle(Float), ... }`, constructor
  checking against open variant rows, and match exhaustiveness that requires a
  `_` arm on an open variant.
- 13.4 — record transforms as row computation (done): 13.4a lowers closed record
  and variant row transforms — spreads (`..source`/`:..source`), adds, replaces,
  deletes (`-field`), and renames (`old -> new`) — when every source row is
  statically known and closed, with structured diagnostics for closed-row
  conflicts. 13.4b adds the A-lite path for extension and update over open or
  row-variable-shaped sources, absorbing the abstract remainder as an open tail.
  13.4c adds value-direction record-literal transform inference for closed
  sources and the same A-lite extension/update behavior for open inferred
  sources. 13.4d seeds record-pattern binder types from known subject rows,
  including closed residual records for field-rest patterns like
  `{ x, ..rest }`. Open-row field-rest restriction remains deferred to the
  comptime era, so open or row-variable `..rest` binders stay unconstrained
  without a diagnostic.
- 13.5 (addendum, 2026-06-24) — general row-polymorphic spread/merge inference:
  unannotated lambdas whose bodies spread/merge open parameter rows now infer
  row-polymorphic function types, extending 13.4's A-lite path to the
  inference-direction lambda results (merged as `fix/rowpoly-general`).

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

Goal: advance the comptime surface _without_ committing the evaluation engine.
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

- 14.1 — comptime RHS artifact detection: detect when a capitalized (comptime)
  binding's right-hand side certainly denotes a **non-liftable comptime
  artifact** (record/variant _types_, type aliases, modules), and treat
  everything else as unknown until evaluation exists. Use the artifact result to
  diagnose the liftability errors the spec specifies — a lowercase runtime
  binding cannot hold a non-liftable artifact (`config = User`,
  `userType = User`) — while runtime bindings initialized from non-artifact or
  deferred comptime values remain accepted (`httpOk = HttpOk`). No evaluator:
  detection is structural plus alias-following across top-level comptime
  bindings; ambiguous RHSs defer silently. Done: `aven-check` detects
  non-liftable comptime artifacts and emits `comptime.non-liftable-into-runtime`
  for lowercase runtime bindings holding type artifacts. The liftable-value
  lattice is deferred to the evaluator.
- 14.2 — comptime-binding surface + honest diagnostics: ensure capitalized
  bindings whose RHS needs evaluation (rather than a structural type/value)
  produce an honest "comptime evaluation not yet supported" diagnostic with a
  Milestone 14 reference, instead of silently passing. Done: `aven-check` now
  emits `comptime.evaluation-unsupported` for top-level comptime RHS computation
  forms; nested computations inside aggregate literals such as
  `{ port: getPort() }` are a deliberate follow-up gap outside this shallow
  trigger.
- 14.3 — comptime utility-type _terms_: parse `Pick`/`Omit`/`Merge`/`Partial`
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

Status: complete (15.1–15.5; the value-position flip is recorded in
`../../docs/literal-types.md`)

Goal: string and number literal types (`@{'waiting', 'running'}`, `@{0, 1, 2}`)
reusing the **variant-row machinery** — closed singleton rows that widen by the
same boundary-subtyping rule as tags (see `../../docs/language-spec.md` →
"Comptime, literal types, and labels" and "Assignment and subtyping"). A new row
_entry kind_, not a new type system; only tags carry payloads.

Scope decision (original, superseded): the milestone first covered only the
type/annotation and checking directions, keeping bare literal value inference at
the base types (`Int`/`Text`) to avoid TypeScript-style widening rules.
**Superseded 2026-06-30 — the value-position flip landed.** Bare string/number
literal values now infer open singleton variant rows (`a = 2` gives `a : 2`);
base operations widen literals back to their base (`1 + 2 : Int`), so ordinary
arithmetic is unaffected, and open tails let distinct producers join by row
unification. The rule-by-rule model (R1–R6, the `|` operator) and per-slice
commits are recorded in `../../docs/literal-types.md` — all done. `Bool`
literals still infer `Bool` (no bool singletons yet — see 15.3).

Slices:

- 15.1 — literal-union types + checking: lower `@{ <string/number literals> }`
  in type position to a `Type::Variant` row of literal entries (closed); a fresh
  literal value checks against a literal-union annotation by **membership**
  (reusing the fresh-literal path), and literal-union vs literal-union widens by
  **subset** (reusing variant widening). A wide base-typed value (`Text`/`Int`)
  into a narrower literal union is rejected with a structured diagnostic. Mixed
  tag+literal entries in one set get an honest diagnostic (homogeneous for now).
  Bare-literal value inference is unchanged.

- 15.2 — literal-union match exhaustiveness: when a `?>` match subject has a
  closed literal-union type, require each member literal to be covered by a
  literal-pattern arm or a `_` catch-all, reusing the existing closed-variant
  exhaustiveness path (`type.non-exhaustive-match`). A literal-pattern arm
  covers its member; an arm matching a literal outside the subject union is
  reported as unreachable. Open literal unions require `_` (same as open
  variants). Scope is exhaustiveness/coverage only — no new inference.

- 15.3 — bool singletons: `true`/`false` infer singleton rows like other
  literals, so comptime guards and literal-union machinery treat all three
  literal kinds uniformly. Requires a bool literal row-entry base kind;
  `Bool`-typed APIs are unaffected via the R3/R4 widening rules.

- 15.4 — comptime const-folding of base operations: `c = 1 + 2` infers `3`
  (singleton) instead of widening to `Int` (the recorded watch item in
  `literal-types.md`). Fold only comptime-known operands of the built-in
  operator subset; everything else keeps R4 widening.

- 15.5 — literal-argument diagnostics completion: close the deferred
  `open("x", 5)` gap (a base-kind-mismatched literal argument against a
  literal-union domain reports membership failure instead of deferring silently)
  and fix the recorded double-report wart (an overlapping-label spread merge
  reporting both `duplicate-spread-label` and `unresolved-binding`).

Done when:

- a binding annotated with a literal union accepts a member literal and rejects
  a non-member literal and a wide base-typed value, all with structured
  diagnostics
- a narrower literal union widens into a wider one at a boundary; fixtures lock
  each direction
- ~~bare number/text literal inference still yields `Int`/`Text`~~ superseded:
  bare literals infer open singleton rows that widen to `Int`/`Text` at
  boundaries and base operations (the 2026-06-30 flip; `literal-types.md`)
- a `?>` match on a closed literal union is non-exhaustive unless every member
  or a `_` is covered; an out-of-union literal arm is reported unreachable;
  fixtures lock both

## Milestone 16: Comptime Evaluator

Status: in progress

Goal: the comptime evaluator that M14.2's `comptime.evaluation-unsupported`
diagnostic stands in for — the engine that resolves `Type::Deferred` sites by
running comptime-position expressions at check time. Design:
`../../docs/ language-spec.md` → "Comptime, literal types, and labels" and the
`comptime-evaluator-design` notes (two-stage staging; `ComptimeValue` domain
whose liftable arm is `Value`; types reify as the checker's `crate::ty::Type` IR
— no second representation; camelCase reflection `typeOf`/`keysOf`/`fieldsOf`/
`tagsOf`; specialization-time/demand-driven evaluation reusing `Deferred`). The
evaluator lives as a module **inside `aven-check`** (checker → evaluator →
checker for types) until it earns its own crate.

Built tooling-first as the smallest honest engine first, growing one comptime
position at a time. No `@param` specialization or monomorphization in the first
slice.

Slices:

- 16.1 — thinnest reflection slice: resolve the simplest `Type::Deferred` /
  `comptime.evaluation-unsupported` case — a **capitalized binding whose RHS is
  a reflection call on a concrete type**, starting with `keysOf` on a record.
  The evaluator runs `keysOf` at check time, reads the record's field labels,
  and reifies the result as a literal-union `Type::Variant` (the type-position
  face of a comptime label set), so the binding gets a concrete type with no
  `Deferred` and no unsupported diagnostic. A new `ComptimeValue` domain
  (minimal: enough for a label set and a reified `Type`) and an evaluator module
  in `aven-check`. `keysOf` on a non-record concrete type is a structured
  diagnostic; a non-concrete argument defers (M11 discipline). No `@param`, no
  specialization, no other reflection functions yet. Done: `aven-check` now
  evaluates `keysOf(<closed record type>)` in comptime type position, reifies
  the sorted field-name set as a closed literal-union variant, defers
  non-concrete subjects without diagnostics, and reports
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
  small and honest — a parameter reference, a reflection built-in call
  (`keysOf`) on an in-scope value, a nested comptime-function call, or a literal
  type term; anything else flows to the existing deferred /
  `comptime.evaluation-unsupported` path. **Specialize (memoize) per distinct
  `(function, comptime-arg-tuple)`** — the monomorphization point and the cycle
  key. Recursion is bounded two ways: a visited-set over `(fn, args)` catches a
  specialization that depends on its own in-progress result
  (`comptime.evaluation-cycle`), and a fuel budget bounds deep-but-finite
  evaluation (`comptime.evaluation-limit`); either reports a structured
  diagnostic and recovers by treating the site as `Deferred`. The body's
  `ComptimeValue` reifies into `crate::ty::Type` (reuse `reify_type_position`).
  Out of scope: `@param` marker, parser changes, runtime-position
  specialization, computed keys, comprehensions, general value-parameter
  specialization (all → M16.3). Done: `aven-check` now specializes top-level
  lambda bindings in type-position calls, threads a minimal comptime parameter
  environment through bodies, memoizes by function and comptime argument tuple,
  reports bounded recursion with `comptime.evaluation-cycle` /
  `comptime.evaluation-limit`, and reifies `keyUnion(User)`-style results to
  concrete literal unions.

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
    `ExprKind::Index` in **value** position (`o[k]`) where the callee infers to
    a concrete record and the single arg is a **comptime-known label** → that
    field's type; defer otherwise (do not disturb the type-position `Array[Int]`
    `Type::Apply` meaning of the same node). The result type flows into
    inference (M11). Membership guarantees the field exists, so access is exact
    (no nullability in this slice).
    - Out of scope (later slices): record comprehensions (`{ keys -> k; ... }`),
      `pick`/`omit`, key-**union** access (`o[k]` over a key set → field-type
      union), runtime-`Text`-key access (→ nullable), computed transforms
      (`[k]=v`, `-[k]`, `[k]->[k2]`), other reflection functions. Done:
      `aven-parser` now treats `@lowercase` as a declaration-only comptime
      parameter marker with structured recovery, `aven-fmt` round-trips `@key`,
      and `aven-check` evaluates literal comptime arguments, specializes
      `keysOf(r)` domains from runtime argument types, reuses literal-union
      membership for out-of-domain keys, and infers exact field types for single
      computed-key record access when the key is comptime-known.

- 16.4 — record comprehension + comptime unrolling (thinnest: `pick`): the first
  comprehension slice. Thinnest end-to-end target:

  ```
  pick = (o: {..r}, @keys: keysOf(r)@{}) => { keys -> k; (k, o[k]) }
  pick(user, @{"name", "email"})    # result type: { name: Text, email: Text }
  ```

  Pieces:

  - **Parser (`aven-parser`):** add a record-body **iteration** item
    `source -> binder; body` as
    `RecordEntry::Iteration { source, binder, body: Vec<RecordEntry> }` — `body`
    reuses `RecordEntry` recursively (iteration repeats sub-items; no parallel
    tree). Disambiguate from the existing rename `from -> to`: a trailing `;`
    (with sub-items) marks iteration, bare `a -> b` stays a rename. A `(k, v)`
    tuple in a record/comprehension body is an **add-entry** item (reuse the
    tuple `Element`; the checker interprets a 2-tuple as add-field). Thread
    `walk`/`resolve`/`names` (the binder is an ordinary binder) and `aven-fmt`
    (round-trip the iteration form).
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
  label-set comptime arguments for `keysOf(r)@{}`, checks each member against
  the literal-union domain, and unrolls record iterations so
  `pick(user, @{"name", "email"})` infers `{ name: Text, email: Text }` while
  non-concrete key sets defer.

- 16.5 — postfix collection-type sugar `X[]` / `X@{}`: a trailing **empty** `[]`
  or `@{}` after a type is sugar for the named collection generic (decided
  2026-06-20) — `X[]` ≡ `Array[X]`, `X@{}` ≡ `Set[X]` (`Set`/`Array` are already
  builtin types). Non-empty `X[a]` stays type application. Desugar to the
  named-generic application (reuse the `Array[a]`/`Set[a]` path; **no new Type
  IR variant**), which also fixes the current loose `X[]` → `Apply{X, []}`
  lowering. `pick`/`omit`'s key parameter becomes `@keys: keysOf(r)@{}` (==
  `Set[keysOf(r)]`), matching the `@{...}` set value; update the checker's
  `literal_union_domain_row` to unwrap `Set[<literal union>]`. Parser + fmt
  round-trip the postfix forms; the `@{}` postfix is the empty set adjacent to a
  type (mirroring the empty `[]` postfix), distinct from a `@{...}` set literal.
  `aven-parser` + `aven-fmt` + `aven-check`.

  Done: `aven-parser` desugars empty postfix `[]` and adjacent empty postfix
  `@{}` to existing `Array[...]`/`Set[...]` applications, `aven-fmt` round-trips
  both postfix spellings, and `aven-check` unwraps `Set[<literal union>]` for
  comptime key-set domains used by `pick`.

- 16.6 — `omit` via bulk computed delete `-keys`: the `pick` dual for closed
  record transforms. A bare delete name that resolves to a comptime label set
  deletes every member from the current closed row, while ordinary static
  deletes like `-password` keep their existing single-field behavior.
  Out-of-domain key sets remain rejected by the existing `@param` literal-union
  membership check.

  Done: `aven-check` now resolves bare delete names to in-scope comptime label
  sets before falling back to static delete, applies the existing closed-row and
  absent-field rules to bulk deletion, preserves static field delete behavior,
  and locks `omit(user, @{"name"})` plus out-of-domain `omit` fixtures.

- 16.8 — comprehension guards for filtered record unrolling:
  `source -> binder, guard; body` evaluates a small comptime predicate language
  per unrolled member. `set.has(k)`, `!`, `&&`, and `||` produce internal
  comptime `Bool` values; `true` folds the body, `false` skips it, and anything
  unsupported or not comptime-known defers through the existing row-entry path.

  Done: `aven-parser` carries `guard: Option<Expr>` on `RecordEntry::Iteration`
  and preserves rename disambiguation, shared AST walkers/name resolution
  include the guard in binder scope, `aven-fmt` round-trips guarded
  comprehensions, and `aven-check` filters unrolled record members so
  `omit2(user, @{"name"})` infers `{ email: Text }`.

- 16.9 — type-position record comprehension foundation: comptime functions whose
  body is a record comprehension can now specialize in type position and lower
  to a record type. The evaluator threads parameter bindings into annotation
  lowering, `fold_iteration_entry` unrolls closed `keysOf` label sets in
  annotation mode, and computed type-position field reads like `object[k]`
  resolve to the selected field type. This enables the identity record type map
  (`clone(User)`) while leaving optional computed field modifiers for the next
  slice.

  Done: `clone = (object) => { keysOf(object) -> k; (k, object[k]) }` applied to
  a closed record type lowers to the corresponding closed record type in
  `aven-check`; open or unknown subjects defer without diagnostics, and
  non-record `keysOf` subjects reuse the existing reflection type-mismatch
  diagnostic.

- 16.10 — computed field-add for record type maps: record entries can now add
  fields with computed keys via `[k]: v`. Parser, formatter,
  reference/name/scope walkers, LSP token handling, compiler references, and
  checker row folding all thread `RecordEntry::FieldComputed`; in annotation
  mode the checker resolves the computed key with `comptime_known_label` and
  folds `object[k]` through the M16.9 type-position index path.

  Superseded by N3: optionality no longer lives on field labels, so `partial`
  now uses `partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }`.

- 16.11 — `required` type modifier (strip Optional): Restored by N5. The old
  implementation stripped optional field flags; after N3 removed field-level
  optionality, `required` is now expressed as a comptime type map using prefix
  `!` to strip the `Optional` wrapper from each field type:
  `{ keysOf(object) -> k; [k]: !object[k] }`.

- 16.12 — `tagsOf` reflection for variant constructor tags: `tagsOf(variant)`
  mirrors `keysOf(record)`, reflecting a closed variant type's constructor tag
  names into the same comptime label-set value so it works in type-position
  bindings, `@param` domains, and record-comprehension iteration sources.

  Done: `aven-check` now evaluates `tagsOf(<closed tag variant type>)` through
  the shared comptime label-set machinery, reifies it as a sorted closed
  string-literal union, dispatches `tagsOf` alongside `keysOf` for iteration
  sources, and reuses the existing reflection type-mismatch diagnostic with
  variant-specific wording for concrete non-variant subjects.

- 16.13 — `typeOf` reflection from value expression to static `Type`:
  `typeOf(value)` asks the checker for a value expression's inferred static type
  and reifies the normalized, concrete result as a comptime `Type`, so it
  composes with the existing type modifiers and reflection functions.

  Done: `aven-check` now dispatches `typeOf` as a value-expression reflection
  built-in distinct from `keysOf`/`tagsOf`'s type-subject label reflection. The
  evaluator infers the argument through a checker hook, resolves/defaults and
  normalizes the result, reifies concrete resolved types, and defers unresolved
  subjects without diagnostics. This enables `partial(typeOf(config))` and
  direct annotations such as `T = typeOf(config)`. The current hook uses a fresh
  top-level environment, so top-level bindings and self-contained literals work;
  subjects depending on active local bindings still defer until a follow-up
  threads the active `TypeEnv` into the query, matching the M16.9-style context
  threading.

  Done when:

- `Keys = keysOf(SomeRecord)` lowers to the literal union of that record's field
  names, usable as a type, with no `Deferred` and no
  `comptime.evaluation-unsupported`; a fixture locks it
- `keysOf` on a concrete non-record type produces a structured diagnostic; a
  `keysOf` call whose argument is not yet concrete defers without diagnostic
- the evaluator is a self-contained module in `aven-check`; reified types are
  the checker's own `Type` IR (no parallel representation)
- `keyUnion = (r) => keysOf(r)` with `Keys = keyUnion(User)` lowers `Keys` to
  the literal union of `User`'s field names, usable as a type, with no
  `Deferred` and no `comptime.evaluation-unsupported`; a fixture locks it
- a comptime function applied to a non-concrete type argument defers without a
  diagnostic; a self- or mutually-recursive comptime function that cannot
  resolve is bounded and reports `comptime.evaluation-cycle` (or
  `comptime.evaluation-limit`); fixtures lock both
- `@key` parses as a declaration-only comptime parameter (a `Param` comptime
  flag), `aven-fmt` round-trips it, and `@` outside a parameter declaration
  diagnoses; fixtures lock parse + fmt
- `get = (o: {..r}, @key: keysOf(r)) => o[key]` with `get(user, "name")` types
  as the `name` field's type; `get(user, "phone")` reports the out-of-domain
  membership diagnostic; `o[k]` for a comptime-known label on a concrete record
  yields the field type and defers otherwise; fixtures lock each
- a record-body iteration `source -> binder; body` parses to
  `RecordEntry::Iteration` (distinct from rename), `aven-fmt` round-trips it,
  and the binder resolves as an ordinary binder; fixtures lock parse + fmt
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

The tooling skeleton is in place, the semantic type IR and value-inference
engine landed (M10, M11), Hindley-Milner generalization is complete (M12), row
polymorphism is complete (M13), comptime tooling-first slices are underway
(M14), and literal types are underway (M15). The remaining hard semantic systems
are still deliberately out of scope for this plan.

Phase 2 work not planned here:

- comptime evaluation _engine_ (the staged interpreter; M14 covers only the
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
- reserved operator starts `=`, `:`, `.`, `?`, and `@` produce lexer diagnostics
  for unknown runs instead of silently splitting
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
  sequential block bindings, and match-arm pattern binders; LSP go-to-definition
  uses it before falling back to the top-level declaration list
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
  compare structurally when their arities match. Local checking and inference
  now share parser-backed scoped known/unknown bindings. Unannotated sequential
  locals acquire concrete synthesized types when possible; unresolved locals and
  pattern binders still block top-level fallback. Expected function annotations
  seed unannotated lambda parameters and check lambda return values. Contextual
  block checking now uses prefix locals to check final expressions, including
  final calls. Contextual match checking now pushes the expected result type
  into each arm body; guarded match arms check each guard against `Bool`. Simple
  variant patterns use a known literal variant subject type to seed payload
  binders, direct constructor values check against literal variant rows, and
  unannotated match expressions synthesize a concrete type when their arm body
  types agree. At embedded-script sizes whole-module re-inference is cheap, so
  consuming artifact invalidation for inferred results stays deferred until
  profiling shows it pays off.
- Consolidation C2: LSP hover now shows inferred types for unannotated bindings,
  sourced from compiler snapshots and building on C1's inferred-types API.
- Consolidation C3: LSP completion now offers identifier names from in-scope
  locals, top-level declarations with inferred-type detail, and builtin type
  names. Type-directed field, record-label, and tag completion remains a later
  slice.
- Consolidation C6: goto-definition and completion local-scope queries now share
  one position-scoped traversal that yields visible bindings plus the binder, if
  any, under the cursor.
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
  as unsupported for now. Bare string/number literal inference initially
  remained `Text`/`Int`; the 2026-06-30 flip (R1–R6 in
  `../../docs/literal-types.md`) later made value-position literals infer open
  singleton rows that widen at boundaries and base operations.
- Milestone 15.2 done: closed literal-union matches reuse the variant
  exhaustiveness path, literal arms cover matching members, open literal unions
  require a default arm, and out-of-union literal arms report
  `type.unreachable-match-arm`.

### Current queue (updated 2026-07-04)

The 2026-07-02 queue is done through HTTP: Q, S, J, X (examples half), K,
15.3–15.5, and H1+H2 all landed 07-02/07-03. Quoted record field names also
landed (an X-discovered gap), and dynamic JSON (Milestone J2, below) landed
07-04. What remains:

- the live nvim sweep (Milestone X's other half — user-driven)
- **Milestone F — formats and decode ergonomics: done 2026-07-05** (see below) —
  map indexing (`?v`), Yaml/Toml formats on shared `text_format` machinery,
  `text.decode(Fmt, T)` method form; spec consolidated on `decode` (bracket type
  application and `.parse` removed). Open question parked there: a
  format-neutral rename for the dynamic `Json` variant
- **Milestone V — type-artifact statics (platform subset): done 2026-07-05**
  (see below) — `Json`/`Map` are genuine type values carrying statics; the
  `json_namespace_target` shape-sniff is deleted
- X-discovered gaps: runtime variant/set spread in value position;
  `partial(User)`/`required(...)` as standalone comptime bindings. (The
  "match-arm layout ergonomics" gap is fixed 2026-07-05: it was never layout —
  `parse_match_pattern_term` used `at_item_boundary`, whose previous-is-Dedent
  clause rejected any arm following a block-bodied arm.)
- H3 open questions (recorded under Milestone H, not scheduled)
- the Milestone IO watch item: define the bare write tier in terms of the Result
  handles so the two tiers cannot drift
- Milestone Z — modules and imports: **Z5 bare library names / embedded std done
  2026-07-11** (see below); versioned packages remain open as package resolution

## Milestone N — null/undefined model

- N1 done: `true`, `false`, `null`, and `undefined` are reserved value keywords.
  `true`/`false` infer and evaluate as `Bool`; `undefined` is the renamed unset
  empty with type `Undefined`; `null` is a distinct deliberate empty with type
  `Null`. The old `Nil` builtin/value surface is removed from checker/tooling
  fixtures. Postfix `T?` still means the existing `Type::Nullable(T)` shape and,
  for N1, still admits `Undefined`; prefix `?`, `Optional`, optional-field
  changes, and spread semantics remain deferred to later N milestones.
- N2 done: prefix `?T` now lowers to `Type::Optional(T)` and admits `undefined`;
  postfix `T?` remains `Type::Nullable(T)` and now admits `null`. The composed
  `?T?` form normalizes as `Optional(Nullable(T))`, subtype widening flows from
  `T`, `?T`, and `T?` into `?T?`, and matches peel the required
  `undefined`/`null` arms before binding the payload.
- N3 done: the optional field flag and `x?:` / computed-field optional marker
  syntax are removed. Record rows always carry `name: T`; a record literal may
  omit a field only when the normalized field type is `Optional`, so
  `{ name: Text, phone: ?Text }` accepts `{ name: "Ada" }` while non-`Optional`
  missing fields keep the existing `type.missing-field` diagnostic. `partial` is
  now written as `{ keysOf(object) -> k; [k]: ?object[k] }`; N5 later restores
  `required` with prefix `!`.
- N4 done: record spreads are undefined-transparent for optional patch fields.
  When an incoming spread field is `?T` and the base already has that label, the
  base field type survives while the present `T` is checked against it; ordinary
  and nullable values still overwrite through the existing spread rules.
  Explicit value-record fields such as `x: undefined` now emit
  `record.redundant-undefined` and suggest omission or `-x` deletion.
- N5 done: `!` now neutralizes `?` in type position, independently on the
  optional and nullable sides. Prefix `!T` strips the normalized outer
  `Optional`, postfix `T!` strips `Nullable` while preserving any outer
  `Optional`, and compositions such as `!?T?!` lower to existing `Type` IR
  shapes without adding a new type variant. This restores `required` as
  `{ keysOf(object) -> k; [k]: !object[k] }`, so `required(partial(User))`
  lowers back to `User`.
- N6 done (2026-07-01): value indexing is typed. `array[i]` returns `?a`
  (Optional element — out-of-bounds is `undefined`, matching the runtime); tuple
  indexing requires a comptime-known index and projects the exact element type
  including literals (`("Ada", 36)[1] : 36`); out-of-range and non-comptime
  tuple indexes report structured diagnostics.
- N7 done (2026-07-01): field access peels `Optional`/`Nullable` wrappers,
  re-wraps the result, and honors `?.`: a plain `.` through an empty-wrapped
  receiver reports `type.unguarded-empty-access`, naming the receiver expression
  and spanning the `.field`, with `?.`/`??`/match repairs suggested.

## Milestone T — editor type intelligence

- T1 done: LSP completion now recognizes field-access position for name
  receivers and asks `aven-check`/`aven-compiler` for the receiver record's
  fields instead of inspecting row internals in the language server. Field items
  render their field type as completion detail, `Optional` and `Nullable`
  receiver wrappers are peeled by the checker query, and unknown or non-record
  receivers keep the previous identifier-list fallback.
- T2 done: LSP inlay hints now render cached inferred types for unannotated
  binders as `: Type` at the end of the binder name. The server advertises
  `inlayHintProvider`, answers `textDocument/inlayHint` from the stored semantic
  snapshot without re-parsing or re-checking, and, because the current snapshot
  also contains declared annotation types, suppresses hints for binders that
  already have a written annotation.
- T3 done: LSP signature help now advertises `(` and `,` triggers and answers
  `textDocument/signatureHelp` from the cached parse plus inferred-type
  snapshot. Name callees resolve through the shared definition query, callable
  types are exposed through the checker/compiler `function_signature` accessor
  with `Optional`/`Nullable` wrappers peeled, and the active parameter is
  counted from depth-aware top-level commas inside the enclosing call.
- T4 done: the inferred-type snapshot now records concrete expression spans in
  addition to binder spans, and `type_at` returns the narrowest containing span
  so hover can target calls, literals, records, fields, indexes, and other
  inferred sub-expressions without regressing name hover. Expression entries are
  recorded only when the type is concrete at inference time after
  resolve/default/normalize; expressions that become concrete only through later
  unification are still omitted until a future record-then-resolve pass.
- T5 done: LSP field completion and signature help now query the cached
  expression-type snapshot at the receiver/callee boundary before `.` / `(`, so
  non-name expressions such as call results, index results, and higher-order
  call callees participate without re-parsing or re-checking. Bare-name
  definition resolution remains only as a fallback when the positional snapshot
  has no type, and signature labels prefer the callee source text while
  preserving name-callee labels.
- T6 done: LSP completion now recognizes direct annotated construction sites:
  record literal binding values offer missing declared labels with field type
  details, and variant set literal binding values offer declared `@` tags. The
  server advertises `@` as a completion trigger, uses checker/compiler accessors
  for record fields and variant tags instead of row destructuring, and falls
  back to identifier completion when the cursor is inside an existing entry
  value or when no declared expected shape is available. Nested construction
  sites inside calls, tuples, or other expressions remain future work because
  they need expected-type propagation.
- T7 done: identifier completion keeps working during parse errors (recovery
  keeps the cached snapshot usable) and offers host globals seeded from
  `aven-host::standard_check_globals()`.
- T8 done: literal-argument completion triggers on `"` and is quote-aware, so
  `File.open(path, "` offers the mode literals from the parameter's
  literal-union domain.
- T9 done: field completion works on host-record globals and records carrying
  comptime fields (`File.` completes `open`).
- T10 done: field completion through `Optional`/`Nullable` receivers inserts the
  `?.` operator via an additional text edit when the receiver is empty-wrapped
  and the typed operator is `.`; already-null-safe and plain-record receivers
  are unchanged.
- Quick fixes so far: a colliding spread offers an overwrite-merge (`:..`)
  rewrite; a dropped `Result` value warns and offers a `?!` insertion.

## Milestone E — tree-walking evaluator

Status: in progress

Goal: make parsed Aven programs executable through a direct AST evaluator before
the later bytecode/runtime work.

- E1 done: added `aven-eval`, a tree-walking evaluator over parser AST nodes
  with runtime `Value` support for `Int`, `Float`, `Text`, `Bool`, `undefined`,
  and `null`. The first slice evaluates literals, grouping, unary `-`/`!`, core
  arithmetic, numeric and equality comparisons, boolean short-circuiting, and
  text concatenation. `aven run <path>` now parses a file, renders parse/runtime
  diagnostics through the existing CLI renderer, and prints the last expression
  value on success.
- E2 done: added evaluator environments for sequential module bindings,
  block-local bindings, name lookup, and block result values. Item evaluation is
  sequential: bindings are visible only to later items, a module value is
  produced only by a trailing expression, and a block with no trailing
  expression evaluates as `undefined`. Block scopes can shadow outer bindings
  without leaking mutations back out. Forward references and mutual recursion
  remain out of scope until E3 closures; a reference to a later binding reports
  `runtime.unbound-name`.
- E3 done: added lambda closures and function calls. Closures capture shared
  environment scopes, so top-level function bodies see sibling functions added
  after the closure was created, enabling self and mutual recursion once E5
  match adds base-case branching. This letrec-style behavior applies to
  functions; eager value forward references such as `a = b` before `b = 1` still
  report `runtime.unbound-name`.
- E4 done: added runtime record and variant values. Records preserve insertion
  order for display while comparing structurally by field name, and the
  evaluator now handles record construction, spread/overwrite, delete, rename,
  shorthand, computed fields/deletes, field access, and text-key record
  indexing. Variant tags evaluate as `@Tag`/`@Tag(payload...)` values. Missing
  field lookup reports `runtime.missing-field`; nil-safe access, record
  comprehensions, and tuple/array indexing remain explicit `runtime.unsupported`
  follow-ups.
- E5 done: added runtime `?>` pattern matching over literals, wildcards,
  nullable empties, variant tags and payloads, record field patterns, and guard
  expressions. Match evaluation reports `runtime.no-match` if the checker safety
  net is needed. With match base cases available, self and mutual recursion are
  now demonstrable end to end through `aven run`.
- E6 done: added runtime arrays, tuples, and sets. Arrays are ordered and index
  out of bounds to `undefined`; tuples are fixed-arity and report
  `runtime.index-out-of-bounds`; sets deduplicate by structural equality while
  preserving first-seen display order and compare order-independently. Tuple
  patterns now bind tuple elements in matches, `?.` propagates null/undefined
  receivers, and `??` short-circuits to the left value when it is present.
- E7 done: added the eval-side platform boundary with host-injected globals and
  native functions. This first shipped as ambient `Platform.Console.log` through
  ordinary record field access, with the stdout effect implemented in the CLI
  host and native failures reported as `runtime.platform-error`; CLI IO Phase 1
  later removed that namespace in favor of bare `write`/`writeLine`.
- E8 done: added first-class structured logging, initially as `Platform.Log`.
  `aven-eval` owns logger semantics, OTel-aligned levels/severity numbers, child
  loggers, context merging, W3C trace-context fields, and the host-agnostic
  `LogSink` trait; the CLI host owns stdout JSON-line output, timestamps, and
  `/dev/urandom` root trace/span id generation. CLI IO Phase 1 later kept
  `logger` ambient and removed `Platform.Log`. Deferred: per-child span-id
  generation, full `tracestate` semantics, and HTTP `traceparent` header
  extraction when the HTTP platform lands.
- E9 done: `aven run` now injects a host-curated ambient prelude as ordinary
  base-scope bindings. The root structured logger is available directly as
  `logger` (originally also through `Platform.Log`, since removed); normal
  scoping still applies, so user bindings may shadow prelude names. Roc-style
  selective imports are deferred until a module system exists.
- E10 done: runtime record comprehensions now evaluate through the shared record
  entry folder, so tuple-emits like `(k, object[k])` can insert or replace
  fields across comprehension iterations. `aven-eval` also provides the pure
  `keysOf` intrinsic for record labels and `.has` methods on Set/Array values
  through the existing field-access-plus-call path. The parsed single-identifier
  binder iterates Set/Array elements or Record field labels as `Text`; the
  spec's `(k, v)` tuple-binder form is not parsed yet and remains deferred.
- E11 done: types are first-class runtime values, mirroring the Layer-2 comptime
  premise (Zig-style, types as values). `aven-eval` adds one opaque
  `Value::Type` (a bare name; the real type IR stays in `aven-check`, no
  dependency added) and binds the atomic primitive type names (`Bool`, `Float`,
  `Int`, `Null`, `Text`, `Undefined`, `Unit`) as intrinsics next to `keysOf`,
  seeded before host globals so a user binding may shadow them. Record-as-type
  reuses `Value::Record`, so `User = { name: Text }` evaluates to a record of
  type-values and the canonical annotated `pick`/`omit` programs now run
  honestly with no type-alias erasure. `dbg` is a CLI-host native that writes
  each argument's `Display` to stderr and returns its single argument unchanged,
  keeping stdout clean. Function types (`->`), open rows (`{..r}`), and type
  application (`Array[a]`) only make sense in the full type language and appear
  only in ignored annotations; in bound value position they remain unsupported
  via the existing `runtime.unbound-name` / `runtime.unsupported` paths. A
  staged Core IR with type _erasure_ is the eventual VM-phase answer (deferred
  to the VM milestone).
- E12 done: error propagation operators `?^` (`ExprKind::Propagate` /
  `PropagationMode::ReturnError`) and `?!` (`Panic`) evaluate. `Result` stays
  the ordinary tagged value `@Ok(v)` / `@Err(e)` (no dedicated Result value);
  both operators just inspect the `Value::Tag`. The mechanism is a control-flow
  channel: the evaluator's internal result type migrates to
  `type Eval = Result<Value, Flow>` with
  `Flow::{Fail(Vec<Diagnostic>), Propagate(Value)}`, so `?^`'s non-local early
  return bubbles through `?` automatically. `Flow::Propagate` is caught at
  exactly two boundaries — the closure body in `eval_call` (the `@Err` becomes
  the function's return value) and the top-level item loop (the `@Err` becomes
  the program value and stops further items). Blocks deliberately do _not_ catch
  it: `eval_block` lets `Propagate` pass through so a `?^` inside a
  binding-value block early-returns the enclosing function rather than landing
  in the binding. Existing `Flow::Fail` recovery (collecting diagnostics across
  items) is preserved. `?^` on `@Ok(v)` yields `v`; `@Err` early-returns; `?!`
  on `@Err` raises a new `runtime.panic` diagnostic embedding the payload's
  `Display`; either operator on a non-Result raises `runtime.type-error`.
  Deferred: annotated error-type fitting / `mapError` (a checker concern) and
  any finer block-level exit semantics — function-level propagation is what's
  implemented.
- E13 done: CLI IO Phase 1 replaced the old standard platform namespace with the
  bare panic-on-error convenience tier. Standard globals are now `logger`,
  `dbg`, `write : Text -> {}`, `writeLine : Text -> {}`,
  `readLine : () -> ?Text`, and `readAll : () -> Text`; `Platform`/`Console` are
  no longer seeded. `logger` remains ambient and
  `aven-host::standard_check_globals()` is the checker/LSP source of truth.
  `aven run --log <stdout|stderr|path|syslog>` selects the logger sink (`stdout`
  default; files append; `syslog`/`journald` are explicit not-yet-implemented
  stubs) and `--log-format <json|text>` selects JSON lines or a simple
  `LEVEL message key=value` rendering. A final `@Err(...)` program value now
  prints to stderr and exits non-zero.

## Milestone P — typed platform boundary

Aven's differentiator is a type-safe host/script boundary: a host (or a
Rust-implemented library) registers named values with their Aven types, and
`aven check` type-checks uses of those names. This milestone builds that
boundary in thin, self-contained slices.

- **P1a done (`aven-check` only).** The checker can seed a typed global
  environment via `check_module_with_globals(module, globals)`, where
  `globals: &[(String, Type)]` are monomorphic host/library values;
  `check_module` delegates with `&[]`. Seeds flow into the existing top-level
  `value_types` map (each as `TypeScheme::mono`) for names no user declaration
  claims, so a user top-level binding **shadows** a seed (runtime-prelude
  scoping). Seeded names are then checked by the **existing** call/field/arity
  machinery — both the directed `check_value_against` path and the inference
  `Name` path read them (the latter via a `value_types` fallback in
  `infer_name_reference`, and seeds are populated before user-declaration
  inference so a binding like `x = logger.info` resolves the global).
  Statement-position calls and field accesses are now checked against a
  _concretely-known_ callee/receiver type
  (`check_value_call`/`check_value_field_access`), surfacing argument/arity and
  missing-field errors through the existing `report_*` helpers instead of
  silently deferring; an unknown/free receiver keeps today's permissive
  behaviour, so non-seeded names produce no false errors. A small public type
  builder surface
  (`build::named/text/int/float/bool/unit/function/record/ open_record/optional/nullable`)
  lets hosts and tests spell Aven types in Rust without reaching into row
  internals; `TypeScheme` stays private. `keysOf`, `pick`, and `omit` remain
  checker-native/runtime comptime builtins — they are **not** host globals and
  are unchanged.
- **P1b done (`aven-host` + wiring).** A new `aven-host` crate sits above both
  `aven-eval` and `aven-check` and holds a `Host` registry.
  `register(name, value, type)` binds a runtime value and its Aven type in
  **one** call (the same API for libraries and platforms, so the two halves
  can't drift); `register_runtime_only(name, value)` is the escape hatch for
  not-yet-typeable generics (runs but isn't checked);
  `eval_globals()`/`check_globals()` feed the evaluator (all values) and checker
  (typed only). Required capabilities are Rust traits the platform implements:
  `register_logger(sink, trace)` takes the existing
  `aven_eval::logging::LogSink` impl, builds the logger value, and registers it
  under `logger` with the statically-known type (`logger_type()`, built from
  `aven_check::build::*`). `aven-compiler` threads globals through
  `analyze_semantics_with_globals` / `check_source_file_with_globals` (the
  no-global versions delegate with `&[]`; the incremental artifact path is
  unchanged). This first demonstrated the typed boundary with a `Platform`
  namespace; CLI IO Phase 1 later removed that standard namespace and moved IO
  to bare globals. (P1b registered `logger` runtime-only until default params
  existed; D4 re-types it through the typed path — see below.) Remaining
  P-thread follow-ups: deriving generic host-fn types through the typed-fn
  adapter; the recursive `Logger` type (`child` returns an open record); and
  checking calls in expression (non-statement) position.
- **P2 done (`aven-host`).** A typed-fn adapter derives both the Aven `Type` and
  a marshalling `Value::native` from a monomorphic Rust closure, so a host fn's
  value and type can't drift — register a closure once and both halves are
  generated from the signature. `AvenMarshal` is the single source pairing a
  Rust type with its Aven type (`aven_type()` via `build::*`) and the
  conversions in both directions (`to_value`/`from_value`); implemented for
  `i64`→`Int`, `f64`→`Float`, `String`→`Text`, `bool`→`Bool`, `()`→`Unit`, with
  `from_value` returning a clear shape-mismatch `Err` ("expected Int, got Text")
  that surfaces as `runtime.platform-error` through the native path. A sealed
  `IntoHostFn<Args>` (macro-implemented for `Fn(A0..A3) -> R + 'static` where
  every type is `AvenMarshal`, arities 0..=4) yields
  `into_host_fn() -> (Type, Value)`: an all-required `Type::Function` plus a
  native that arity-checks (`expected N arguments, got M`), unmarshals each arg,
  calls the closure, and marshals the result. `Host::register_fn(name, f)`
  routes that pair through the existing `register`, so it lands in both
  `eval_globals` and `check_globals` with no new registry path. Verified end to
  end: `register_fn("add", |a: i64, b: i64| a + b)` makes `add(2, 3)` check and
  evaluate to `5` while `add("x", 3)` is a check-time type error. The
  then-existing `logger`/platform/`debug` registrations were **not** migrated in
  this slice (logger is a record of optional-arg methods and `debug` was
  generic; P4 later typed it through the ordinary register path, and CLI IO
  Phase 1 renamed it to `dbg`). Deferred: generic host-fn derivation (`Value`
  passthrough mapped to type variables), compound marshalling (records↔structs,
  `Vec`↔Array, `Option`↔`?T`, `Result`↔Aven `Result`), optional params via
  the adapter, arities above 4, and migrating the existing host regs.
- **P3 done (`aven-eval`).** `pick` and `omit` are now predefined runtime
  intrinsics alongside `keysOf`, seeded before host globals so a user binding
  may shadow them. Each takes `(record, labels)` — a `Value::Record` and a
  `Value::Set` of `Value::Text` labels (the shape `keysOf` and `@{...}` produce)
  — and returns a new record: `pick` keeps the fields whose names are in the
  set, `omit` removes them, both preserving the source record's field order; a
  label absent from the record is skipped (intersection semantics, lenient at
  runtime). Because a record _type_ is just a record whose values are
  type-values, the same natives run uniformly on data and type records with no
  special casing — e.g. `omit({ name: Text, email: Text }, @{"name"})` ⇒
  `{ email: Text }`. Wrong arity, a non-Record first arg, a non-Set second arg,
  or a non-Text set member each surface as `runtime.platform-error`. These are
  language builtins (like `keysOf`), **not** host-registered through
  `aven-host`. Deferred: the comptime-typed-builtin form — so
  `pick(user, @{...})` _infers_ the precise picked row without a user definition
  — which is a larger checker-side follow-on.
- **P4 done (`aven-check` + `aven-cli`).** Seeded host/library globals still use
  the existing `&[(String, Type)]` plumbing, but seeding now generalizes free
  named `Type::Variable`s (spelled by hosts as `build::var("a")`) into
  `TypeScheme` quantified metas before inserting into `value_types`. The
  existing `instantiate_scheme` read path freshens those metas per use site, so
  generic host/library functions type-check without a parallel generics
  mechanism: `debug : (a) -> a` (later renamed `dbg`) accepts any argument type
  while its result type still flows through inference and annotations. The CLI
  registers it through the typed `Host::register` path and no longer treats it
  as runtime-only. Still deferred: teaching the P2 typed-fn adapter to derive
  generic types from `Value` passthrough positions by assigning distinct
  per-position type vars, and migrating other host registrations where the
  adapter can own the type.
- **P5 done (`aven-host` + `aven-cli` + `aven-lsp`).** The standard host
  type-globals now live in one place: `aven_host::standard_check_globals()`
  returns the current standard host interface used by editor analysis (`logger`,
  `dbg`, `write`, `writeLine`, `readLine`, `readAll`). The LSP seeds semantic
  analysis with those globals, so diagnostics and cached inferred types cover
  host globals the same way `aven check` does; hover, field completion, and
  signature-help paths can now read host global types from the same `type_at`
  snapshot as user code. The CLI still owns the runtime values and effects, but
  its registered check globals are compared against `standard_check_globals()`
  in a drift-guard test so CLI and LSP host types cannot silently diverge.
- **P6 done (`aven-check` + `aven-host`).** Host comptime functions: a host can
  register a comptime type resolver alongside a value
  (`register_comptime_fn`/`register_comptime_resolver`; `HostComptimeFnSpec`
  records which parameters are comptime). The checker evaluates the
  comptime-known arguments (`ComptimeArg`) and asks the resolver for the call's
  result type. This types `File.open(path, mode)`: the mode literal
  (`"r" | "w" | "a" | "rw"`) selects a phantom-typed handle record, so read
  methods exist only on readable handles. A base-kind-mismatched literal such as
  `open("x", 5)` still defers today (see 15.5).

## Milestone D — default/optional parameters

Aven gains default/optional parameters so capabilities like `logger.info` (an
optional trailing fields argument) can be typed without falsely rejecting the
short call form (see Milestone P1b). The chosen surface is explicit defaults on
**lambda parameters**: `(msg: Text, fields: Record = {}) => ...`,
`(name = "world") => ...`. A parameter with a default may be omitted at the call
site. Sliced parser-first; semantics follow.

- **D1 done (`aven-parser` + `aven-fmt`).** `Param` carries a new
  `default: Option<Expr>`; `parse_lambda_params` parses an optional `= value`
  after the annotation in both the ordinary-identifier and comptime-param arms.
  The default is an ordinary **value** expression (parsed via the same value
  entry as a call argument, not the type-term parser), so it naturally stops at
  the `,` or `)` that delimits the parameter; the `Param`'s span extends to
  cover it. Defaults must be **trailing**: a required parameter following a
  defaulted one emits a recoverable `parse.required-param-after-default`
  diagnostic (primary label on the offending parameter, repair note) and parsing
  continues. `walk.rs` visits the default so name resolution sees it. The
  token-based formatter already renders `name: Type = default` /
  `name = default` with normal `=`/`:` spacing (round-trip is stable); no fmt
  rendering change was needed beyond a guarding test. The checker and evaluator
  ignore the new field for now, so a defaulted lambda still
  type-checks/evaluates with today's arity behaviour — a call that omits a
  defaulted argument still errors until D3 (acceptable for this slice).
- **D2 done (`aven-check`).** `Type::Function` carries a `required: usize`
  (`params[required..]` are the optional/defaulted trailing params; invariant
  `required <= params.len()`). Lambda inference derives `required` from the
  trailing-default count and type-checks each default expression against its
  parameter type: an annotated param's default reuses `check_value_against` (a
  mismatch is a normal `type.*` diagnostic on the default), an unannotated param
  infers its type from the default via unification. Calls arity-check the
  `required..=total` range — both statement-position `check_value_call` and
  inference `infer_call` accept an omitted trailing optional; arguments are
  checked against `params[0..args.len()]`. `report_function_arity_mismatch` grew
  a range message ("expected between {required} and {total} arguments…") for
  `required != total`, keeping the exact-count wording otherwise. Unify requires
  equal total length **and** equal `required` (conservative).
  `build::function_opt` lets a host spell optional trailing params (e.g.
  `logger.info` with an optional fields record). D3 = evaluator applies defaults
  at call time; D4 = re-type `logger`. Deferred: function subtyping (accepting a
  fewer-required function where a more-required one is expected) and standalone
  function-_type_ default syntax.
- **D3 done (`aven-eval`).** The closure now carries each param's optional
  default (`ClosureParam { name, default: Option<Rc<Expr>> }`). At call time the
  evaluator accepts `required..=total` args (`required` = leading params with no
  default; `total` = all params) and emits the runtime arity diagnostic — now a
  range ("expected between {required} and {total} arguments") when they differ,
  keeping the exact-count wording when equal. Provided args bind first; each
  omitted trailing param then has its default evaluated **in the call env, in
  order** (so a later default may reference an earlier param, e.g.
  `(x, y = x + 1)`), and only when omitted (a supplied arg never triggers
  default evaluation, so a failing default like `1 / 0` stays inert). Default
  failures propagate through the existing `Flow` channel. Native functions are
  unaffected (they default their own args in Rust).
- **D4 done (`aven-host` + `aven-cli`).** `logger` is now **typed** via
  `function_opt`: each level method (`trace`/`debug`/`info`/`warn`/`error`/
  `fatal`) is `(Text, ?{..}) -> Unit` — one required message, an optional
  trailing fields record — so `logger.info("msg")` and
  `logger.info("msg", { .. })` both check, `logger.info(42)` is a
  `type.mismatch` (Int vs Text), and `logger.info()` is a `type.mismatch` arity
  error ("expected between 1 and 2 arguments"). The CLI registers `logger`
  through the typed path (`host.register("logger", …, logger_type())`). This
  slice also restored a closed `Platform` record at the time, but CLI IO Phase 1
  later removed `Platform` from the standard globals. The typed host boundary
  now covers the required logging capability end to end. Remaining P-thread
  follow-ups: generic host fns via the typed-fn adapter (P2), the recursive
  `Logger` type (`child` still returns an open record), and expression-position
  call checking.

Deferred: writing a literal default inside a standalone function-_type_
annotation (e.g. `(Text, Record = {}) -> Unit` as a bare type) is out of scope.
A function type's optionality will be represented in the D2 type IR, derived
from the lambda, not from type-annotation syntax.

## Milestone IO — platform IO

Status: phases 1–3 done (phase 3 = Milestone J, landed 2026-07-02)

Goal: real input/output through the typed platform boundary, layered in tiers: a
bare panic-on-error convenience tier for scripts, `Result`-returning stream
handles for programs that handle failure, and codecs for structured data.

- Phase 1 done: bare tier + CLI plumbing. Standard globals `write`, `writeLine`,
  `readLine`, `readAll` (panic-on-error), `logger`, `dbg`;
  `aven run --log/--log-format`; stdout flushed before stdin reads; a final
  `@Err` program value prints to stderr and exits non-zero. (Also recorded under
  E13/P5.)
- Phase 2 done: Result-tier handles and files. `stdout`/`stderr`/`stdin`/
  `stdio` handle records whose `write`/`writeLine`/`readLine`/`readAll`/ `flush`
  return `Result[..., WriteError/ReadError/IoError]`; `File.open(path, mode)`
  with a literal-union mode and phantom-typed handles via host comptime
  resolution (P6), drop-RAII on unconsumed handles, and a must-use warning;
  `Http.get(url, ?{ headers, params })` with a streaming body handle. File
  handling lives in `aven-host` for reuse by other hosts.
- Phase 3 — JSON codec: next; specified as Milestone J below.

Watch item: the two write tiers (`writeLine` panics; `stdout.writeLine()`
returns a `Result`) are two implementations of one effect. Once the surface
settles, define the bare tier in terms of the handles
(`write = stdout.write (_)?!`-style) so the tiers cannot drift.

## Milestone Q — typed `?^` error propagation

Status: done 2026-07-02 (plus a follow-up: matching on a `Result`-typed subject
works via a row view — `subject_variant_row` — and a variant-row body fits an
inline `Result` return annotation through the boundary rule)

Goal: close the check/run soundness gap around `?^`. Today `infer_propagate`
unwraps `Result[a, e]` to `a` and discards `e`; nothing requires the containing
function to return a `Result`, so `aven check` passes programs that return
`@Err` where the checker inferred the success type (a runtime `type-error` when
the caller uses the value). The spec ("Errors and Partials") requires: the
containing function returns a compatible `Result`, and inferred error types
union across propagation sites
(`loadUser : Path -> Result[User, @{..FileError, ..JsonError}]`).

Decision (2026-07-02): **explicit `@Ok`** — a body that uses `?^` must have a
`Result`-typed final expression. The evaluator is unchanged: a correct body
already yields a `Result`, and `?^` early-returns `@Err` exactly as today.

Tasks:

- collect propagated error types per function body: each `?^` site contributes
  its subject's `Result` error type; union them with the existing variant-row
  join machinery (the match-arm row-union path)
- infer the function result as the body's `Result` type with the error side
  widened to the union of the body's own error row and all propagated rows
- diagnose a concrete non-`Result` final expression in a `?^`-using body with
  `type.propagate-needs-result` (label the final expression; note suggests
  wrapping it in `@Ok(...)`)
- `?^`/`?!` on a subject whose concrete type is not a `Result` reports a
  structured diagnostic instead of silently deferring; non-concrete subjects
  still defer (M11 discipline)
- `?!` stays exempt from the `Result`-body rule (panic tier; usable anywhere)
- annotated returns: each propagated error row must widen into the annotated
  error type by the existing boundary rule; a non-fitting error reports at the
  offending `?^` site (`mapError` remains future library work)
- module top level: a `?^` in a top-level item keeps today's behavior (the
  program exits with the error)
- update existing fixtures/tests that use `?^` in function bodies without a
  `Result` final expression — expected fallout of the new rule, done
  deliberately

Done when:

- a function using `?^` without a `Result` final expression is diagnosed with a
  repair note; adding `@Ok(...)` fixes it; fixtures lock both directions
- a `loadUser`-style body infers `Result[T, <union>]` with the error union built
  by row join; a fixture locks the rendered type
- `File.open(...)?^` + `readAll()?^` + string concatenation on the call result
  is a check-time error, not a runtime type-error
- annotated error types accept fitting propagated errors and reject non-fitting
  ones with the span on the offending `?^`

## Milestone S — structural consolidation

Status: done 2026-07-02

Goal: pay down the structural debt of the 06-18→07-01 sprint before it
compounds. No behavior change except where noted (escape diagnostics).

Tasks:

- split `aven-check/src/checker.rs` (6.6k lines, one impl block, 317 fns) into
  focused submodules behind the same `Checker` type — suggested seams: match
  checking, record rows/transforms, literal rows/boundaries, field
  access/indexing, diagnostics/reporting. Mechanical moves only; the existing
  suite is the guard
- one string-literal decoder: move decoding into `aven-parser` (the lexer owns
  string syntax), expose it, and delete the four copies
  (`checker.rs::string_literal_label`, `comptime.rs::string_literal_label`,
  `host_comptime.rs::decode_string_literal`, `aven-eval::decode_string_literal`)
- implement the spec's `\u{H}` escape in that one decoder, and make unknown
  escapes a lexer diagnostic (`lex.unknown-escape`) instead of silently passing
  the bare character through (today `"\u{41}"` silently yields `u{41}`)
- move the inline test modules out of `aven-eval/src/lib.rs` and
  `aven-lsp/src/lib.rs` if the churn stays mechanical; deeper splits of those
  files are their own later slice

Done when:

- no source file in `aven-check` exceeds ~2k lines; `cargo test` is untouched
  except for new escape fixtures
- exactly one string-literal decode implementation exists in the workspace
- `"\u{41}"` evaluates to `"A"`; `"\q"` reports `lex.unknown-escape` with the
  escape's span; fixtures lock both

## Milestone J — JSON codec (IO Phase 3)

Status: done 2026-07-02

Goal: the spec's headline glue workflow — typed JSON decode/encode.

Decision (2026-07-02): the decode target type is an ordinary trailing comptime
argument (`Json.decode(text, User)`), reusing the host-comptime resolver seam
(P6) exactly as `File.open`'s mode does — no type-application syntax.

Tasks:

- `Json.encode(value) : Text` — encode records, arrays, tuples and sets (as
  arrays), `Text`/`Int`/`Float`/`Bool`, and `null` (from `T?`);
  `undefined`-valued optional fields are omitted; a non-encodable value
  (function, handle, type, NaN/infinite float) is a structured runtime error
- `Json.decode(text, T) : Result[T, JsonError]` — parse, then shape-check the
  parsed value against the target type. `JsonError` is a named variant along the
  lines of
  `@{ @Parse({ message: Text }), @Shape({ path: Text, expected: Text, found: Text }) }`
  — keep the payload structured and the JSON path precise
- optional/nullable mapping: absent key → `undefined` for `?T` fields; JSON
  `null` → `null` for `T?` fields; `null` into a non-nullable field is a shape
  error
- checker side: a host comptime resolver types `Json.decode`'s result from the
  comptime type argument; a non-comptime-known type argument defers
- runtime side: `aven run` is checker-free, so decode shape-checks against the
  runtime type value (E11 records-as-types). This needs a minimal extension of
  the runtime type grammar — `?T`, `T?`, and `Array[T]` in value position should
  build composite `Value::Type` shapes instead of erroring — scoped to exactly
  what decode needs
- `Json` registers through `aven-host` like `File`/`Http`; the JSON parser
  dependency (or hand-rolled parser) stays behind the `aven-host` boundary
- defer: the one-argument dynamic `Json.decode(text)` form until a dynamic JSON
  value shape or `Map` exists (Milestone K)

Done when:

- `user = Json.decode(text, User)?^` checks with result type `User` and
  round-trips `Json.encode(user)`; fixtures lock encode and decode including
  optional/nullable fields
- decode errors carry the JSON path plus expected/found shapes
- the spec's `loadUser` (read + decode + `?^` + `@Ok`) checks and runs end to
  end — the combined Q + J payoff example

## Milestone X — example suite and live verification

Status: examples half done 2026-07-03; the live nvim sweep is still open

Goal: lock the sprint surface into committed, executable examples — "prefer
committed tests over ad-hoc checks" applied to the whole platform surface — and
verify the editor features live (unit tests bypass the LSP scheduler).

Tasks:

- an `examples/` set covering: a file read/transform/write pipeline, HTTP
  fetch + decode, `pick`/`omit`/`partial`/`required`, literal-union modes, `?^`
  chains ending in `@Ok`, and logger usage
- CLI integration tests `aven check` every example; hermetic examples (file IO
  in a temp dir, no network) are also `aven run` with output assertions; network
  examples are check-only in tests and runnable by hand
- a live nvim pass over hover, inlay hints, field completion (including `?.`
  insertion, `File.`, and mode-literal completion), signature help, and quick
  fixes; record findings rather than fixing inline
- fix or file everything the sweep surfaces

Done when:

- `cargo test` fails if any example stops checking (or running, for hermetic
  ones)
- the live sweep is recorded and every finding has a fix or a filed follow-up

Progress: the committed-examples half is done (2026-07-03). Eight examples under
`examples/` (hello, file-pipeline, records, literal-modes, json, http-fetch,
logging, errors) are locked by `crates/aven-cli/tests/examples.rs` — every
example must `aven check` cleanly; hermetic ones also `aven run` with output
assertions; `http-fetch` is check-only (network). `errors.av` demonstrates Q's
inferred error unions with no return annotation. The live nvim sweep is still
open. Gaps found while writing the examples (follow-ups):

- variant/set spread in **runtime value position** is unsupported —
  `colors = @{..a, ..b}` hits `runtime.unsupported` (the evaluator's set
  literals only take element entries)
- `partial(User)` / `required(partial(User))` as standalone comptime
  **bindings** report `comptime.evaluation-unsupported`; the same calls work in
  annotation position
- quoted record field names (`{ "content-type": v }`) are a parse error — the
  spec supports quoted non-identifier field keys; the parser does not yet
- multi-line match-arm bodies inside lambda bodies need fiddly indentation —
  layout ergonomics worth a look

## Milestone K — Map type

Status: done 2026-07-03

Goal: `Map[Key, Value]` — the runtime-key counterpart to records (spec → Maps).
Needed for headers, query params, grouping, and dynamic JSON.

Tasks:

- runtime `Value::Map` with insertion-order-preserving display and structural
  equality; construction via `Map.empty()` / `Map.from([(k, v), ...])` (literal
  syntax deferred)
- core operations: `get(key) : ?v` (matching the array-indexing rule), `set`,
  `delete`, `has`, `keys`, `values`, `entries`, `size`, `merge`
- checker: `Map[k, v]` as an ordinary generic application; operations typed
  through the existing host/builtin scheme machinery
- `record[runtimeTextKey]` stays deferred/dynamic — maps are the sanctioned
  runtime-key structure
- revisit `Http.get` headers/params and the dynamic `Json.decode(text)` form
  once Map exists

Done when:

- a grouping example (fold an array into `Map[Text, Int]`) checks and runs;
  `get` misses type as `?v`; fixtures lock the operation types

## Milestone H — HTTP methods

Status: H1 + H2 done 2026-07-03; H3 open questions recorded below

Goal: round out `Http` beyond `get`. The API is a flagship surface for the
language, so the design was settled first (user decisions 2026-07-03):

**Request headers/params — field-type domains at the comptime boundary.**
Headers and query params are multimaps (repeated names are legal), but Aven
deliberately has no untagged unions (R5; tagged sums only), and a general
`T → Array[T]` boundary coercion was rejected (it needs a runtime coercion,
breaking check/run independence, and `xs : Array[Int] = 5` masking bugs).
Instead: headers/params are plain record literals, and the `Http.*` host
comptime resolver checks **each field's type** against the domain `Text` or
`Array[Text]` — the same seam and check species as `File.open`'s literal-union
mode, lifted from literals to types. No new syntax, no new `Type` variant; the
runtime already accepts and normalizes both shapes. Diagnostics name the field:
"header `accept` must be `Text` or `Array[Text]`, found `Int`". This is not
platform-only special casing: it is the first user of the row-wide value
requirement the spec already sketches for open-row comprehensions, which should
later become a user-facing comptime feature.

**Response headers — `Map[Text, Array[Text]]`** (runtime keys, multi-valued;
`Set-Cookie` cannot be safely joined), keys normalized to lowercase. The
response record carries a native `first(name) : ?Text` convenience field
(record-of-natives, like file handles) for the common single-value read.

Slices:

- H1 — rework `Http.get` onto the designed surface: options
  `?{ headers: <field-domain record>, params: <same>, timeout: Int (ms) }`,
  per-field domain checking through the host comptime resolver (reading the
  reified options record type; non-concrete option types defer), response
  `{ status: Int, headers: Map[Text, Array[Text]], first(name), body }` (body
  stays the streaming handle). Update `examples/http-fetch.av`.
- H2 — `post`/`put`/`delete` (+ `patch` if free): one shared options record
  across methods, plus body options — `body: Text` (sent as-is) or
  `json: <encodable>` (Milestone J's `Json.encode`, sets
  `content-type: application/json`); `body` and `json` together is a
  resolver-checked error.
- H3 — open questions, recorded not solved: an order-preserving
  `params: [(k, v), ...]` alternate shape for the rare order-sensitive query
  string; `Int` header values (currently rejected — interpolate instead);
  redirect/TLS knobs (host defaults for now).

Progress: H1 landed 2026-07-03 — `Http.get` on the designed surface (per-field
`Text`/`Array[Text]` domains via `TypeOf` comptime args + the boundary-probe
check, unknown-option and optional-field rejection, `timeout`, response
`{ status, headers: Map[Text, Array[Text]], first, body }`, a TcpListener
wire-assertion harness). H2 landed 2026-07-03 — `post`/`put`/ `delete`/`patch`
sharing one validator and response builder; `body: Text` / `json: <value>` (J's
encoder; sets `content-type` only when absent; both together is a check-time and
runtime error); ureq upgraded 2.12 → 3.3 within `aven-host`, retiring H1's
case-varied repeated-header workaround.

Done when:

- a bad header/param field type reports the field-naming domain diagnostic;
  `Text` and `Array[Text]` fields both check and both reach the wire (repeated
  names sent repeatedly)
- `post` with `json:` round-trips against a local test server (or a hand-rolled
  listener in the test) including repeated headers
- response `headers.get("set-cookie")` types `?Array[Text]`;
  `first("content-type")` types `?Text`; fixtures lock the response shape
- the options record is one definition shared by all methods; `ureq` stays
  behind `aven-host`

## Milestone J2 — dynamic JSON (one-arg decode)

Status: done 2026-07-04

Progress: landed 2026-07-04. Recursive named variant definitions are accepted
(the lowering fixpoint is round-bounded; pure alias cycles still diagnose);
`subject_variant_row` unfolds named variant definitions one level, so `match`
over `Json` gets typed arms and exhaustiveness; hand-built constructor tags fit
`Json` boundaries; hover folds recursive expansions back to the name
(`display_named_definitions`). Decode's target argument became optional (one-arg
≡ `Json.decode(text, Json)`); parsing uses a custom serde visitor (direct
`serde` dep) so object key order is preserved and the `@Int`/`@Float` split
follows the number lexeme (i64 overflow → `@Float`); encode gains a structural
carve-out for the seven constructor tags (wrong payload shape stays an error).
`examples/dynamic-json.av` locks decode → match → re-encode. Note: in
checker-free runs the explicit `Json` target arrives as the namespace record;
`json_namespace_target` shape-sniffs it and must track `json_value`'s field
list.

Goal: `Json.decode(text)` for JSON whose shape is unknown at compile time — the
form the spec's layout example already uses. J deferred it until a dynamic value
shape existed; Map (K) closed that gap.

Design (user decisions, 2026-07-04): the dynamic value is a **recursive nominal
variant**, registered by `aven-host` exactly like `JsonError`:

```
Json = @{ @Null, @Bool(Bool), @Int(Int), @Float(Float), @Text(Text),
          @Array(Array[Json]), @Object(Map[Text, Json]) }
```

Arm names reuse the language's own type names. Numbers split `@Int`/`@Float`
(i64-fitting, fraction/exponent-free → `@Int`; else `@Float`) so IDs stay exact.
The one-arg form is sugar: `Json.decode(text)` ≡ `Json.decode(text, Json)`,
result `Result[Json, JsonError]` — no new API shape, just a named type the
existing target-type machinery understands.

Tasks:

- checker: allow a **self-referential named variant definition** — the
  definition-substitution fixpoint must leave self-references nominal instead of
  expanding forever, and the alias-cycle detector must only flag pure alias
  cycles (`A = B = A`), not recursion through a variant body
- checker: `subject_variant_row` unfolds `Type::Named` one level through the
  definitions table (the same row-view trick Bool and Result already use), so
  `match` over a `Json` subject gets arms, payload types, and exhaustiveness
- boundary: a hand-built tag (`@Int(5)`, `@Object(m)`) fits where `Json` is
  expected via the existing variant-fits-boundary rule routed through the
  definition's row
- host: the decode resolver accepts the one-arg form (types it as
  `Result[Json, JsonError]`) and `Json` as an explicit second argument; the
  runtime shape-decoder gains a `Json` target that builds the tag tree from
  parsed JSON
- host: `Json.encode` serializes Json-constructor tags back to JSON (structural
  carve-out from the reject-all-tags rule; applies recursively, so a record
  containing a `Json` subtree encodes) — decode → encode round-trips
- example: extend `examples/json.av` (or a new `dynamic-json.av`) with a decode
  of unknown-shape input, a `match` over the arms, and `@Object`/`Map.get`
  drilling; locked by the examples test

Done when:

- `parsed = Json.decode(text)?^` checks with `parsed : Json`; `match parsed`
  over the seven arms is exhaustive with typed payloads
- `Json.encode(parsed)` round-trips (modulo whitespace/key order)
- numbers land in the right arm (`1` → `@Int`, `1.5`/`1e10` → `@Float`, i64
  overflow → `@Float`)
- the recursive definition produces no cyclic-alias diagnostic, and hover/LSP
  render `Json` by name rather than an infinite expansion

## Milestone V — type-artifact statics (platform subset)

Status: done 2026-07-05

Progress: landed 2026-07-05 (claude-rust/opus slice). `HostGlobals` gained a
statics table (`register_type_with_statics` binds type definition + statics in
one host call); the checker resolves `Type.static` field access through
generalized schemes with the same shadowing rule as host-comptime fns; the
evaluator binds statics as `"Type.static"`-keyed globals consulted on
`Value::Type` field access, and `Map[k, v]` value-position application builds
`RuntimeType::Map`. `Json`/`Map` namespace records and the
`json_namespace_target` sniff are deleted; LSP completion/hover read statics via
`type_statics` as record-like fields.

Goal: implement the spec's statics model (Members and Methods: "`=` defines what
the type carries") for host-registered platform types. `Json` and `Map` are
currently namespace _records_ on both sides (checker `map_global_type()`, eval
`map_namespace()`/`json_value()`) — an implementation shortcut that diverges
from the spec ("`Json` is a type artifact, not a marker value") and becomes
observable when a type is passed as a value: `Json.decode(text, Json)` in
checker-free runs passes the namespace record, forcing the
`json_namespace_target` shape-sniff.

Tasks:

- named types can carry **statics**: registration binds the type definition and
  its statics (names + checker types + runtime natives) in one call; `Json` and
  `Map` globals become genuine type values whose field access resolves statics
  on both the checker and eval paths
- checker: `Json.decode` / `Map.from` type through the statics table instead of
  a record global; `Json.`/`Map.` completion lists statics; annotation position
  (`Map[Text, Int]`, `Json`) is unchanged
- eval: field access on `Value::Type(Named)` consults a statics registry;
  `Map[k, v]` type application in value position builds a composite runtime type
  value (decode support for Map targets may stay deferred)
- retire the `json_namespace_target` sniff — the decode target arrives as a type
  value in every mode
- out of scope: user-defined statics, instance-route statics (`myTask.zero`),
  field defaults, method slots/focus — those wait for type-artifact declarations
  in the language

Done when:

- `Map.from`/`Map.empty`/`Json.encode`/`Json.decode` check, complete, and run
  exactly as before; all examples pass untouched
- `Json.decode(text, Json)` runs without the namespace sniff (the sniff is
  deleted)
- `x = Json` / passing `Map` as a value yields a type value, not a record

## Milestone F — formats and decode ergonomics

Status: F1–F7 done (F1–F6 2026-07-05; F7 2026-07-10; user decision 2026-07-05:
the shared dynamic variant renames `Json` → `Data`)

- **F7 — honest hover/completion types for encode/decode sugar: done
  2026-07-10** (codex). The sugar records an applied method-view signature at
  the member-name span (`encode`: `Yaml -> Text`; `decode`:
  `(Json, User) -> ...`), so hover works, and the LSP's synthetic completion
  entries reuse the same shape — the `? -> Text` detail is gone. Receivers whose
  own type carries `encode` keep their real member type. The encode-fallibility
  spec divergence stays parked for the DT/codec revisit.

- **F6 — encode sugar on all checked receivers: done 2026-07-05** (codex). Root
  cause was not the probe: the statement checker (`check_value_call`) ran an
  ordinary field-access check on the `.encode` callee before call inference
  reached the desugar, and a _named_ annotation made the receiver a concrete
  closed record, so that pre-check reported `missing-field`. `check_value_call`
  now routes an applicable `.encode(Fmt)` through desugared inference first
  (guard `value_encode_sugar_receiver` shared with `infer_value_encode_call`),
  and the per-item pass dedupes exact code+span diagnostic collisions from the
  check-then-infer seam. LSP field completion offers `encode` on any value
  receiver without its own `encode` member; `decode` stays Text-only.

F4 progress: landed 2026-07-05 (codex; user decision 2026-07-05:
`value.encode(Json)` is the encode spelling). Decode's helpers generalized
(`format_member_name`/`format_member_hint`/`probe_receiver_type`) instead of a
parallel copy; `static_member_wins` keeps the direct `Fmt.encode(value)`
spelling on its ordinary path; a receiver whose own type carries `encode` keeps
field semantics; `type.encode-format` diagnostics; early arity check (encode has
no host-comptime resolver to catch it). LSP offers `encode` alongside `decode`
on Text receivers only for now — universal-receiver completion parity is
deferred as a polish item.

- **F5 — rename the dynamic variant to `Data`**: the formats stay `Json`/
  `Yaml`/`Toml` (artifacts carrying `decode`/`encode` statics); the recursive
  tree they decode into is the named variant `Data`. One-arg decode defaults to
  `Data`; `Data` joins the builtin type-name lists; user-visible types, hovers,
  diagnostics, tests, and examples say `Data`. Spec updated (conversions now
  exemplify `value.to(Data)`/`Data.from`; the `toJson()` hook order belongs to
  `Json.encode`). Clean break: `Json`/`Yaml`/`Toml` are rejected as explicit
  dynamic decode targets with a "use `Data`" diagnostic; the format names no
  longer keep row definitions just to carry statics.

- **F4 — `value.encode(Fmt)` method form**: mirrors F3 with the receiver on the
  value side — `value.encode(Json)` ≡ `Json.encode(value)`, universal sugar (any
  receiver), with an `encode` member actually carried by the receiver's type
  winning under closed-lookup precedence. Spec updated ("Decoding and encoding
  text"). Noted divergence to close later: spec says the default encoder is
  fallible (`Result[Text, JsonEncodeError]`); the implemented `Fmt.encode` is
  `(a) -> Text` with runtime errors.

Progress: all three slices landed 2026-07-05. F1 (claude-rust/sonnet): map
indexing mirrors the array rule in `infer_value_index` and reuses
`map_get_method`'s closure in `eval_index`. F2 (codex) + F2b (codex): Yaml/Toml
register like Json; the guided dynamic→typed construction moved to a shared
`text_format` module (`FormatValue` tree + `decode_value`); the YAML engine is
`serde_norway` (an internal parser written during a registry outage was
replaced, −405 lines); `Json | Yaml | Toml` all work as dynamic decode targets.
F3 (claude-rust/opus): `text.decode(Fmt, T)` — the checker probes the receiver
(snapshot-restored), builds the equivalent `Fmt.decode(receiver, ...)` call, and
rides the existing statics/host-comptime resolution; eval resolves the format's
decode through the `"Type.static"` dotted-key global and prepends the receiver;
`type.decode-format` diagnostics list registered formats; LSP completion offers
`decode` on Text receivers. Note: worktree offload agents branch from
origin/main — F3 was built on a 5-commit-stale base and rebased/ ported by the
agent before landing.

Spec decision (user, 2026-07-05): decoding text is spelled `decode` and owned by
the format type artifact — `Fmt.decode(text, T)` with the one-arg form
defaulting `T` to the format's dynamic type. The target type is an ordinary
comptime argument; the spec's `Json.decode[User](text)` bracket form is removed
(no bracket type application on functions — expression brackets mean indexing),
as is the format-implicit `text.parse(Config)`. `Text` carries one generic
`decode` method that flips into dataflow order (`text.decode(Json, Config)` ≡
`Json.decode(text, Config)`), mirroring `.to(Target)` ⇄ `Target.from(value)`;
formats plug in as the first argument because method lookup is closed. `.to`
keeps "convert this value"; `decode` keeps "interpret this text via a format"
(resolves the `"5".to(Json)` parse-vs-encode ambiguity). Spec: "Decoding text"
under Conversions.

Slices:

- **F1 — map indexing**: `m[key]` sugars to `.get(key)` and types as `?v`,
  matching the array-indexing rule (queue item found live 2026-07-04; currently
  a runtime error). Checker `infer` for index-on-Map, eval reuses the `.get`
  path, fixture + example coverage.
- **F2 — Yaml and Toml formats**: `Yaml`/`Toml` registered via
  `register_type_with_statics` exactly like `Json`; `decode(text, T)`,
  `decode(text)` (dynamic target reuses the `Json` variant as the shared dynamic
  data model — open question: rename that variant to something format-neutral
  later), `encode(value)`; `YamlError`/`TomlError` mirror `JsonError`. Typed
  construction machinery is shared with JSON, not triplicated. TOML has no null
  and its datetimes map to `@Text` for now.
- **F3 — `text.decode(Fmt, T)` method form**: `Text`-carried generic method
  dispatching to the format's `decode` static via the host-comptime registry
  (any registered format works, no per-format cases). Checker inference on
  Text-receiver field access, eval, LSP completion/hover on `"...".`.

Done when:

- `m["name"]` checks as `?v` and runs as `.get`
- `Yaml.decode(text, Config)` / `Toml.decode(text)` / round-trip encodes work
  and are locked by examples
- `text.decode(Json, Config)?^` checks, runs, and completes in the LSP
  identically to `Json.decode(text, Config)?^`

## Milestone DT — date and time types

Status: design settled 2026-07-11 (user decisions; full note in clex
`docs/temporal-types.md`); DT1 done 2026-07-11.

Settled design: five types — `Instant` (UTC timeline point, epoch nanos),
`Date`, `Time`, `DateTime` (unanchored wall-clock; deliberately NOT
zone-carrying), `Duration`. Offset date-times normalize to `Instant` (no
presentation-offset type). Nominal host-registered representation (validated
constructors), not transparent records. Wire format is ISO 8601/RFC 3339 text
for JSON/YAML; TOML gets its four native datetime kinds both ways. The
host-internal `FormatValue` grew a `Temporal` arm; the Aven-visible `Data` stays
temporal-free (untyped decode yields ISO `Text`). tzdb is host data, never
bundled; `now()`/named zones are platform capabilities (DT4).

- **DT1 — vocabulary + TOML native mapping: done 2026-07-11** (grok-4.5 slice;
  first grok offload — evaluated well). New `aven-host/src/temporal.rs`:
  hand-rolled RFC 3339/ISO 8601 parse/format (Hinnant civil-day math, no new
  deps), i64 epoch nanos (~1678–2262 range; out-of-range errors rather than
  wraps), `Date.new`/`Time.new`/`DateTime.of`/`Duration.ofSeconds` +
  `parse`/`compare` per type via the existing `register_type_with_statics` path,
  fixed-offset conversion both ways. Runtime values are records with a private
  `__temporal` kind marker for codec recognition (a true opaque host value
  variant in aven-eval would be the nominal-er alternative; deferred). Typed
  TOML decode maps all four kinds (local date-time into `Instant` is a shape
  error, no silent UTC; string fields accepted into temporal targets if they
  parse; temporals accepted into `Text` targets as ISO text); untyped decode
  unchanged (ISO `Text`); TOML encode emits native unquoted datetimes; JSON/YAML
  emitters are total over the arm (ISO string/plain scalar). Known latent gap:
  type definitions are structural aliases, so a hand-built record can pose as a
  temporal statically — runtime marker checks + toml-crate validation catch
  abuse with errors, not garbage.
- **DT2 — JSON/YAML ISO-string decode: done 2026-07-11** (grok-4.5 slice, with
  DT3). Turned out to already work — DT1's string acceptance lives in the shared
  `decode_value` path JSON/YAML also use — so the slice locked it in with
  committed tests only (ISO decode incl. offset normalization, shape errors on
  malformed strings, typed round-trips; plain + quoted YAML scalars). No
  behavior change.
- **DT3 — arithmetic + `now()`: done 2026-07-11** (same grok slice).
  `instant.plus/minus(Duration)`, `instant.since(Instant) -> Duration`,
  `date.plusDays(Int)` (pure civil-day math), `duration.plus`,
  `Duration.ofMinutes/ofHours/ofDays` — all checked; overflow is a runtime
  native error for methods, an `Err` value for constructors (same policy as DT1
  conversions). `now() -> Instant` is a bare global via a NEW
  `Host::register_clock()`, deliberately separate from `register_temporals()` so
  a minimal platform keeps the pure vocabulary without a clock (the
  droppable-capability split from the design note); range edges error rather
  than saturate, pre-epoch times give negative nanos. `now_type()` exported for
  hosts. Placement note: bare global follows the `writeLine` precedent; migrates
  into `std/time` when std imports land (Z-open).
- **DT4 — `Zone` platform capability: done 2026-07-11** (grok-4.5 slice;
  milestone complete). `zone(name: Text) -> Result[Zone, Text]` bare global via
  separate `Host::register_zones()` (droppable, like `register_clock`). Reads OS
  TZif bytes itself — search chain `$TZDIR` → `/etc/zoneinfo` →
  `/usr/share/zoneinfo` (NixOS has no /usr/share) — parsed by the `tz-rs` 0.7.3
  crate (first new dep since HTTP; pure parser, NO bundled tzdb, per the
  never-bundle principle; its types stay private). Path-traversal names rejected
  before FS access. `Zone`: `name`,
  `wallTime(Instant) -> { dateTime, offsetMinutes }`,
  `instant(DateTime) -> ZoneResolution` where `ZoneResolution` =
  `@Unique(Instant) | @Ambiguous(Instant, Instant) | @Skipped(Instant)`
  (ambiguous = fall-back, earlier first; skipped = spring-forward gap, payload
  is the post-gap interpretation — provisional). Resolution probes offsets ±1
  day around the wall time and keeps interpretations that map back to the probed
  offset. Tests run only against committed TZif fixtures
  (`crates/aven-host/fixtures/zoneinfo/`: Australia/Sydney + UTC), search dirs
  injected via `register_zones_with_dirs` — no machine-tzdb or env dependence.
  Known truncation: historical odd-second offsets round toward zero to whole
  minutes (fine for modern IANA data).

## Milestone P-rm — remove native path literals: done 2026-07-06

User decision 2026-07-06: drop the first-class `Path` type and bare path-literal
syntax; file/module locations are ordinary `Text` and the host resolver reads
the leading root prefix (`./ ../ $/ ~/ //`) out of the string. The `Path` type
was never implemented (path literals already typed as Text), so the slice
(claude-rust/sonnet) removed only the literal layer: `PathLiteral` token,
`scan_path`/`is_path_end_byte`, the four prefix dispatch cases, and the
`Literal::Path` AST variant with all its match arms (parser/check/eval/lsp); net
−24 LOC. A bare leading `.` now surfaces the pre-existing
`lex.reserved-operator` diagnostic. Spec updated in clex (§"File and Module
Specifiers"). Note: the worktree was built on a 13-commit-stale base
(origin/local submodule pointer lag) but the patch applied clean and the full
gate suite passed on current main.

## Milestone Z — modules and imports

Status: Z5 (bare library names, embedded `std`) done 2026-07-11; versioned
packages remain open as package resolution

Goal: host-controlled module resolution per the spec (`import("./lib/Text")`,
`Std = import("std")`). Import specifiers are static `Text` (P-rm), so `import`
reads a string-literal argument — no path-node handling.

Settled architecture (2026-07-09): a **module-graph driver in `aven-compiler`**
(`modules.rs`), not a re-entrant checker. The driver scans static
`import("...")` calls, resolves specifiers, builds the file graph, topo-sorts
(cycles are `module.import-cycle` with the full path), and checks/evaluates
leaves-first, threading each module's export-record type/value into dependents
via an injected `ModuleImports` map. `aven-check`/`aven-eval` stay
single-module: `import` there is a map lookup, and a failed import inserts as
`Deferred` so downstream checking recovers. This per-module boundary is the
future memoization seam for incremental compilation (Milestone 9), chosen over
checker re-entrancy to keep the semantic crates small.

- Z1+Z2 done 2026-07-09: `./`/`../` imports in `aven check` and `aven run`.
  Export = final expression, must be a statically known closed record; modules
  evaluate once (import-time effects run once, diamond-safe); `.av` extension
  inferred. Diagnostics: `module.not-found`, `module.not-importable`,
  `module.import-cycle`, `module.import-has-errors`, `module.dynamic-import`,
  `module.unsupported-root`, `module.unresolved-import` (warning — a relative
  import checked in a single-file context such as the LSP). CLI renders
  multi-file reports (dependency errors against the dependency's source); JSON
  output keeps the single-file schema when only one file is involved. Review
  fixes: dropped the driver's skip-semantic-on-module-errors gate so an
  importer's own type errors surface alongside a broken dependency (recovery
  over early abort), and the single-file relative-import diagnostic is an honest
  `module.unresolved-import` warning instead of a false `module.dynamic-import`
  error.
- Z3 done 2026-07-11: `$/` discovers the nearest `Aven.toml` ancestor of the
  entry file (falling back to its directory), `~/` uses the host home root, and
  `//` uses the filesystem root when provided by the host. CLI and file-backed
  LSP resolution discover roots; embeddings can explicitly provide none
  (`module.root-unavailable`). Bare names stay `module.unsupported-root`.
- Z4 done 2026-07-11: explicitly exported monomorphic type aliases travel in the
  module export channel. Importers can use `util.User` in annotations and
  extract `{ User }` (including rename patterns); type exports participate in
  completion, hover, and goto provenance. Alias-shaped standard modules now meet
  the type-export prerequisite. Comptime functions producing types and
  parameterized aliases remain deferred; bare library names still require
  package-resolution work.
- Z-LSP diagnostics done 2026-07-09: file-backed documents run the module-graph
  driver with open-buffer overlays (`SourceOverlay`; buffer beats disk, cached
  entry parse reused), publishing the same `module.*` diagnostics as
  `aven check`. Pathless/untitled buffers keep the single-file
  `module.unresolved-import` warning.
- Z-LSP2 done 2026-07-10: cross-file intelligence. The driver returns per-node
  semantics plus an export-provenance map (export field → defining file+span;
  punned/renamed/explicit/spread entries chased transitively). File-backed
  documents store the entry node's import-aware semantics per revision (single
  analysis path — the double entry analysis is gone), so member completion and
  hover through import bindings ride the existing type machinery. Goto resolves
  specifier strings, pattern-bound import names, and imported member access to
  the dependency file; import-specifier completion lists sibling dirs/`.av`
  files (extension omitted) for `./`/`../` and `$/` (from the discovered project
  root), and offers nothing for bare library names.
- Z5 done 2026-07-11: bare library names resolve through host-registered
  libraries on `ModuleRoots` (`libraries`: name → module specifier → embedded
  source; empty by default). The CLI and file-backed LSP register `std` from
  `aven_host::std_library()` — `.av` sources embedded via `include_str!` under
  `crates/aven-host/std/`, so the binary needs no filesystem for std. `std/time`
  re-exports the five temporal types by punning the host-registered names; `std`
  itself exports only `version`. Library modules key the graph by a virtual path
  (`std:/time`) that never touches `fs::canonicalize` or disk, dedups diamonds
  onto one node, and renders diagnostics as `std/time`. Relative imports inside
  a library resolve within the same library map; `$/`/`~/`/`//` from a library
  module diagnose `module.root-unavailable`. Unregistered library →
  `module.unsupported-root`; registered library with a missing module →
  `module.not-found` (with a "tried … in library …" note). Export capture
  widening: an uppercase export whose source is a statics-carrying host type
  types its _value_ field as the statics record (mirroring `Value::Type` field
  access at eval), so `Instant.parse` checks through the import; the type-export
  channel still carries the definition for annotations. LSP import-specifier
  completion offers library names after `"` and a library's module paths after
  `std/`; goto into an embedded module is deliberately omitted (the virtual key
  is not a file URI the editor could open). Deliberate deferrals: `now`/`zone`
  migration into `std/time` (blocked on droppable-capability composition),
  packages/versioned dependencies, and `Aven.toml` `[dependencies]`.
- Z-open: versioned packages remain open as package resolution. Dynamic import
  is permanently comptime-only and reports `module.dynamic-import`; it is never
  a runtime fallback.
- Design decision (user, 2026-07-09): **modules bind lowercase** — a module is
  an ordinary record value; uppercase is reserved for types alone (no
  module/namespace category). `text = import("std/Text")`; types travel inside
  the record (`x: text.Text`) or are extracted by record-pattern bindings
  (`{ Text } = import("std/Text")`, an ordinary type binding). A lowercase `=`
  binding whose RHS is comptime-evaluable (static import) is comptime-known and
  traversable in type position; `:=`-rebound bindings never are; a _bare_ type
  still cannot bind lowercase (`config = User` stays an error). Zig/TS
  precedent. Spec updated in clex (naming rules, Type and Comptime Bindings,
  Modules/Imports examples). Supersedes "imported namespaces are capitalized".
- Binding forms done 2026-07-09: record-pattern bindings
  (`{ a, b -> c } = expr`) and block spread bindings (`..expr` / `:..expr`) land
  at the top level and in blocks, for all records — the spec's selective-import
  forms (`{ join } = import("./x")`, `..import("./x")`) fall out. The pattern
  LHS parses as an ordinary `Expr` (no parallel pattern AST); eval reuses
  match-pattern destructuring; spreads require a statically-known closed record
  (`type.spread-shape-unknown` otherwise); `:..` is block-only
  (`name.no-toplevel-spread-shadow` at top level); uppercase pattern binders on
  a static import with a matching type export bind ordinary type aliases (Z4);
  otherwise they still diagnose `type.uppercase-pattern-binder-unsupported`.
- Checker bug discovered (2026-07-09), still open: a binding that shadows a
  builtin type name (`Text = import("./text")`) breaks record spread of that
  binding when the imported signatures mention the shadowed type —
  `{ ..Text, ... }` infers Deferred, so the module becomes
  `module.not-importable`. Any other binding name works, and shadowing alone is
  fine when signatures don't mention the type. The lowercase-modules convention
  makes the trigger rare (module bindings no longer collide with type names),
  but the bug is still latent for any value binding named like a type.

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
