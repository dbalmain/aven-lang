# Claude: repair the review findings before enabling slice 5

Prepared 2026-09-12. Review baseline: `da9c901` on
`comptime-unification-slices-1-2`.

## Recommendation

**Make evaluation cheap before enabling slice 5. Do not ship broad folding at
its current cost, and do not use a fold-count cap as the primary solution.**
First repair the three correctness findings below: current programs can pass
literal-type checking while producing a different value at runtime.

The user requests that you address this review. Preserve unrelated changes,
follow `/home/dave/w/clex/.ai/core.md`, and do not push. Work in separate,
reviewable commits. Update `PROGRESS.md` as you go.

## Work order and acceptance

1. **Repair proof identity and lifetime.** Use resolved lexical binding identity,
   module identity, specialization identity, and valid execution context.
   Parameters and unknown shadowing bindings must mask earlier evidence. Add
   the cross-function and shadowing regressions below. Audit imported expression
   spans and repeated specializations, not only binding names.
2. **Preserve local captures.** Replace the flattened local-initializer map with
   a representation that retains defining scopes and initialization identity.
   Prove that closures retain old bindings across `:=`, and that a shadowing
   initializer reads its predecessor. Keep runtime parameters unknown.
3. **Fix or conservatively reject family-dependent evaluation.** The family
   interpolation example below must never certify `"99"`. With full runtime
   elaboration support, certify `"money"`; otherwise reject the explicit demand
   with an explanatory diagnostic. This does not require changing the rule for
   which expressions may introduce a primitive-family brand.
4. **Reduce evaluator setup and repeated work.** Follow the session-owned
   preparation and demand-owned state design below. Profile before and after;
   do not merely cache the existing mutable environment globally. Preserve
   prelude lexical isolation, module order, bounded fuel, and memory release.
5. **Only then enable slice 5**, retaining the preflight. Verify both real-file
   checker timings and scaling across generated files. Keep ordinary inferred
   types unchanged and proof-dependent acceptance predictable.

For each correctness repair, add focused positive and negative tests at the
owning layer, plus check/run agreement where runtime semantics are involved.
Before accepting the integrated implementation run:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

Use `nix develop` for the pinned toolchain and `nix develop .#msrv` for
`cargo check --workspace --all-targets` under Rust 1.91.0. Account for socket
access required by host tests. Preserve the existing ordering, cache isolation,
recursive equality, prelude capture, and string tests. Do not erase failures by
loosening their assertions.

For performance, record evaluator invocation counts, setup time, expression
time, definition cloning, and retained memory across repeated checks. Compare
100/1000/larger binding files and representative real programs using repeated
warm runs. The existing acceptance target remains a few percent for real-file
checking; investigate any larger regression before enabling broad folding.
Do not infer a fixed setup cost solely from subtracting end-to-end CLI times.

Report commits, tests, timings, and any remaining unsupported semantics. A
conservative rejection is acceptable where the agreed plan permits it, but
must be documented as a support limitation rather than called implemented.

## Detailed review evidence and performance design

### 2026-09-12 — Completion review: proof identity, captures, and family rendering

Reviewed `0e61f7b..da9c901`; verdict: **needs work**. Rebuilt the release CLI
with `nix develop -c cargo build --release -p aven` and reproduced the cases
below on that binary. Normal workspace gates were not rerun; PROGRESS reports
1870 passing tests and passing MSRV. No source changes made during review.

**P1: binding evidence is global by spelling, not lexical binding identity.**
`checker/knowledge.rs:147` inserts into a checker-wide `known_bindings` map;
`:177` retrieves by name and execution context. All unspecialized functions
share RuntimeUnknown. Popping a local scope does not remove its evidence, and
an unknown replacement initializer does not invalidate old evidence (`:197`).
Both programs below pass `aven check` and return process exit code 2 from
`aven run`, despite the annotation promising 1:

```aven
first = (@n: Int) => [n][0]
f = () =>
  x = first(1)
  x
g = (x: Int) =>
  checked: 1 = x
  checked
g(2)
```

```aven
first = (@n: Int) => [n][0]
f = () =>
  x = first(1)
  x := [2][0]
  checked: 1 = x
  checked
f()
```

Fix: associate proofs with resolved binding identities in lexical scopes;
unknown bindings/parameters must mask prior names. Scope push/pop and explicit
shadowing must apply to types, values, and proofs together. Also finish the
expression identity design: `(Span, ExecutionContext)` does not distinguish
specializations or imported sources. Contrary to its comment, `record_known`
does not reject foreign body spans, and the imported branch only truncates
`inferred_types`, not knowledge. Add import/specialization isolation tests as
well as the two proven failures above.

**P1: flattening local initializers changes closure captures.**
`checker/inference.rs:482` combines every visible local initializer into one
name map. `env.rs::LocalValue` retains an expression but no defining lexical
environment. A closure is therefore reconstructed against the latest shadow
instead of the binding it originally captured:

```aven
f = () =>
  x: Int = 1
  get = () => x
  x := 2
  result = comptime(get())
  checked: 2 = result
  checked
f()
```

This passes checking and runs with exit code 1. Fix: preserve lexical scope
and initialization identity when capturing/evaluating a local definition;
do not reconstruct closures from a flattened map. Include a positive control
proving 1 and a negative control rejecting 2, and cover `x := x + 1` so the
initializer resolves the preceding binding rather than itself.

**P1: the required family-rendering repair remains open.**
`checker/inference.rs::evaluate_known_expression` rejects elaboration-dependent
preludes but does not carry this module's runtime family plans into evaluation.
The new proof producer at `:4105` can consequently certify the wrong Text.
This is the historical family finding left incomplete, not a request to widen
the language's branding syntax:

```aven
Money = Int {
  toText(): Text => "money"
}
price: Money = 99
text = comptime("${price}")
checked: "99" = text
checked
```

Checking succeeds; running prints `money`. Rejecting `BrandedPrimitive` at the
final transport boundary cannot catch an interpolation that has already
converted an incorrectly unbranded input to Text. Fix: use runtime-equivalent
family elaboration throughout demanded dependencies, or conservatively reject
such evaluation before admitting the result. Add compiler check/run tests.
The new written-literal-branding test addresses a separate rule.

**Performance recommendation: fix setup before enabling slice 5.**
The code supports the setup concern, but the quoted wall-clock subtraction is
not an isolated measurement of one evaluator invocation. The checker clones
every module initializer into each demand's definition map; the evaluator then
reconstructs ambient modules and evaluates preludes; the `@` result proof path
also evaluates the body. Ambient installation is largely definition/method
registration, not eager execution of every std body.

Fresh release CLI measurements (three runs, medians in seconds; generated
`bN = N`, `bN = comptime(N)`, or `bN = comptime(double(N))`, with the same
`double = (n: Int): Int => n + n` helper in each file):

| Bindings | Ordinary | Pin | Helper pin |
|---|---:|---:|---:|
| 100 | 0.0272 | 0.0679 | 0.1212 |
| 1000 | 0.0478 | 1.2401 | 3.5445 |

The exact baseline differs from Claude's reported input/environment, but the
substantial overhead is reproduced. Per-demand whole-module AST cloning also
creates a scaling problem beyond fixed std setup. Instrument setup time,
expression time, evaluator invocation count, and cloned definitions before
assigning an exact fraction to each cause. A few thousand folds at 3.3 ms each
means seconds to tens of seconds; the claim of minutes requires additional
scaling or repeated work not established by that rate alone.

Recommended implementation sequence after the correctness repairs:

1. Create a prepared evaluator owned by one checking session: shared immutable
   source/definition data and correctly isolated prelude/ambient lexical scopes.
   Prepare std/prelude once per matching module configuration, not per demand.
2. Give each demand its own locals, initialization context, mutable memo state,
   and bounded fuel. Avoid deep-cloning every definition. Cache proven results
   only under complete binding/module/specialization/context identity. Carry
   evidence forward rather than recomputing an already proved value.
3. Rework teardown with this ownership split: a demand must not clear shared
   prelude method tables or invalidate another demand's captured environment.
   Release demand-owned cycles promptly and session-owned cycles at session end.
   Test failure paths and repeated sessions for retained scopes/memory.
4. Measure scaling and representative real files, then enable the planned
   preflight and ordinary folding. Do not accept a large regression merely
   because a new global fold quota hides it in small fixtures.

A deterministic resource limit remains useful for runaway evaluation, but a
global fold-count budget must not decide whether a later typed proof succeeds
because unrelated earlier calls exhausted it. Explicit/typed proof demands
need a separate, predictable bounded evaluation path. Retain slice 5 disabled
until this setup/ownership gate passes.

- **Guard:** Proposed regressions above are not yet installed. The existing
  positive local-demand tests did not cover capture-preserving shadowing or
  proof leakage between separate functions; add those before optimization.

