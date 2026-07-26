# Integer representation — does Aven need arbitrary-precision `Int`?

A design note prompted by the Exercism-derived benchmark suite omitting four
cases because values exceed Aven's current integer range. Short answer: **no —
stay signed 64-bit, diagnose the boundary well, and treat those four cases as a
documented language limitation.** The note records what the implementation
actually does (not what the draft spec claims), the real cost of each
alternative, and what would change the recommendation.

## Finding first: overflow is not silent

Before the design question: **arithmetic overflow does not wrap and does not
panic.** The evaluator uses `i64::checked_*` for `+`, `-`, `*`, `/`, `%`, `^`,
and unary `-`, and surfaces a structured diagnostic:

```text
integer arithmetic overflow
`+` overflowed i64
note: Aven Int currently uses i64; arbitrary precision integers are planned
      for a later milestone
```

See `int_arithmetic` / `integer_overflow` in
`crates/aven-eval/src/lib.rs`. That is the right failure mode for a fixed-width
`Int`. It is **not** an independent soundness emergency that forces bignums.

What *is* a tooling gap, independent of bignums:

1. **`aven check` accepts out-of-range integer literals; only `aven run`
   rejects them.** The checker never range-checks the lexeme. A program like
   `n : Int = 18446744073709551615` is "ok" at check time and fails at
   evaluation with `invalid Int literal`.
2. **`i64::MIN` cannot be written as a decimal literal.** `-9223372036854775808`
   is parsed as unary `-` over the positive lexeme `9223372036854775808`, which
   does not fit `i64`, so evaluation fails before the negate. Workable only via
   arithmetic (`-9223372036854775807 - 1`).
3. **JSON numbers above `i64::MAX` demote to `Float` with precision loss.**
   `Json.decode` of `18446744073709551615` yields
   `@Float(18446744073709552000.0)`, by design of Milestone J2 (i64-fitting →
   `@Int`, else `@Float`). That is lossy, and it is already the host boundary's
   answer to "what is a large integer."

None of these require arbitrary precision to fix. They require honest range
diagnostics and, for JSON, a deliberate policy that is already half-written.

## What Aven does today

### Representation

| Layer | What `Int` is | Where |
| ----- | ------------- | ----- |
| Runtime value | `Value::Int(i64)` | `crates/aven-eval/src/lib.rs` |
| Branded payload | `PrimitivePayload::Int(i64)` | same |
| Host marshal | `AvenMarshal for i64` only | `crates/aven-host/src/marshal.rs` |
| Format codecs | `FormatNumber::Int(i64)` | `crates/aven-host/src/text_format.rs` |
| Comptime canonical form | `CanonicalLiteral::Int(i64)` (overflow → `InvalidNumber`) | `crates/aven-check/src/comptime.rs` |
| AST / tokens | **string lexeme**, not a parsed integer | `Literal::Number(String)`, `TokenKind::Number(String)` |

There is no bigint crate, no dual representation, no tagged small/large int.
`Int` in the type system is a named primitive; width lives only in the value
layer.

### Literals

1. Lexer (`scan_number` in `crates/aven-parser/src/lexer.rs`) accepts decimal
   digits, optional fraction, optional exponent, and `_` separators. **No
   `0x`/`0b`/`0o` prefixes yet** (the draft spec lists them; they currently
   parse as `0` followed by an unsupported identifier/operator form).
2. Parser stores the raw lexeme as `Literal::Number(String)`.
3. Checker treats integer-shaped number literals as `Int` (or a number-literal
   singleton / union under literal types). **No magnitude check.**
4. Evaluator (`eval_number_literal`) strips `_`, then `parse::<i64>()`. Failure →
   `invalid Int literal` diagnostic. Floats take the `.` / `e` path to `f64`.

### Arithmetic and methods

- Binary ops: `checked_add` / `sub` / `mul` / `div` / `rem` / `pow` → overflow
  diagnostic.
- `/` and `%` with zero divisor: separate `division_by_zero` diagnostic (and
  the checker already requires a statically non-zero divisor for `/`/`%`).
- `Int.div` / `Int.mod`: checked, return optional / error on zero or overflow.
- `Int.abs`: `saturating_abs` (so `abs` of the minimum i64 clamps to
  `i64::MAX` rather than overflowing — a small inconsistency with the rest of
  the checked surface).
- `Int.pow`: `checked_pow`, overflow diagnostic.

### Codec / host boundary

- **JSON / YAML / TOML:** integers are `i64`. Encode writes the decimal text of
  that `i64`. Decode of a number that does not fit `i64` becomes `Float`
  (JSON/YAML visitor: `visit_u64` tries `i64::try_from`, else `as f64`).
- **CLI value JSON** (`aven-cli`): `JsonNumber::from(*i64)`.
- **Typed host fns:** `register_fn("add", |a: i64, b: i64| …)` is the documented
  pattern. There is no `BigInt` marshal type.

### Spec vs implementation

`docs/language-spec.md` currently says:

> `Int` should be arbitrary precision by default. … Fixed-width integer types
> can exist later for interop …

The implementation is the opposite: **fixed-width now, arbitrary precision only
as a note in the overflow diagnostic.** The draft also mentions `BigInt` as a
JSON codec concern, which already anticipates that large integers are not free
at the host boundary. The aspirational spec sentence should be revised to match
a deliberate decision (this note's recommendation), not left as a silent debt.

## The benchmark evidence

Across 142 Exercism-derived tasks, 2246 rendered cases: **Python omits 0, Ruby
omits 0, Aven omits 6.** Of those six:

| Cause | Cases | Tasks |
| ----- | ----- | ----- |
| Values exceed 64-bit `Int` | 4 | `grains`, `armstrong-numbers` |
| `Int / Int` is integer division (upstream `/` is float) | 2 | `list-ops` |

The four bignum cases are values like `2^64 - 1` and a ~39-digit armstrong
number. That is **four cases out of 2246**, and they are classic "textbook
puzzle that needs big integers" exercises, not representative glue-script
traffic (files, JSON schemas, HTTP, maps, text).

**The mandate is thin.** It does not justify growing the core numeric type.
It justifies: (a) documenting the range, (b) good diagnostics when the range is
hit, (c) recording those four cases as a known Aven limitation in the benchmark
harness.

(The division mismatch is orthogonal and already settled by the 2026-07-18
ruling: integer `/` is integer division with a statically non-zero divisor.)

## Options

Costs are stated against the **core principle**: keep the implementation small;
complexity belongs in libraries, not the compiler/evaluator core.

### A. Stay `i64`, diagnose the boundary well (recommended)

**What changes**

- Check-time range diagnostic for integer literals that do not fit `i64`
  (including a special case so `i64::MIN` is accepted as a literal form).
- Sharper messages (say signed 64-bit range explicitly; drop or rephrase the
  "planned later" note until a real plan exists).
- Spec: rewrite the "arbitrary precision by default" claim to "signed 64-bit;
  overflow is a runtime error; fixed-width interop types later if needed."
- Benchmark: document the four omissions as a language limitation, not a
  generator bug.
- Optional later: library-level big-integer type *in Aven* (array of limbs, or a
  host-provided opaque) for the rare script that needs it — not as `Int`.

**Cost by surface**

| Surface | Cost |
| ------- | ---- |
| Checker | Small: parse lexeme to `i64` (or compare digit string to max) at literal
  sites; one diagnostic code. |
| Evaluator | None for the type; already checked. Small polish on messages / `i64::MIN`. |
| Codec / host | None. `i64` stays the ABI. JSON policy stays "fit → `@Int`, else
  `@Float`" unless revisited separately. |
| User model | One sentence: `Int` is signed 64-bit; overflow errors; use `Float` or a
  library/host big-int if you need more. |

**Surfaced complexity:** minimal. Matches today's reality.

### B. Wider fixed width (`i128` as `Int`)

**What it buys:** covers `2^64 - 1` and some intermediate products; still fails
the ~39-digit armstrong case and any true bigint workload.

**Cost**

| Surface | Cost |
| ------- | ---- |
| Checker / eval | Mechanical swap `i64` → `i128` in many match arms; tests and
  fixtures churn. |
| Codec / host | **Painful.** JSON numbers are not i128-safe; JS consumers max out
  at 2^53 - 1 for exact ints; `AvenMarshal` today is `i64`. Host Rust code
  expects `i64`. You either truncate at the boundary (reintroducing the same
  problem) or invent a wider host ABI for little gain. |
| User model | Still a surprise cliff, just further out. |

**Verdict:** pays most of a representation change for almost none of the
benchmark win, and worsens the host story. Reject.

### C. Separate opt-in `BigInt` alongside `Int`

**What it buys:** the four cases become expressible if the author opts in; `Int`
stays host-friendly.

**Cost**

| Surface | Cost |
| ------- | ---- |
| Checker | New named type; promotion rules (`Int` → `BigInt`?); operator
  resolution; literal defaulting (does `1000` stay `Int`?). Every mixed
  `Int`/`BigInt` op needs a rule. |
| Evaluator | Second numeric path, or a dependency (`num-bigint` / similar) and
  methods on both types. |
| Codec / host | Encode policy (string? nested array? reject?), decode policy,
  marshal type (probably `String` or opaque handle). Spec already flags
  `BigInt` as a JSON codec concern. |
| User model | Two integer types. Python-trained authors will use the wrong one
  or expect silent promotion. |

**When this earns its place:** a real product need (crypto toy scripts,
  unrestricted combinatorial counters, porting a bigint-heavy library) — not
  four Exercism cases. Prefer implementing it first as a **library / host
  capability** (`Platform.BigInt` or pure-Aven limbs) before promoting it to a
  core type. Aligns with "complexity in libraries."

### D. `Int` is arbitrary-precision by default (small-int fast path)

**What the draft spec currently promises.** Implementation shape: NaN-box or
enum `{ Small(i64), Big(Rc<BigUint-ish>) }`, arithmetic always correct, print
and compare total.

**Cost**

| Surface | Cost |
| ------- | ---- |
| Checker | Mostly unchanged at the type level (`Int` stays `Int`), but comptime
  folding and literal canonicalization must stop assuming `i64`
  (`CanonicalLiteral::Int(i64)` today). |
| Evaluator | Every arithmetic site, method, hash/eq for map keys, display,
  pattern matching on number literals. Non-trivial core growth; new dependency
  or a hand-rolled bigint. |
| Codec / host | **The expensive part.** Either:
  (1) refuse to marshal big values across the host (so `Int` is not really
  uniform), or
  (2) define a host ABI for bigints and force every platform author to handle
  them, or
  (3) silently coerce to `i64`/`f64` at the boundary (lies).
  JSON cannot represent arbitrary integers portably; stringified numbers or
  lossy floats are the usual escapes. |
| User model | "Ints never overflow" is pleasant until the value hits a host fn,
  a JSON field typed as number, or a Rust `i64` parameter — then the pleasant
  fiction breaks at the boundary Aven exists to serve. |

**Verdict:** the right default for a *standalone* scripting language whose
primary audience is puzzle solvers (Python). The wrong default for a **typed
embeddable** language whose point is a disciplined host boundary. Reject for
v0/v1 core.

## Embedding consequences (the load-bearing argument)

Aven's pitch is Roc-style platforms: pure computation with a **typed** host
boundary. Today that boundary is simple:

```rust
// host author mental model
register_fn("add", |a: i64, b: i64| a + b);
// AvenMarshal: i64 ↔ Value::Int
```

If `Int` becomes arbitrary-precision:

- Every host function that takes or returns `Int` must decide what to do with a
  value that does not fit the host type. Rust hosts overwhelmingly want `i64`
  (indexes, counts, file sizes below 8 EiB, HTTP status, timeouts).
- JSON/YAML/TOML already chose "i64 or float." Making language `Int` bigger
  than what codecs can carry creates a permanent dual: language-true vs
  wire-true.
- Map keys use value equality; bigints as keys are fine internally but hostile
  when the map is encoded.

The host boundary is therefore **the strongest argument against** default
bignums, and **the strongest argument for** keeping `Int` = `i64` while leaving
room for an explicit big type later if a platform needs it.

Conversely, default bignums' strongest argument is author ergonomics on pure
numeric puzzles. That is real for Python. It is not Aven's primary job.

## Prior art (only what earns its place)

| Language | Default integers | What they gave up |
| -------- | ---------------- | ----------------- |
| **Python** | Arbitrary precision | Cheap fixed-width mental model; C extension ABI complexity (`PyLong`); JSON libs stringify or fail on big ints. |
| **Lua** (5.3+) | 64-bit integers + floats (or all-float builds) | Seamless bigint; authors hit wrap/overflow or use libraries. Pre-5.3 all-float gave up exact ints above 2^53. |
| **Wren** | Single `Num` (double) | Exact integers above 2^53; no separate int type at all. |
| **Roc** | Fixed-width (`I64`, etc.) | "Never think about overflow" ergonomics; overflow is the author's problem (or checked ops). Matches embed/app separation. |
| **Zig** | Fixed-width at runtime; **comptime** ints are arbitrary-precision | Runtime stays predictable for systems code; big numbers only where the compiler evaluates them. |

Aven is closer to Roc + Lua-5.3 than to Python: embeddable, host-defined
capabilities, small core. Python's choice is coherent for Python's goals and
expensive for Aven's.

## Recommendation

**Commit to signed 64-bit `Int` with checked overflow (Option A).**

1. **Do not** change the runtime representation for the four benchmark cases.
2. **Do** make out-of-range integer literals a **check-time** diagnostic (same
   failure users already see at run time), and accept `i64::MIN` as a literal.
3. **Do** revise the language-spec claim that `Int` is arbitrary-precision by
   default so the draft matches the implementation and this decision.
4. **Do** record the four Exercism omissions as a deliberate Aven limitation in
   the benchmark docs/harness.
5. **Defer** any `BigInt` core type until a non-benchmark product need appears;
   if something is needed sooner, prefer a host/library type over changing
   `Int`.
6. **Leave** JSON's i64/`@Float` split as-is unless someone is losing real IDs;
   if so, the fix is a codec policy (e.g. optional stringified integers), not
   language-wide bignums.

### What would change this mind

Any one of:

- A concrete embedding or stdlib feature that **cannot** be expressed without
  language-level big integers (not "Exercism grains," not "would be nicer").
- Evidence that **checked i64 overflow** is a frequent real failure mode in
  Aven glue scripts (production logs, user reports), not synthetic suites.
- A decision that Aven's primary audience is **standalone numeric scripting**
  rather than typed embedding — that would reopen Option D, and should also
  reopen several other design choices.

Absent those, default bignums are complexity in the core for a problem the
host boundary would only reintroduce.

## If this work is scheduled

It is **not** a current tooling-first milestone. It is a small correctness /
honesty slice that can ride on evaluator and checker polish:

| Work item | Natural home |
| --------- | ------------ |
| Check-time integer literal range + `i64::MIN` | `aven-check` (literal inference / annotation path) + a shared parse helper with `aven-eval` |
| Diagnostic copy (drop "planned" until planned) | `aven-eval` `integer_overflow` / `invalid_numeric_literal` |
| Spec rewrite of Numeric Types / integer literals | `docs/language-spec.md` |
| Benchmark limitation note | benchmark harness / campaign docs (outside `aven-lang` core) |
| Optional: `Int.abs` consistent with checked overflow | `aven-eval` method table |

No new crate, no new dependency, no host ABI change.

## Out-of-scope issues noticed while looking

These are real but not part of the integer-representation decision:

- **Spec lists `0x` / `0b` / `0o` integer literals; lexer does not implement
  them** (`0xff` fails as unsupported syntax). Separate lexer milestone work.
- **`Int.abs` uses `saturating_abs`**, so the minimum i64 clamps instead of
  erroring — inconsistent with checked arithmetic elsewhere.
- **JSON large-integer → `@Float` is lossy by design**; fine for the J2 split,
  but worth a one-line user-facing doc note next to `Json` if IDs can exceed
  2^53.
- Overflow diagnostic still says arbitrary precision is "planned for a later
  milestone" — that sentence should not outlive this decision.
