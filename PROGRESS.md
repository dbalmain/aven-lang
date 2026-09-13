# Implementation status — language proposals

Updated: 2026-09-14, Australia/Sydney.

## Current state

`main`, tip `c09a7a2`. All three branches described below are now merged into
it, in this order: `comptime/final-repairs` (`5e8507d`), `perf/unify-trail`
(`de22322`), and `cli/positional-arguments` (`c09a7a2`). Gates green on the
merged tip in `nix develop`: `fmt --all -- --check`,
`clippy --workspace --all-targets -D warnings`, `git diff --check`, and
`cargo test --workspace` at **1909 passed / 0 failed** (1888 before this
round). The MSRV gate (`nix develop .#msrv`,
`cargo check --workspace --all-targets` on 1.91.0) also passes.

`deps/easy-updates` (`89acb31`, the dependency bump) is still live and
unmerged; no commit in this round touches its files.

### This round — the final comptime repair handoff

`docs/claude-comptime-final-repair-plan.md` priorities 1–4, implemented in
three commits. Every reproduction in that document was reproduced on
`2e1073a` before any code changed.

| Commit    | Work                                                    |
| --------- | ------------------------------------------------------- |
| `6388e26` | Let a proof answer the demand its value satisfies        |
| `893dd6b` | Carry the driving demand through a native's callback     |
| `8f32348` | Resolve a demand's names in the scope that wrote it      |

**Scope (priorities 1–2).** A `DemandScope` value replaces the previous
`include_caller_scope` flag and is threaded through *both* the evaluation and
the dependency walk, so the two cannot disagree about which binding a name
denotes. An omitted parameter's default now resolves at the callee's
definition site rather than through the caller's locals, and the
primitive-family guard asks the same question of the same bindings.
`collect_expr_names` stopped at the top level of a record, so a shorthand
nested in a comprehension body carried a family past the guard; entries nest,
so the walk now does too, through a new `walk_record_entries` in the parser.
The family refusal moved to the shared demand boundary --- below the type walk,
so `pick(Money, ...)` still reifies a type, and above the value evaluation
where a family's rendering is actually lost.

**Demand propagation (priority 3).** `NativeContext` now carries the
environment driving the call, so a closure reached through `flatMap`, `fold`,
a stream stage or a family method spends the *demand's* fuel and registers its
frames with the demand's teardown. Before this a prepared prelude export ran
on preparation time's unlimited budget: `[1].flatMap(step)` on a six-unit
budget returned `[2]` for free, while the identical demand-local definition
exhausted it. Preparation itself is now bounded too, by its own budget,
cleared before the session is handed out.

**Typed use (priority 4).** The approved rule is restored: a known present
optional satisfies a nonoptional expected type, literal or not, at an
annotated binding or typed argument, with no assertion operator and no
`comptime(...)` wrapper. `found: Int = m.get("a")` checks and runs as 1. See
"Resolved: present optionals and opportunistic folding" below.

Two divergences from the brief, both verified against the checker rather than
assumed, and both pinned by their own tests:

- The brief's positive control for the default case (`checked: 1` inside a
  function) cannot hold. A demand written in a lambda has no initialization
  frontier, so it reads *no* module binding --- `comptime(x)` fails there too,
  unchanged by this work. The control is written at the top level instead.
- An imported callee's default reaches none of its own module's bindings,
  because the evaluator's definition map is built from the importing module.
  Both halves of that case now refuse; before, one of them certified the
  caller's value.

One behaviour changed deliberately: `@Ok(x).map(dbg)` used to print with no
location, because the context reaching the native had dropped the source. A
callback now inherits the driving call's context, so it prints the `.map(dbg)`
location. The test that pinned the gap is rewritten to state the new rule.

### Performance after the repairs

Medians of five warm release runs, `/usr/bin/time` wall clock and peak RSS,
`2e1073a` binary versus `6388e26`. Fixtures are generated files of N
definitions each, held at the same size for both binaries.

| File                       | `2e1073a`             | `6388e26`             |
| -------------------------- | --------------------- | --------------------- |
| pins, 100 / 1k / 2k        | 0.03 / 0.17 / 0.41 s  | 0.03 / 0.17 / 0.41 s  |
| helper pins, 100 / 1k / 2k | 0.03 / 0.13 / 0.29 s  | 0.03 / 0.13 / 0.30 s  |
| foldable calls, same sizes | 0.03 / 0.09 / 0.22 s  | 0.03 / 0.09 / 0.23 s  |
| prepared callbacks, same   | 0.05 / 2.62 / 11.04 s | 0.05 / 2.62 / 10.96 s |
| `examples/cli.av`          | 1.60 s                | 1.61 s                |
| `aven-host/std/cli.av`     | 1.50 s                | 1.50 s                |
| `example_stub_cli.av`      | 1.68 s                | 1.66 s                |

Peak RSS tracks within noise throughout (2000 pins: 27.0 MB vs 26.3 MB). No
regression: every figure is inside run-to-run variation, including the
prepared-callback workload, which is the one this round's evaluator changes
run through on every line.

That workload's ~4x per doubling is **not** a regression and not about
comptime. It is the quadratic recorded in the repair plan's section 6:
checking cost grows with the square of module size, and these fixtures grow
the file. `2e1073a` shows the same curve. Section 6 is a separate workstream
and is deliberately untouched here.

Both CI workflows had been red since 2026-09-01 on `clippy`, because they
resolved `dtolnay/rust-toolchain@stable` and a new lint landed; the pin to
1.98.1 that fixes it was sitting on this branch the whole time. Clearing it
exposed two further failures neither workflow had been able to reach, both
fixed here and neither a defect in Aven itself:

> The workspace total was reported as 1893 while that branch was in flight.
> That figure came from a de-duplicated count of the per-suite result lines,
> which silently drops two suites reporting the same number in the same time.
> Summed properly it was 1888 --- the figure `main` still carries.

Both CI workflows had been red since 2026-09-01 on `clippy`, because they
resolved `dtolnay/rust-toolchain@stable` and a new lint landed; the pin to
1.98.1 that fixes it was sitting on this branch the whole time. Clearing it
exposed two further failures neither workflow had been able to reach, both
fixed here and neither a defect in Aven itself:

| Commit    | Pipeline repair                                                       |
| --------- | --------------------------------------------------------------------- |
| `544df97` | bash 5.2 never dispatches completion for `$'` nested inside `$(`       |
| `5798bba` | proptest's rejection ceiling does not scale with `PROPTEST_CASES`      |

`--include-ignored` in the Heavy job is currently a no-op: the `#[ignore =
"slow: <reason>"]` tier is described in five files and used by none, so the
nightly and the PR gate run exactly the same tests, the nightly only at a
higher case count.

Astra's follow-up review of `da9c901..af60ed4` (`.ai/REVIEW.md`) found six more
gaps in the same three repairs, three of them P1. All six reproduced exactly as
reported before any code changed. All six are now closed: findings 1–3 were
repaired here and *completed* in `8f32348`, which found the same errors still
live on paths the first repair did not reach; 4 and 6 were closed in `893dd6b`;
and the fifth, a genuine tension between two previously-approved decisions
rather than a bug, was settled in `6388e26`.

| Commit    | Work                                                                     |
| --------- | ------------------------------------------------------------------------ |
| `78ebe43` | Give a call into a prepared closure the calling demand's fuel and scopes |
| `64bbbb7` | Evaluate a specialization through its own scope, not its caller's        |
| `af60ed4` | (review baseline)                                                        |
| `a30ac3e` | Slice 5 — fold ordinary calls; a fold answers a literal demand only      |
| `11f4c21` | Prepare the comptime evaluator once per check, not once per demand       |
| `2685d8f` | Withhold a proof a primitive family could have changed                   |
| `68ee95c` | Tie a proof to the binding that earned it                                |
| `da9c901` | (first review baseline)                                                  |

### The follow-up review's six findings

1. **Specialization evidence still evaluated in the caller's scope.** The shared
   evaluation helper always spliced in the checker's current local scope, so a
   caller's `x := 2` could stand in for a module callee's free `x` it never
   captured. Fixed: the helper now takes an explicit flag, and a resolved
   callee's body is evaluated with caller locals excluded rather than included.
   **Completed in `8f32348`**, which found the same error still live on the
   *default* path --- an omitted parameter's default went through the ordinary
   argument evaluator, caller locals and all --- and replaced the flag with a
   `DemandScope` shared by evaluation and dependency analysis.
2. **The family guard missed shorthand references and omitted defaults.**
   `collect_expr_names` skipped `RecordEntry::Shorthand`, so `{ price }` carried
   a family past it unseen; the guard also read the caller's raw `args` rather
   than the resolved `arguments`, missing a family reaching only through a
   default. Fixed: shorthand names are collected explicitly, and the check runs
   against the resolved argument. **Completed in `8f32348`**: collecting
   shorthand names only at the top level of a record still lost one nested in a
   comprehension body, and the guard still resolved the default's names in the
   caller's scope.
3. **Withholding the final proof was too late.** A family-tainted comptime
   argument had already selected a match arm or a domain member before any proof
   was asked for, so no `Known` was even needed to certify the wrong value.
   Fixed: a family-reaching argument is now rejected before it is evaluated at
   all, which necessarily also rejects `comptime(price)` itself — see the
   updated support-limitation entry below.
4. **Prepared closures bypassed the per-demand fuel budget.** A closure captured
   at session-preparation time carried that time's unlimited fuel and untracked
   scope registry forever; calling it ran with that budget rather than the
   active demand's. Fixed with `Environment::call_child`, which splits lexical
   capture (scope chain, imports, ambient tables — from the closure) from
   dynamic state (fuel, comptime boundary, scope registry — from the active
   caller). Does not yet cover a closure called back through a native
   higher-order function (array `.map`/`.filter`/`.fold`, operator dispatch),
   since `NativeContext` carries no environment reference — noted as a remaining
   gap. **Closed in `893dd6b`**: `NativeContext` now carries the driving
   environment, and every callback route inherits it.
5. **Slice 5 reportedly reversed the approved typed-use contract.** Resolved in
   `6388e26` in favour of the completion plan's contract item 3. See "Resolved:
   present optionals and opportunistic folding" below.
6. **Calls into prepared closures retained demand objects until session end.**
   Same root cause as #4, fixed by the same change: a dynamic call frame now
   registers with the active demand's scope registry rather than whichever
   registry its lexical parent happened to carry.

### What the three repairs were

**Proof identity and lifetime.** Evidence lived in one checker-wide map keyed by
name, so it outlived both its scope and its binding. Three programs passed
`aven check` and contradicted the annotation at runtime. Evidence for a local
binding now lives in the same scope stack as its types and initializers, so
every push/pop covers all three; introducing any local name masks what an older
binding of that spelling proved, and a binding whose value proves nothing
records that rather than leaving the question to whoever asked last.

**Lexical captures.** Local initializers reached the evaluator flattened into a
map keyed by name, so a closure written between `x = 1` and `x := 2` was rebuilt
against the second. They now arrive in written order, one lexical scope each. A
`:=` shadow's own initializer resolves from the parent scope, which is also what
gives `x := x + 1` its runtime meaning.

`record_known` now really does refuse an imported body's spans, as its comment
already claimed. No supported program exhibits the leak --- a nested
comptime-parameter call through an import does not propagate evidence today ---
so the suppression stands on the argument, not on a fixture; noted in
`an_imported_specialization_proves_a_value_at_the_callers_span`.

**Primitive families.** See the support limitation below.

### Performance

Instrumenting a thousand-pin file located the cost, which was not where the
wall-clock subtraction had suggested. Per demand: 570ms total building the
definition map, 144ms preparing ambient `std`, 2.6ms on the prelude, 0.3ms
actually evaluating --- and 2,002,000 definitions cloned across 2000 calls,
every initializer in the file copied again for every demand written in it.

Module bindings now become evaluator definitions once per check and are shared
behind an `Rc`. Intrinsics, ambient method sets and the prelude become a
`ComptimeSession`, prepared on the first demand. Teardown follows the same
split: a demand owns its scopes, fuel, boundary and family descriptors; the
session owns the ambient method table and the prelude chain.

Medians of three, release, against the `da9c901` binary:

| File                                             | Before       | After     |
| ------------------------------------------------ | ------------ | --------- |
| 1000 ordinary bindings, no demand                | 0.04 s       | 0.04 s    |
| 1000 pins                                        | 1.24 s       | 0.12 s    |
| 1000 pins through a helper                       | 3.45 s       | 0.37 s    |
| 2000 pins through a helper                       | 13.04 s      | 1.00 s    |
| 1000 foldable ordinary calls                     | 0.08 s       | 0.09 s    |
| 1000 unfoldable calls in one block               | 2.18 s       | 2.30 s    |
| `examples/cli.av`                                | 1.61 s       | 1.61 s    |
| `examples/json.av`, `errors.av`, `http-fetch.av` | 0.02--0.03 s | unchanged |

Peak RSS on the 2000-pin file is 27.1 MB either way, so none of it was bought by
holding more. The residual +5% on the unfoldable-calls file is slice 5's
preflight; that file's 2.18 s baseline is itself pathological and unrelated to
comptime, as is `cli.av` at 1.6 s. Both are worth a look and neither is this
work.

### Slice 5, and the rule it needed

Enabling folding at ordinary calls broke six tests, none of them about comptime:
checked division returning `?Int`, a literal annotation refusing an optional, a
`1 | 1.0` join refusing `Int`. Each said the same thing --- an expression's type
is its contract --- and folding answered all of them with an implementation
detail.

So an unasked-for proof answers a **literal-type demand and nothing else**, and
does not discharge optionality however literal the demand.
`total: 6 = double(3)` works; `asInt: Int = f(0)` does not, because that
annotation would hold only while the checker could still fold `f`.
`comptime(...)` asks for both, because there asking is the point.

With that rule all six stay green. Nothing was loosened and no existing
assertion was touched.

## Known gaps, recorded rather than fixed

- **Primitive families and comptime — a support limitation, not a repair.** A
  demand that can reach a primitive family is rejected outright.
  `Money = Int { toText(): Text => "money" }` is an `Int` that renders as
  `money`, and the brand arrives through an elaboration the checker records as
  it goes; comptime evaluation runs definitions without those elaborations, so
  `comptime("${price}")` rendered the bare payload and certified `"99"` for a
  program that prints `money`. **This now refuses the evaluation itself, not
  only the proof** — the follow-up review found that withholding only the proof
  was too late: a family-tainted argument could already have selected a match
  arm or a domain member before any proof was asked for. That closes the hole
  but costs more than the first repair intended: `comptime(price)` is rejected
  too, not only the interpolation that would have rendered it wrong, because
  distinguishing "harmless pass-through" from "drives a decision" needs the same
  elaboration this repair still does not do. A demand that cannot reach the
  family is untouched even in a module that declares one. Certifying the right
  answer needs runtime-equivalent family elaboration inside the demand --- a
  slice of its own. Reproducing every position where a literal may be branded
  was rejected as the alternative: it would be a second copy of a rule that
  already lives in the checker, and a copy that drifts is worse than none.
  Pinned by `a_demand_reaching_a_primitive_family_proves_nothing`,
  `comptime_pin_of_a_named_family_is_rejected_conservatively`,
  `a_family_reaching_default_is_refused_whatever_the_caller_binds` and
  `a_nested_shorthand_keeps_its_family_dependency`. The last two are this
  round's: the refusal now also survives a caller that binds an ordinary value
  of the same name, and a shorthand nested at any depth in a comprehension.
- **Branding and comptime.** `price: Money = 99` is accepted;
  `price: Money = comptime(99)` is not. This is the existing rule applying
  evenly --- branding keys on a literal _written_ at the annotated position, and
  `price: Money = 40 + 59` fails the same way with no comptime involved. Pinned
  by `only_a_written_literal_brands_a_primitive_family`. Widening it is a
  language decision.
- **`@`-param call result types.** A call to an `@`-param function returns the
  body's type rather than the declared return type. Pre-existing from slices
  1--2; relevant to contract item 7.
- ~~**Prepared-closure fuel/registry threading stops at natives.**~~ Closed in
  `893dd6b`. `NativeContext` carries the driving environment, so a closure
  reached through a native higher-order function spends the demand's fuel and
  registers with its teardown. `None` now means only the genuinely
  environment-free boundaries: the public `call_value`, `Stream`'s `Iterator`
  impl, the display protocol, and host natives.
- **An imported callee reaches none of its own module's bindings.** A demand
  evaluating an imported function's body or default sees the *importing*
  module's definitions, because that is what the evaluator's definition map is
  built from. The refusal is conservative --- the alternative was certifying the
  caller's same-named value --- but it means a supported program can go
  unproved. Pinned by
  `an_imported_comptime_default_does_not_read_the_importer_locals`.
- **A demand inside a lambda reads no module binding.** A lambda body runs at
  some future call, so the execution-context filter has no initialization
  frontier for it and refuses every module binding a demand there reaches ---
  `comptime(x)` no less than a callee's `= x` default. Widening it needs a
  frontier for deferred code, not a change to scoping. Pinned by
  `a_demand_inside_a_lambda_cannot_read_a_module_binding`.
- **Interpolation hole margins.** Margin validation still walks lines inside a
  `${...}` hole. Reproduced, documented and pinned; skipping them would be a
  language decision.
- **Wrapped interpolation.** A newline does not continue an infix operator, so
  `${"a"\n  + "b"}` stays rejected --- as does the same expression outside a
  string. Valid wrappings are documented.

## Resolved: present optionals and opportunistic folding

Settled in favour of `docs/claude-completion-plan.md` contract item 3, and
implemented in `6388e26`. The conflict, as it stood: item 3 says a known
present optional may satisfy a nonoptional expected type with no exception for
how the knowledge was obtained, while slice 5's provenance rule said an
_opportunistic_ fold answers a literal-type demand and nothing else. Both
existing tests were correct readings of their own decision.

The answer is item 3, without the provenance qualifier. A demand is answered by
the value, not by who asked for it, so `found: Int = m.get("a")` checks and runs
as 1. `Provenance` itself is deleted rather than left inert: once it decides
nothing, keeping it reads like a second assignability rule beside the real one.

What the decision costs, stated plainly rather than denied: replacing an
upstream known value with runtime input can now break a distant typed use that
relied on the proof. That is contract item 4, and it is accepted.

What it does not cost. Evidence must still agree with the expression's own type
before it speaks about any other, so a `Float`-returning call does not satisfy
`Int`. Known absence has no literal spelling and satisfies nothing. An argument
the checker cannot evaluate proves nothing. A primitive family withholds its
proof outright. And no type changes: `m.get("a")` is still `?Int` and `f(0)` is
still `1 | 1.0`, asserted by rendering beside every acceptance.

`checked_integer_division_narrows_on_static_divisors` --- the test that
motivated the provenance rule --- keeps its zero-divisor and named-family
rejections. What changed is its `n : Int = 2` case, which the checker really
can prove: the surviving negative is a divisor that only exists at runtime.
The `?Int` claim it was protecting is now asserted directly, as a type-at
assertion on an unnarrowed division, rather than indirectly through a
rejection.

## Resumed implementation — agreed review amendments

Implementation resumed after discussion with the user, from clean `3163938` (the
plan commit following `58eb0a8`). The root agent orchestrates and reviews; Terra
and Luna completed the ordering repair, and root accepted its final gates. No
agent currently owns active source edits. Implementation is incomplete: slices
1–2 and the binding-order follow-up have passed their gates. Semantic knowledge
(slice 3) is next. See **Accepted ordering repair** below before the historical
gate results. Live notes are in `docs/comptime-implementation-progress.md` and
`docs/comptime-prelude-progress.md`. No push is authorized.

The discussion supersedes these parts of the plan below:

- Unsupported evaluation fails conservatively at explicit demand sites. An
  ordinary expression may remain unknown; it must never receive a guessed value.
- A compile-time-known **present optional may implicitly satisfy a nonoptional
  expected type**, including a literal annotation, when its payload satisfies
  that type. An absent or unknown optional cannot. Typed arguments and annotated
  bindings are demand sites; no assertion operator is required. The user accepts
  that changing an upstream known value to an unknown runtime input can break
  distant uses that depended on this proof.
- Preserve ordinary inferred types, but do not interpret that as forbidding
  verified conversions at typed uses. Preserve semantic values (presence and
  named-family behavior) in the knowledge channel; type preservation alone does
  not make raw evaluator values sound.
- The runtime-dependency shortcut, named-family rendering mismatch, and
  check/runtime binding-order discrepancy require regression coverage and
  correction or conservative rejection. Passing the previous suite is not
  sufficient evidence for these cases.
- Review each implementation stage before expanding it. Measure checker cost
  directly as well as workspace-test elapsed time; preflight must account for
  lexical captures and demanded dependencies.

The green status and test count below describe the starting implementation, not
verification of the resumed work.

### Accepted ordering repair — September 11

Root accepted and committed the repaired ordering implementation as `55ac2d1`
after Terra/Luna coding and independent review. **1845 workspace tests pass,
zero failures**; checker has **673** tests. Workspace clippy
(`--all-targets -- -D warnings`), formatting, and diff checks pass. Final logs:
`/tmp/aven-order-final-test.log`, `/tmp/aven-order-final-clippy.log`,
`/tmp/aven-order-final-fmt.log`, and `/tmp/aven-order-final-diff.log`. The
workspace test run had socket access.

The checker distinguishes Artifact, RuntimeUnknown, and RuntimeKnown contexts;
actual annotations run in Artifact context. Unproven runtime lambda demands
reject conservatively. Lazy evaluation respects initializer boundaries before
memoized reads. Specialization caches retain context separately from canonical
recursive type identity. A same-specialization cache regression was mutation
tested: context-insensitive lookup incorrectly returned 3 and failed the test.

The integrated gate exposed recursive equality repeatedly unfolding optional and
expanded `Chain(Int)` references. Equality now recognizes wrapped recursive
identities and tracks compared type pairs. The existing recursive compiler
fixture passes. Failed annotation-depth and test-only context workarounds were
removed.

Root rebuilt/checked CLI controls: direct, transitive, pinned, helper, imported,
and unknown-lambda forward demands reject; earlier closure capture, initialized
import, artifact-derived Bool, and generic recursive equality pass. No push.
Slice 3 has not started; follow the semantic knowledge gate below. Historical
blocked/intermediate notes below are retained as the diagnosis trail and are
superseded by this accepted checkpoint.

### Resumption quality-gate results

- Independently verified clean `3163938` using a snapshot in
  `/tmp/aven-comptime-baseline-3163938`: **1818 passed, zero failures**. Logs:
  `/tmp/aven-comptime-baseline-tests.log` (cold build, 82.91 seconds) and
  `/tmp/aven-comptime-baseline-warm-tests.log` (cached build, 47.58 seconds).
  Three direct checker-suite baseline runs took 3.117, 3.122, and 3.243 seconds;
  data: `/tmp/aven-comptime-baseline-check-timing.json`.
- Slice 1 committed as `4465716`; work now on `comptime-unification-slices-1-2`.
  Root's full checker gate found **652 passed, three failed** in
  `/tmp/aven-comptime-slice1-check.log`. Terra is repairing the failures before
  proceeding to slice 2. Do not treat the slice as green.
- Slice 2 needs generic prelude exports: ordinary functions from
  `std/prelude.av` must reach both checker and runtime as lexical defaults. No
  Rust special case for the name `comptime`; explicit user bindings shadow
  prelude exports. This plumbing is approved, with root review required.
- Terra's first repair attempts did not pass the checker gate. The root assigned
  `repair_demand_outcomes` (inheriting the root model) the bounded slice-1
  repair after Terra could not complete it. Terra is idle; do not resume two
  agents against the same checker files. Uncommitted source changes following
  `4465716` are repair experiments until a fresh passing result is recorded.
- CLI completion fixture baseline
  (`aven check .../completion_tool.av --timings`, five runs) checker times:
  3525.614, 3496.118, 3511.874, 3574.593, 3572.739 ms. Data:
  `/tmp/aven-comptime-baseline-cli-timing.json`.
- Before slice 5, review evaluator lifetime as well as elapsed time:
  `Scope.values` can memoize a `Value::Closure` whose `Environment.scope` points
  back to that scope, forming an `Rc` cycle; installed ambient methods also
  capture their environment. Reconstructing these environments at every fold can
  retain ASTs after scalar results are consumed. This is a static ownership
  finding, not yet measured or fixed. Keep closure/capture behavior correct when
  choosing cache lifetime or cleanup.

### Latest checkpoint — 2026-09-09

- `24d17aa` repairs slice 1. Owner suite: **658 unit + 2 fixture tests pass**;
  format/workspace clippy and final checker clippy passed. The actual annotation
  regression was builtin dispatch preceding user-function lookup (`pick` and
  five sibling names). Shared demand outcomes now retain failures without
  repeating evaluation; a successful runtime result cannot erase a type bound
  failure. Two old silent-deferral tests now explicitly reject unsupported
  evaluated Set/Record arguments, per the user's conservative-failure decision.
- Full unsandboxed gate compiled concurrent prelude work: **938 passed** before
  its module-count assertion failed (expected 10 std modules, now 11).
  `/tmp/aven-demand-repair-workspace-unsandboxed.log` is therefore an integrated
  intermediate result, not a clean slice-1 workspace gate.
- `54c36ff` is an **incomplete prelude infrastructure checkpoint**. Root review
  found checker/runtime export disagreement: checker exposes private bindings
  and erases qualified constraints. It also needs proper checked comptime export
  metadata and builtin removal. Do not treat it as green or slice 2 complete.
- Terra did not complete subsequent bounded metadata patches. Ownership is now
  wholly with `repair_demand_outcomes` for slice 2; Terra is idle. Root owns
  this file. Check for partial uncommitted metadata edits before resuming.
- Slices 3–5 (known-value channel, implicit verified optional conversion,
  binding propagation and opportunistic folding) have **not started**.
- `b589d6e` repairs prelude export metadata. Both graph passes now use actual
  checked record exports with qualified schemes and comptime functions; private
  bindings stay private and source/host shadows agree. Invalid, non-record,
  implicit-type-export, and runtime-failing preludes stop consumers. Owner
  gates: checker **658 + 2**, compiler **42 + 95**; format and all-targets
  checker/compiler clippy pass. Logs: `/tmp/prelude-metadata-owner-tests.log`,
  `/tmp/prelude-metadata-clippy.log`. This repairs `54c36ff`'s known defects.
- Builtin removal is now underway under `repair_demand_outcomes`; no parallel
  implementation agent owns checker files. Generic `@`-call recognition must
  evaluate the whole call when knowledge is demanded, not assume its result is
  known merely because its `@` arguments are known. Preserve named-family and
  optional types; if necessary bring the relevant known-value work forward
  rather than recreating a special case for the name `comptime`.
- Latest builtin-removal work is **uncommitted** after `b589d6e`: compiler,
  evaluator and LSP builtin entries removed; std prelude defines ordinary
  `comptime`; low-level tests receive explicit prelude metadata. The first
  checker run passed 656/658; subsequent fixes preserve label-set `Set(Text)`
  types and address default-argument definition scope. Do not treat this as a
  final gate. Logs use `/tmp/prelude-remove-*.log`.
- Root review of newly reconstructed comptime prelude scopes requires retaining
  `eval_items` outcome diagnostics and conservatively rejecting missing
  elaborations (including private named families). Nested demands such as
  `again = comptime(script)` must see prelude functions with correct lexical
  captures. These checks remain with the implementation owner.
- Resumed September 9 with the same single implementation owner. Recovered
  `/tmp/prelude-remove-tests5.log`: **661 checker unit + 2 fixture tests pass**.
  `/tmp/prelude-remove-integrations2.log`: **42 compiler unit + 97 module + 184
  LSP tests pass**. The last clippy log still fails on `filter_map_bool_then`;
  no final workspace gate or builtin-removal commit yet.
- Root review found a further prelude scope defect: reconstructed prelude
  closures capture the root into which later prelude exports are inserted,
  whereas runtime preludes have independent intrinsic scopes. A later prelude
  exporting `repr` can change an earlier prelude's `render = () => repr(1)` only
  during checking. Implementation owner is separating consumer defaults from the
  immutable base and adding a check/runtime regression before commit.
- The focused scope regression now passes. Reconstruction uses a separate
  consumer-default scope, and the type evaluator prevents a foreign function's
  missing captured callee from resolving to caller exports. Bare checker/eval
  APIs explicitly reject `comptime` without a prelude. Root accepted these
  changes for the slice-2 checkpoint, conditional on final gates.
- Final builtin-removal gate is green: **1833 workspace tests pass, zero
  failures**, with workspace clippy, format, and diff checks passing. Logs:
  `/tmp/prelude-removal-workspace-green.log` and
  `/tmp/prelude-removal-clippy-green.log`. The stale host library-list test now
  includes `std/prelude`. Foreign context isolation also covers type lowering
  and reflection/value inference. Owner checks: checker **662 + 2**, compiler
  **42 + 98**. The implementation agent hit its usage limit after these gates;
  root committed the reviewed checkpoint as `d327ea2`. Slice 3's design below is
  approved but implementation has not started.
- Terra committed `87cb889`, a first binding-order guard; **664 checker unit
  tests pass**, but root review marks it **needs work**. Filtering definitions
  by the demanded expression's source offset does not model the initialization
  boundary of lazily evaluated earlier bindings, or demands in helper/default
  bodies. Terra owns the follow-up; do not start a second source editor.
  Required controls include a closure called after its captured dependency
  initializes, and an earlier initializer that reads a later dependency even
  when the final demand occurs after both. No full-workspace acceptance of this
  follow-up is claimed.
- Terra's uncommitted evaluator repair now tracks lazy initializer availability
  before cached values and restores the caller boundary after initialization.
  `ComptimeDefinition` holds optional order metadata; ambient definitions
  inherit caller context without comparing unrelated source spans.
  `ComptimeEvalConfig` keeps the evaluator API cohesive. **668 checker unit
  tests pass** and affected checker/evaluator all-targets clippy passes. This is
  an intermediate gate: the checker still approximates demand context from
  inference state.
- Root independently confirmed two type-evaluator bypasses: a demand can read a
  later `comptime(3)` binding, or call a later `(@x) => x` helper, and certify
  `3`. Source ownership has transferred to `repair_demand_outcomes`; Terra is
  idle. Required repair: explicit execution context plus runtime availability
  requirements through compiler evaluation and cached specializations. Do not
  simply replay every scalar in the runtime evaluator: compiler-artifact
  operations can also yield Bool/literal values. Preserve those computations.
  The current uncommitted patch is not yet accepted as a complete order fix.

### September 10 resumption

- Terra resumed as the sole source owner of the uncommitted ordering repair; the
  stronger agent's checker edits were interrupted before validation.
- Root review requires preserving specialization caching without reusing a
  result under an incompatible initialization boundary. Canonical recursive type
  identity must remain independent of demand execution context.
- The quality gate also covers pinned forward references, helpers declared
  later, imported runtime helpers, and compiler-artifact computations returning
  scalars. No new green result or acceptance is claimed yet. Slice 3 has not
  started.
- September 10 implementation checkpoint: checker unit suite is green (**671
  passed**), and
  `cargo clippy -p aven-check -p aven-eval --all-targets -- -D warnings` passes.
  The final workspace test/clippy rerun is logged under
  `/tmp/aven-comptime-binding-order-workspace-*-final.log`; root review remains
  pending and no implementation commit has been made.

### Blocking review finding — unknown runtime versus artifact context

Root rejected the September 10 candidate despite 671 passing checker tests.
`runtime_binding_availability` treated `None` as permission for all runtime
reads, while lambda checking also used `None` for unknown execution context.
Both inferred and annotated versions of this program checked but failed to run:

```aven
f = () => comptime(later)
early = f()
later = comptime(3)
early
```

The exception was added for the existing compiler-artifact test
`comptime_function_application_reifies_sorted_literal_union`:
`keyUnion = (r) => keysOf(r); Keys = keyUnion(User)`. These contexts must be
represented separately. Terra is implementing explicit Artifact / RuntimeUnknown
/ RuntimeKnown context and context-keyed caching. Unknown runtime demands reject
unproven reads conservatively; no invocation analysis or later knowledge slice
is authorized as part of this repair. Required final tests include both lambda
forms, artifact scalar preservation, cache context isolation, and compiler
import ordering. Earlier green gates are intermediate results; the
implementation remains uncommitted and unaccepted.

### Latest ownership handoff — September 10

- Terra completed the explicit execution-context split and persistent compiler
  import before/after test, then hit its usage limit (reported reset 11:19 AM).
- Root rebuilt the CLI and confirmed the unknown-runtime lambda example now
  rejects with `comptime.argument-not-known`.
- Luna (`ordering_final_gate`) is sole source owner for the final bounded work:
  replace the ineffective cache-isolation regression (its two calls used
  different argument keys), run final workspace/fmt/clippy gates, update docs.
  The corrected regression must exercise the same specialization under different
  contexts and fail if cache context isolation is removed.
- No implementation commit or final acceptance yet; no semantic knowledge work
  has started. User reiterated Luna/Terra write code and root assures quality.

### September 11 resumption — recursive-type gate remains blocked

- Luna replaced the ineffective cache test with a same-specialization regression
  and verified it fails under context-insensitive cache lookup. Production cache
  code was restored; root accepted this test.
- Annotation lowering now enters Artifact context, restoring three annotation
  regressions; checker suite passes **673**. Final compiler/workspace validation
  reproducibly overflows in
  `recursive_runtime_targets_decode_encode_and_preserve_shape_errors`, including
  serial execution. A diagnostic 16 MiB test stack also overflows immediately
  (`/tmp/aven-order-stack-diagnostic.log`); increasing stack is not an accepted
  fix.
- Luna's additional annotation-depth/wrapper changes did not fix the overflow
  and remain unreviewed experiments. Terra resumes as sole source owner to
  identify the recursion/cache interaction, remove unnecessary experiments,
  repair it, and run final workspace gates. Luna is idle.
- No implementation acceptance or commit yet. Keep all forward-reference,
  unknown-runtime lambda, artifact, cache-isolation, and import tests intact.

### Latest narrowed blocker and ownership

Terra again hit its usage limit; Luna is sole source owner. Root independently
minimized the overflow to `chain == chainAgain` after decoding generic
`Chain(Int)` values. The same source without equality passes CLI checking; Tree
and mutual-record equality controls pass. Files:
`/tmp/comptime-binding-review/chain-use-equal.av` and `chain-use.av`.

Root identified a candidate in `equality_compatibility`: it checks recursive IDs
before peeling optional wrappers, then normalizes wrapped recursive IDs into
records before descending again. This can repeatedly unfold an
`Optional(Recursive(...))` field. Luna is verifying and repairing that path; no
acceptance yet. Preserve mismatch diagnostics and avoid arbitrary stack limits.
Remove failed annotation-depth experiments once the cause is verified.

### Equality repair still under review

The optional-wrapper identity check alone did not complete the repair. Root
reproduced the exact compiler test overflowing again in
`/tmp/aven-order-root-recursion-current.log`. Removing Artifact annotation
context merely masks that overflow and restores annotation regressions; it is
not accepted. Luna is restoring the simple annotation wrapper and tracing the
recursive-versus-expanded equality pair, then implementing a local recursion
pair guard while preserving mismatched-field checks. Failed depth/duplicate
annotation wrapper experiments have been removed. No final green workspace gate
or implementation acceptance is claimed.

### Next implementation gate: semantic knowledge

Slice 3 inventory completed after the ordering commit; no slice-3 code changed.
Root verified the main seams directly: computed `@` parameter narrowing is in
`checker/inference.rs` (`narrow_to_literal`, `infer_comptime_param_call`,
`evaluate_comptime_param_argument`), while typed binding checks enter
`checker/type_checking.rs::check_value_against_target`. Ordinary argument
checking also has inference/unification paths and must share the proof rule. Do
not rely on the inventory agent's earlier attribution of demand evaluation to
`checker/value.rs`.

Implementation should retain actual evaluator values alongside ordinary types,
not encode knowledge by rewriting `Type` or reconstruct values from display
text. Existing compiler artifacts remain distinct. Side-table identity must
include module and lexical/specialization context; a bare Span is insufficient.
Pin narrowing removal and proof-based literal/known-present-optional checking
must land together. Audit existing arithmetic/interpolation folding before
claiming all computed knowledge is separate from types. Family-dependent proofs
require the runtime elaboration plan or conservative rejection.

- Keep runtime knowledge separate from `Type` and from compiler artifacts such
  as reified types. Preserve evaluator values rather than round-tripping through
  display text: integers and floats, absence and presence, and family identity
  must retain their meaning. The evaluator represents optional absence with
  `Undefined` and nullable absence with `Null`; present payloads are unwrapped
  runtime values, so the existing type remains essential context.
- Key expression knowledge by source/module and specialization or lexical
  instance, not bare spans shared by imported functions or repeated calls.
  Unknown runtime parameters must never acquire knowledge from singleton types.
- Remove computed-value type narrowing together with literal demand checking,
  keeping the motivating example passing in that same slice. Typed arguments and
  bindings must share the proof-based optional conversion behavior.
- Before accepting family-dependent knowledge, share the runtime elaboration
  construction or conservatively reject it. `primitive_family_plan` currently
  lives in `aven-compiler/src/modules.rs`; duplicating branding/rendering rules
  in another evaluator would create another check/runtime disagreement.
- Preserve sequential runtime availability when demanding definitions. A forward
  binding cannot be certified merely because lazy evaluation can find its AST.
  Full local propagation and opportunistic folding remain later gates.

At the starting checkpoint, the tree was green and committed.
`cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace` all passed: **1818 tests, zero failures**, up from the
1800 at baseline `8cc9872`.

The open decision is settled: `comptime` is an ordinary `@`-parameter function,
and known values move beside the type rather than into it. The plan is in
_Decided: `comptime` is an ordinary comptime-parameter function_.

## What landed

Commits on `main`, oldest first:

| Commit    | Slice                                                                   |
| --------- | ----------------------------------------------------------------------- |
| `4d7d21c` | raw and triple-quoted string literals (lexer, formatter, codes)         |
| `843a621` | the literal/comptime contract doc and the review page                   |
| `47fda25` | comptime evaluation through ordinary helpers — committed red, see below |
| `70e7cee` | narrow that evaluation to the pin, and keep named families intact       |
| `cf77313` | prove literals survive the formatter; allow a blank opener line         |
| `3940e3c` | the two proptest seeds that found the opener-blank defect               |
| `6a74f8a` | write `std/cli`'s generated shell fragments as raw multiline text       |
| `3a29bf9` | reject an optional value at a literal-type annotation                   |
| `c8fd362` | keep the named-family owner key out of diagnostics; fix a line index    |
| `20f5d52` | fix three pin defects found by review                                   |

`47fda25` was committed knowingly red — 610 passed / 36 failed — because three
agents had been cut off by a shared usage limit with the work uncommitted, and a
WIP commit was worth more than a clean history. `70e7cee` is its repair.

### Strings

The contract in `docs/language-literals-and-comptime.md` is implemented: raw and
triple forms, arbitrary matching hash counts, the closer's margin as the dedent,
blank-line and tab rules, CRLF/CR normalization, and a formatter that moves body
and closer together.

Two amendments to the contract as written, both from running it:

- **Spaces and tabs may sit between `"""` and its newline.** They are invisible
  in an editor, contribute nothing to the value, and Java, Kotlin and Swift all
  accept them. The original rule made an editor's trim-on-save change a
  program's meaning. Found by the formatter's own property suite.
- **The formatter's perturbation strategy no longer edits literal payload.** It
  added trailing whitespace to every content line, which outside a literal is
  layout and inside one is value. It now lexes the seed first.

Coverage: `crates/aven-parser/tests/string_literals.rs` for the decode matrix,
`crates/aven-fmt/tests/string_values.rs` for value preservation across a format
(deeper-than-margin content, blank and whitespace-only lines, tabs, escapes and
their raw counterparts, hash delimiters whose payload contains the closing
spelling, interpolation with a nested raw literal, a literal under indented
syntax, two literals at different depths, and a CR-only source), and
`crates/aven-fmt/tests/fixtures/valid/multiline-strings.{av,fmt}`, which also
seeds the formatter property tests.

### Comptime

`comptime(e)` now evaluates through ordinary helpers, ambient method bodies,
lexical captures and record shorthand, with no helper or parameter marked
comptime. The motivating example works:

```aven
join = (parts: Array(Text)): Text => parts.joinWith("\n")
script = comptime(join(["a", "b"]))
checked: "a\nb" = script
```

`aven-eval`'s `Environment` gained lazy definitions, cycle detection, blocked
local names and lexical resolution; `eval_comptime_expr` evaluates only demanded
definitions, shares the existing fuel budget, and installs no host capabilities.
Record shorthand resolves lazily, so `{suffix}` and `{suffix: suffix}` agree — a
divergence found in review.

The pin narrows to the evaluated value, but only to a **refinement**: the
singleton must already sit inside the type the expression had. A base kind
admits its own literals; an open literal row admits one more of its own base;
nothing else does. A named family such as `Money` is not a base kind, which is
what keeps a branded value from folding to a raw number.

Failure reports evidence, which is the second half of review item 4: the
dependency is named and labelled at its own span, and a runaway call ends in a
bounded resource diagnostic rather than hanging.

### CLI adoption

`crates/aven-host/std/cli.av`'s static bash and fish fragments are now raw
multiline literals rather than arrays of doubly-escaped one-line strings.
Escaped quotes in the file drop from 75 to 11. The dynamic quoting paths are
untouched.

Generated bytes are unchanged, verified against the `completion_tool.av`
fixture: bash 10687 bytes, sha256
`acf85181205c602efcca89e10a6828e5bdcc7a2e6818e7db5de5f32462b96b1a`; fish 5889
bytes, sha256
`0eaab00a89c7f62e0bf177b7491656cc2a0834105de6360a0f213a1475d33a27`. Those
figures were regenerated here rather than carried over from the handoff, and
they match it. The real-shell integration tests pass unchanged.

### Parent spec

`/home/dave/w/clex/docs/language-spec.md` has the prepared string-literal patch
applied, plus the opener-blank amendment above.

## Decided: `comptime` is an ordinary comptime-parameter function

Decided 2026-09-07. `comptime` is **not** a builtin pin. It is

```aven
comptime = (@arg) => arg
```

an ordinary function whose only distinction is that its parameter carries `@`. A
call pins because **every `@` parameter it has was supplied a compile-time-known
argument** — nothing about the call site is special. So a user-written

```aven
comptimeAdd = (@a: Int, @b: Int): Int => a + b
```

pins on exactly the same rule, with no compiler support of its own, and
`comptimeAdd(1, 3)` is known for the same reason `comptime(x)` is.

Two framings are superseded by this. The first is the builtin `comptime(e)` form
currently in `checker/inference.rs`. The second is the spec's _Comptime by
inference_ reading (decision 2026-09-06), where **any** ordinary call with known
arguments folds — that is still wanted, but as the end of this plan rather than
the start; see _Why fold-everywhere comes last_.

> Note for anyone reading Aven for the first time: `@` has two unrelated
> meanings. On a parameter declaration it marks a comptime parameter
> (`(@key: Text) => key`); the body then refers to the bare name, and `@name`
> inside a body is a parse error. Before a brace it is a label-set literal
> (`@{"name", "email"}`). This plan only concerns the first.

### What is already true, and what the gap is

Four probes, run on `58eb0a8` with `target/debug/aven check`. **Re-run them
before starting and report any divergence** — the plan is built on them, and a
brief's claims about code are exactly the thing that rots.

**`@`-parameter functions already fold their bodies to a literal.** This is the
good news and it is load-bearing: much of what the plan wants already exists on
the `@` path.

```aven
comptimeAdd = (@a: Int, @b: Int): Int => a + b
checked: 4 = comptimeAdd(1, 3)     # passes
```

with the negative control that proves the fold is real rather than unchecked:

```aven
checked: 5 = comptimeAdd(1, 3)
# type.literal-not-in-union: literal 4 is not one of 5
```

**But the `@` path is strictly weaker than the builtin pin.** These two programs
differ only in which pin is used, and they disagree:

```aven
pin = (@arg: Text): Text => arg
join = (parts: Array(Text)): Text => parts.joinWith("\n")
script = pin(join(["a", "b"]))
# comptime.argument-not-known: comptime argument to `pin` is not known
```

```aven
join = (parts: Array(Text)): Text => parts.joinWith("\n")
script = comptime(join(["a", "b"]))
checked: "a\nb" = script           # passes
```

The cause is the guard at `checker/inference.rs:4138`:
`evaluate_comptime_param_argument` bails via `is_runtime_computation_call`
(`inference.rs:4164`), which classifies _any_ call to a lowercase function with
no `@` parameters as a runtime computation, "even if the evaluator can reduce
its body". `join` is exactly that. The builtin pin has no such guard — it runs a
two-evaluator cascade and then `evaluate_known_expression`.

Closing that gap is the whole of slice 1, and it is why slice 1 comes before
deleting the builtin: delete the builtin first and the motivating example
regresses.

### The slices

Each is a commit boundary. Slices 1–2 are the `@`-unification; 3–5 are the
known-value work.

**Slice 1 — one demand path, the strong one.** Give a `@`-parameter argument the
evaluation the builtin pin gets. Concretely: `evaluate_comptime_param_argument`
gains the cascade `infer_comptime_pin_call` uses — the type-position walker
(`comptime::evaluate_type_position_with_bindings`), then
`evaluate_known_expression` — and its failures report with the pin's evidence
(dependency named and labelled at its own span, bounded-resource diagnostic for
a runaway).

The `is_runtime_computation_call` guard exists for a stated reason:
"`pick(bad())` must not execute `bad` while validating a comptime argument."
Decide whether that reason survives, and say which in the done-note. Evaluation
is fuel-bounded and installs no host capabilities, so the risk is not effects —
it is likely diagnostic quality (reporting an evaluation failure inside `bad`
instead of "this argument is not known"). If so, keep the guard's _diagnostic_
and drop its _refusal to evaluate_. If there is a soundness reason we have
missed, say so and stop — a correct "this cannot be unified" is worth more than
an implementation of our guess.

Done when `pin(join(["a", "b"]))` and `comptime(join(["a", "b"]))` agree, and
`crates/aven-check/tests/fixtures/check/invalid/comptime-pin-runtime-value.av`
still fails for the same reason.

**Slice 2 — `comptime` stops being a compiler builtin.** Delete
`comptime::COMPTIME_PIN` and its special-casing at `inference.rs:3643` and
`core.rs:1867`, and bind `comptime = (@arg) => arg` in the Aven layer under
`crates/aven-host/std/` (the same layer that already carries the ambient method
sets — see the note in `.ai/core.md`). Keep the arity and "could not evaluate"
diagnostics working; if a user-defined pin cannot produce diagnostics as good as
the builtin's, that is a finding about `@` parameters and should be reported
rather than worked around by keeping the builtin.

Reminder: `crates/aven-host/std/*.av` is `include_str!`-embedded. Rebuild the
binary after editing it.

**Slice 3 — known values move beside the type, and the pin stops narrowing.**
This is the substantial one and the reason the rest is safe.

Today a pin rewrites the _type_: `comptime(e)` narrows `e`'s type to a singleton
literal row, guarded by `literal_type_refines` (`inference.rs:468`). That
conflates "the checker knows this value" with "this type is narrower", and it is
the single cause of three of the four things that broke when fold-everywhere was
attempted in `47fda25`:

| Breakage in `47fda25`                                      | Under a known-value side channel                |
| ---------------------------------------------------------- | ----------------------------------------------- |
| `Map.get("a")` narrowed `?Int` to `1`, erasing optionality | type stays `?Int`; the known value is `Some(1)` |
| a `1 \| 1.0` match join collapsed to whichever branch ran  | type stays `1 \| 1.0`                           |
| a branded `Money` folded to its raw `Int`                  | type stays `Money`; the known value is `99`     |
| every inferred call cloned an evaluator environment        | still real; slice 5's preflight                 |

So: introduce a `KnownValue` side table keyed by expression, holding a
`ComptimeValue`. **It must not live inside `Type`.** Putting known-ness into
`Type` means it flows through unification, subsumption and rendering, which is
the road that produced 36 failures; a side table leaves `Type` untouched.

With it, `literal_type_refines`, `narrow_to_literal` and
`narrow_to_evaluated_literal` (`inference.rs:468–525`) should all be deletable,
along with the `number_literal_text_is_finite` guard that exists only to stop
`NaN` refining `Int`. If any survives, say which and why.

A literal-type annotation becomes a **demand site**: `checked: "a\nb" = script`
passes by consulting `script`'s known value, not because `script`'s type was
rewritten. This half must land in the same commit as the first, or the
motivating example regresses.

Diagnostics have to consult the channel too, or a correct program reports
`expected "a\nb", found Text`.

Two consequences worth stating in the doc, because they are the design's price:

- **Literal types stay.** They are what an author _writes_ and what crosses a
  function signature (`(): "a" => "a"`). Known values are what the checker
  _discovers_, and they deliberately do **not** cross a signature. Two
  mechanisms where there is one today.
- **This is what makes fold-everywhere safe to swap out.** Because the type
  never changes, replacing a comptime-known value with a runtime input cannot
  break callers — it can only fail at the sites that explicitly demanded
  knowledge. Under type-narrowing folding it would break every downstream use.

Slice 3 subsumes two open findings below: _A pin does not apply a named family's
`toText`_ stops being a narrowing question (though the missing family plans in
`eval_comptime_expr` are a separate, small fix that should still be made), and
_The `folded` shortcut accepts a pin without evaluating it_ gets its honest fix,
since "require evaluation provenance rather than a row shape" is precisely what
a known-value channel provides.

**Slice 4 — propagate known values through let-bindings.** `x = 1 + 1` records
`2` beside `Int`; a later `comptime(x)` or `checked: 2 = x` reads it rather than
re-evaluating. Also fixes the open finding _A pin inside a function cannot see
local bindings_, where `blocked` over-blocks local helpers and literals.

**Slice 5 — fold at every call, behind a preflight.** The spec's _Comptime by
inference_ reading, now safe because folding records a known value instead of
rewriting a type. `map.get("a")` on a known map yields a known result.

The preflight is the fourth breakage and is not optional: a cheap syntactic gate
— every leaf of the call is a literal or a binding with a known value — must run
before any evaluator environment is constructed. State the measured cost of a
full `cargo test --workspace` before and after; if folding costs more than a few
percent, stop and report rather than shipping it.

Re-decide the 36 checker tests that `47fda25` broke **one at a time**. Several
encode soundness properties and must not be blanket-updated.

### Why fold-everywhere comes last

It is the most wanted and the least safe to do first. Attempted directly, as in
`47fda25`, it needs a lifting rule for `Optional`, `Result`, records and named
families, because folding rewrites types. Done after slice 3 it needs none of
them, because folding stops rewriting types. The ordering is the whole argument:
slice 3 is not preparation for slice 5, it is what removes slice 5's four known
defects.

### Conventions for this work

- Branch from `main` at `58eb0a8`. **Never `git push`.**
- Commit within the first twenty minutes — notes, the probe results, whatever
  the reading establishes — then every 20–30 minutes, red or green, prefixed
  `WIP:`. Do not save one commit for the end.
- Keep a done-note file updated as you go, not written at the end: root cause,
  what changed, decisions the task did not settle.
- Gates you own, and that the main thread will not re-run:
  `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. Baseline is **1818 passing**; a _drop_ in the count
  matters as much as a failure.
- The CLI package is named `aven`, not `aven-cli` (`cargo test -p aven`).
- Never `git add -A`; name paths.
- If a slice's diagnosis here is wrong, say so and propose the better shape
  rather than forcing it. A correct "this is actually X" is worth more than an
  implementation of our guess.

## Open findings from the three-agent review

Reviewed by Grok, Kimi K3 and GLM 5.3 Flash over `8cc9872..c61cd8f`. Four
findings were fixed (`3a29bf9`, `c8fd362`, `20f5d52`); these are confirmed and
left open, each with the probe that shows it.

**The `folded` shortcut accepts a pin without evaluating it.** (Kimi) An open
singleton row is taken as proof the value folded, but that shape is also what
ordinary inference produces for a literal through a type variable. The pin then
skips evaluation entirely, so a runtime dependency goes unreported:

```aven
f = (t: Int) =>
  x = comptime(Array.range(0, 5).fold(0, (a, b) => a + b + t))
  ok: 0 = x
  x
f(1)
```

`check` passes; `run` produces `15`, from the runtime parameter `t`. This
predates the slice, and the honest fix — require evaluation provenance rather
than a row shape — amounts to "always evaluate", which is a cost decision tied
to the open decision below.

**A pin does not apply a named family's `toText`.** (Grok) `eval_comptime_expr`
installs no family plans, so `comptime("${price}")` on a branded `Money` types
as `"10"` while the program prints `$10`. The spec requires the evaluator's own
rendering precisely to avoid this.

**A pin inside a function cannot see local bindings.** (Grok) `blocked` keeps a
parameter from resolving to a same-named module binding, but it also blocks
local helpers and local literals, so the motivating example works at top level
and fails one scope in.

**Top-level bindings are mutually recursive to the comptime evaluator and
sequential to the runtime.** (GLM) `comptime(double(later))` before `later = 3`
checks and narrows to `6`; running it reports `unbound name later`. The
underlying split predates the slice, but the pin now certifies a specific value
the program cannot produce.

**A wrapped interpolation cannot span lines.** (Grok) Inside a triple-quoted
string, `${"a"\n + "b"}` is reported unterminated. Multiline literals are the
first place a wrapped interpolation is reasonable, so this is new visibility
rather than a new rule.

**Margin validation reaches into `${...}` holes.** (Kimi) Every physical line of
a multiline literal must respect the margin, including expression lines inside a
hole. Values are never computed wrongly — the excess is in validation — but the
rule is not written down.

Smaller, confirmed: a closed literal union is not treated as refinable, so
`comptime(choose(1))` on `(1 | 2) -> 1 | 2` keeps `1 | 2` (conservative, not
unsound); a malformed `"""` opener emits two diagnostics for one defect; and
`crates/aven-fmt/tests/string_values.rs` decodes interpolation fragments with
`decode_string_literal` rather than the parser's own fragment path, so a decode
only the parser gets right would be invisible to it.

## Not run

- **MSRV.** CI checks Rust 1.91 with `cargo check --workspace --all-targets`.
  This machine has only 1.95 and no `rustup`, so that check was not run. The new
  code uses `matches!`, `take_while`, `is_none_or` and ordinary iterator
  methods, all well below 1.91, but the claim is untested here.
- **Older shells.** The bash 5.2.21 / fish 3.7.0 Docker verification from the
  previous slice was not repeated. The generated bytes are identical, so it
  should still hold, but it was not re-run.

## Checking cost is linear in module size again — 2026-09-14

Branch `perf/unify-trail`, three commits off `2e1073a`. This is the separate
workstream from the comptime repair plan's section 6, not a comptime finding.

Checking was quadratic in module size, with three independent causes. Each was
found by profiling rather than reasoning, and each is its own commit:

1. `eac9e97` — `Unifier::snapshot` deep-cloned the whole substitution on every
   unification. It is now a mark of four lengths, with a trail of what each
   in-place write overwrote; `restore` replays the trail backwards, then drops
   what was allocated past the mark.
2. `c3d78f3` — `fork_annotation_checker` copied `bindings` and `annotations`,
   both module-sized, once per annotation. Both are now `Rc`, mutated through
   `Rc::make_mut` in the one place that writes them.
3. `829aeb5` — `binding_for_declaration` and its annotation sibling scanned
   every item in the module per declaration. A by-name index, built once per
   pass, replaces the scan.

`aven check` on N annotated functions, and peak RSS unchanged throughout:

| N    | before | after |
| ---- | ------ | ----- |
| 1000 | 2.25s  | 0.75s |
| 2000 | 7.26s  | 1.49s |
| 4000 | 27.7s  | 2.85s |
| 8000 | 117s   | 6.05s |

Doubling N now doubles the time. `examples/cli.av`, which imports `std/cli`,
went from 1.60s to 0.20s.

Beyond the gates, every `.av` file in the repo — 364 of them — produces
byte-identical diagnostics before and after all three commits.

### Dropped: caching checked std modules across processes

Section 6 planned this third. It was worth doing when importing `std/cli` cost
1.60s of unavoidable re-checking; at 0.20s it would buy back a fraction of that
in exchange for a serialisation format, an invalidation rule keyed on std
content, and a new class of stale-cache bug.

**This reasoning has since been overturned by its own success** — see the next
section. The absolute cost fell, but everything around it fell further, so
re-checking `std/cli` is now about half of a small program's entire runtime.
The decision to drop it was correct on the day and is no longer.

## Startup is ~135ms of re-checking `std/cli` — 2026-09-14

Recorded, not fixed. Prompted by a real program: the Cox-trigrams course
harness stub takes 285ms where its Python equivalent takes 29ms, and the
question was whether a 10x gap is the price of typed CLI argument parsing.

It is not. Argument parsing at runtime is free; the gap is `aven check`
running in full on every invocation, and `std/cli` being far and away the most
expensive module to check.

| Measured                          | wall     |
| --------------------------------- | -------- |
| `aven run` on a trivial program   | 10--20ms |
| `aven run` on `import("std/cli")` | 150ms    |
| the stub, `aven check`            | 280ms    |
| the stub, `aven run`              | 280ms    |

`run` and `check` are the same number, so **evaluation is ~0ms** --- all 280ms
is front-end. About 135ms is checking `std/cli` itself and about 130ms is
checking the stub's own four `cli.define`/`cli.app` calls, which are comptime
row work over record schemas.

`std/cli` is an outlier rather than a general per-module cost. Every other std
module --- `array`, `map`, `set`, `result`, `time`, `clock` --- costs 10--20ms,
and `array.av` is 297 lines against `cli.av`'s 618. The cost is superlinear in
something `cli.av` has, not in its length.

A `perf record -g` profile of that check says where the cycles go: **~60% in
`malloc`/`free`**, driven by `String::clone` inside `Type::clone` and
`Vec<RowEntry>::clone`. This is allocator churn from deep-cloning owned type
trees whose every row entry owns its label `String`. It is not another
quadratic --- doubling module size still doubles the time, as the previous
section left it.

### Why caching bytecode would not help

The natural plan is to cache parsed or compiled output once there is a
bytecode interpreter. That saves single-digit milliseconds here: a trivial
program parses, checks and runs in 10--20ms total, so parsing is not the cost.
What is worth caching is the **checked** module --- `std/cli`'s inferred types,
so an importing program does not re-infer 135ms of rows every run.

### Levers, cheapest first

1. **Swap the global allocator** to mimalloc or similar. One line, no
   semantics, and the profile is 60% allocator.
2. **Intern row labels** as `Rc<str>` so cloning a type stops cloning its
   labels, and consider sharing `Type` subtrees the same way.
3. **Bake the checked std modules into the binary** at build time. The std
   sources are already compiled in, so a rebuild is the invalidation: no cache
   directory, no content hashing, and none of the stale-cache bug class an
   on-disk cache would introduce. This is the approved form of the item
   dropped above, whose cost-benefit has now inverted.

Only (3) touches the 135ms directly; (1) and (2) make every check cheaper,
including the ~130ms the stub spends on its *own* four `cli.define` calls,
which no amount of std caching can help.

### What it is not: the completion literals

An early guess was that `cli.av`'s last 240 lines --- bash and fish
completion-script string literals that almost no program calls --- were the
cost, and that splitting them into a `std/cli-completions` module would fix it
for free. Measured by checking the file in prefixes, that is wrong twice over,
and it is recorded here so nobody spends a slice on it:

| Slice of `cli.av`         | check |
| ------------------------- | ----- |
| lines 1--380              | 140ms |
| lines 381--618 (literals) | 20ms  |

The literals are 40% of the file and 13% of its cost. The split would not have
worked anyway: a module that merely imports `std/cli` still pays the full
150ms, so moving the literals into a module `std/cli` itself imports would
save nothing.

Nor is there a single hot construct in those first 380 lines --- the cost
accumulates smoothly across them, roughly 0.3ms per line. The comparison that
matters is with `array.av`, which is 297 lines and checks in 10--20ms, about a
twentieth the cost per line. What `cli.av` has and `array.av` does not is
pervasive record and row types: open rows (`{ .. }`), the `Meta` and
`Completion` records, and `keysOf` comptime work. That matches the profile,
where the clones are `Vec<RowEntry>` and the `String` labels inside them, and
it is why lever 2 is the primary attack rather than anything about `cli.av`'s
text.

## Background

- `docs/language-proposals-review.html` — the standalone review page the
  proposals were decided from. Historical cards are marked superseded.
- `/home/dave/Downloads/aven-language-review.md` — the user's own responses.
  Items 1, 3 and 5 (early return, shadowing, an `attempt` keyword) were not
  approved and nothing was implemented for them.
