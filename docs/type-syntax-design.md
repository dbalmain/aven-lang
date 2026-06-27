# Milestone 4d — Types-as-Expressions (the "fold" design)

This worktree implements type syntax (Milestone 4d) by **folding types into the
expression grammar**. There is no `TypeExpr`/`TypeKind` and no parallel
`parse_type_*` family. Type-level forms (`a -> b`, `Array[a]`, `T?`, record
shapes, variant shapes) are parsed as ordinary `Expr`/`ExprKind` nodes. The
type-vs-value distinction is deferred entirely to later semantic phases (name
resolution, type checking), which do not exist yet.

## AST

### `ExprKind` (19 variants)

Existing value forms are unchanged except where noted; the additions/changes are:

- `Arrow { params: Vec<Expr>, result: Box<Expr> }` — `a -> b`, right-associative
  (`a -> b -> c` = `a -> (b -> c)`). A **parenthesized tuple on the left
  flattens** into `params`, so `(A, B) -> C` has two params and `A -> C` has
  one. Parsed in a dedicated `parse_arrow` layer that wraps
  `parse_binary_expression` and right-recurses on `->`.
- `Index { callee: Box<Expr>, args: Vec<Expr> }` — `Array[a]` and value
  indexing `users[2]` share this neutral postfix node. Bound in `parse_postfix`
  exactly like a call; whether it is type application or element access is a
  semantic question deferred to later.
- `Nullable(Box<Expr>)` — postfix `T?`. See the seam below.
- `Set(Vec<RecordEntry>)` — **changed** from `Vec<Expr>` so `@{...}` shares the
  record-entry machinery.
- `Lambda { params, return_annotation: Option<Box<Expr>>, body }` — gained the
  optional `: type` return annotation.

Type names vs. variables are already encoded by the lexer/atom layer: uppercase
`Text` → `ComptimeName`, lowercase `a` → `Name`. No type-specific name nodes
were added.

### `RecordEntry` (7 variants)

One entry parser (`parse_record_entry`) serves both `{...}` records and `@{...}`
sets/variants, selected by an `EntryMode { Record, Set }` flag. The union:

- `Field { name, name_span, value: Expr, overwrite, span }` — the value is an
  ordinary `Expr`, so `name = Text` (→ `ComptimeName`) and `name = "Ada"`
  (→ `String`) use the same parser. N3 removed the former optional-key flag;
  omittability is represented by the field value type, `?T`.
- `Shorthand`, `Spread { overwrite }`, `Delete`, `Rename` — unchanged.
- `Open { span }` — **new**, the `.._` open-row marker (record mode only; `:.._`
  and `.._` of any other term remain spreads).
- `Element(Expr)` — **new**, a bare member of `@{...}` (`@Red`, `@Ok(1)`,
  `@ParseError(Text)`, `@NotFound`); parsed with the full expression entry point.

`->` is overloaded inside brace entries: after reading a label, a `Rename`
(`name -> to`) is checked **before** any field value is parsed, so the
function-type `->` only applies when parsing a value/term.

### Items

- `Binding` gained `annotation: Option<Expr>` (`name : T = value`).
- `Param` gained `annotation: Option<Expr>` (`(path : Path) => ...`).
- New `Item::Signature { name, name_span, annotation: Expr, span }` for a
  top-level `name : term` with no `=`. Disambiguated like the reference:
  `find_binding_operator` (depth-0 `=` or `:=`) → `Binding`; else
  `is_signature_start` (ident + `:`, no depth-0 binding operator) → `Signature`.

Every `:` RHS is parsed with the normal expression entry point
(`parse_annotation_term` → `parse_expression`), so `->`, `?`, `[]`, records and
variants all work in annotations and it naturally stops at `=` / `=>`.

## The `?` / `?>` operators (match is now a dedicated token)

Match used to be triggered by a bare `?` whose next two tokens were `Newline`
then `Indent`, disambiguated from postfix nullable (`T?`) by a lookahead seam
(`at_match_operator`). **That seam is gone.** Match is now the dedicated `?>`
operator:

> **Bare `?` in postfix position is unconditionally `Nullable`** — no lookahead.
> `value = result ?` parses cleanly as `Nullable(Name("result"))`.
>
> **`?>` is the match operator**, always, followed by a newline + indented arm
> block.

- The lexer's `KNOWN_OPERATORS` table lists `?>` before bare `?` (longest-match),
  so `result ?>` lexes as `result` then `?>`, while `Text?` before a
  newline/`=`/`,` still lexes the bare `?`.
- `parse_postfix` consumes `?` as `Nullable` unconditionally (no guard).
- `parse_expression` calls `finish_match` when the current token is `?>`.

Because the ambiguity is gone, two diagnostics now fire **unambiguously** from
`finish_match`:

- `parse.missing-match-arms` — `?>` followed by a newline without an indented
  block, or by a boundary/EOF (e.g. `value = result ?>`).
- `parse.inline-match-arms` (**new**) — arms written on the same line as `?>`
  (e.g. `result ?> @Ok(x) => x`); message "match arms must start on the next
  line, indented", then recover to the next line. The old bare-`?` design could
  not give this precisely because `result ? @Ok(x)` was a valid nullable-then-call
  reading.

Verified by unit tests: `value : Text? = name` parses `Nullable(ComptimeName)`
and stops at `=`; `value = result ?` parses `Nullable(Name)` with no diagnostics;
`result ?>\n  @Ok(x) => x` parses a `Match`; `result ?>` with no block reports
`parse.missing-match-arms`.

> **Spec follow-up:** the shared language spec was updated to use `?>` for match
> and to state that bare `?` is purely the nullable marker.

`is_lambda_start` was extended (`lambda_arrow_follows`) so a `)` may be followed
either by `=>` directly or by `: returnType =>` and still be recognised as a
lambda head.

## Diagnostics moved to the semantic phase

- **uppercase variant-tag enforcement** (reference `expected-variant-tag`,
  `@{@ok(Text)}`): under the fold, `@ok(Text)` is a perfectly well-formed `Call`
  element, so this is **not** a parse error. The "tags must be uppercase" rule
  is semantic (name resolution) and there is no semantic phase yet, so no
  equivalent fixture exists here. (The reference's invalid fixture lives in the
  competing tree; nothing was removed from this tree because it never had one.)
- **`{ 1 = Text }` / `{ 5 = 1 }`** (numeric label): still a parse error via the
  shared record-entry diagnostic, unified under `parse.expected-record-entry`
  ("expected record entry"). The same code now covers both value-record and
  type-record bad labels, since they are the same grammar.
- **`name : = "Ada"`** (`missing-type`): after `:`, the annotation term meets
  `=` and reports `parse.expected-type` ("expected a type here") at the `=`
  position, matching the reference message/span.

## Deviations from the brief

1. **`missing-match-arms` fixture restored (post-`?>`).** Under the earlier
   bare-`?` seam, `value = result ?` with no indented block was valid syntax
   (`Nullable(result)`), so the diagnostic could not fire and the fixture was
   deleted. With the dedicated `?>` match operator, `value = result ?>` with no
   arm block now fires `parse.missing-match-arms` unambiguously, so the fixture
   and its `.diag` are back. (`value = result ?` is now the *nullable* case.)
2. **`unsupported-operator` fixture changed** from `left <=> right` to
   `left ~ right`. Longest-match lexing now splits `<=>` into `<=` `>` (the
   point of the new lexer), so it is no longer a single unknown operator. `~`
   is a single known operator with no infix binding power, which still exercises
   `parse.unsupported-syntax`.
3. **`TermContext` not threaded.** The brief offered a `TermContext { Value,
   Type, Pattern }` for tuning diagnostic wording. It proved unnecessary:
   annotation-specific messages such as `parse.expected-type`, and
   match-position messages such as `parse.expected-pattern`, are produced at
   the call sites before delegating to the shared expression entry point.
   Avoiding the parameter keeps the parser smaller with identical tree shape,
   which is the stated goal.

## Pattern follow-up: pattern terms are expressions too

Milestone 4e applied the same fold to match patterns. `MatchArm.pattern` is now
an `Expr`; constructor patterns are ordinary calls, nullary tags are comptime
names, tuple/group patterns are ordinary tuple/group expressions, and record
patterns use the same `RecordEntry` parser as value/type records. Bind-vs-ref,
wildcard `_`, illegal pattern-position deletes/optional fields, and other
pattern legality rules are semantic questions, not parser questions.

## Lexer: longest-match operators

`scan_operator` now does a first-match scan over the `KNOWN_OPERATORS` table
(ordered so every operator precedes any operator it is a prefix of), and
`is_operator_byte` is derived from that same table (single source of truth). The
no-match branch is `unreachable!`. Consequence: `Text?,` lexes as `Text` `?`
`,`, and `<=>` lexes as `<=` `>`. Covered by
`tests/fixtures/lexer/valid/operator-longest-match.{av,tokens}`.

## Fixtures added / changed / removed

- Added: `parser/valid/type-syntax.av` (verbatim from the reference target),
  `parser/valid/dedent-item-boundary.av`,
  `lexer/valid/operator-longest-match.{av,tokens}`,
  `parser/invalid/missing-type.{av,diag}`.
- Changed: `parser/invalid/unsupported-operator.{av,diag}` (see deviation 2).
- `parser/invalid/expected-record-entry.{av,diag}` already used the unified
  `parse.expected-record-entry` code and needed no change.

### `?` → `?>` match-operator change

- Added: `parser/invalid/inline-match-arms.{av,diag}` — `value = result ?> @Ok(x)
  => x`, the new `parse.inline-match-arms` diagnostic.
- Restored: `parser/invalid/missing-match-arms.{av,diag}` — `value = result ?>`
  with no arm block (see deviation 1).
- Changed (match line `?` → `?>`): `parser/valid/question-operators.av`;
  `parser/invalid/expected-match-arrow.{av,diag}`,
  `single-item-pattern-tuple.{av,diag}` (the `?>` is one byte longer than `?`,
  so the indented-arm label spans on the following line shift by +1).
- Added after the pattern fold: `parser/valid/patterns.av`; removed the former
  unsupported record-pattern and match-guard fixtures.
- Lexer: `?>` added to `KNOWN_OPERATORS` (before bare `?`).
- `parser/valid/type-syntax.av` is unaffected (uses `?^` and `Text?`, not match).
