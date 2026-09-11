# Completion plan for Claude

Prepared 2026-09-12, Australia/Sydney. This is an implementation handoff for
the remaining approved language work. Read this before the historical narrative
in `PROGRESS.md`. Later explicit user instructions take precedence.

## Starting point and scope

Work in `/home/dave/w/clex/aven-lang` on
`comptime-unification-slices-1-2`. Current HEAD is `0e61f7b`.
Do not restart from the old `58eb0a8` or `3163938` baselines named in historical
plans. Read `/home/dave/w/clex/.ai/core.md` for repository instructions.

Already implemented:

- Raw and triple-quoted strings, formatter preservation, and adoption in
  `std/cli`; see `docs/language-literals-and-comptime.md`.
- Slice 1: a shared, capability-free, bounded evaluation demand path for `@`
  parameters, including ordinary helpers and diagnostic evidence.
- Slice 2: `comptime = (@arg) => arg` is an ordinary prelude function.
  There must be no compiler special case for its name. Generic prelude exports,
  lexical capture, visibility, constraints, and shadowing were repaired.
- Initialization-order repair, accepted at `55ac2d1`: explicit Artifact,
  RuntimeUnknown, and RuntimeKnown contexts; context-sensitive specialization
  caches; lazy initializer boundaries; imported-helper ordering; recursive
  equality termination. Preserve these repairs.

At `55ac2d1`, independent gates passed: 1845 workspace tests, including 673
checker unit tests; workspace clippy, formatting, and diff checks. `1732081`
records that acceptance. These are historical results, not a fresh gate on HEAD.
Subsequent commits `f054152` and `0e61f7b` pin Rust/clippy to 1.98.1, retain
MSRV 1.91.0, adjust SHA-256 code, and add a Nix development shell. Preserve them.
At handoff, the only untracked content before this document was `tmp/`, the
user's scratch area. Do not stage it.

**Slices 3–5 have not started.** Complete them in the order below. Also close
or explicitly disposition the remaining string and verification findings.
Early return, relaxed shadowing, and an `attempt` keyword are not approved;
do not implement them. The user wants this foundation complete before further
language design work.

## Settled semantic contract

1. Knowledge is semantic evidence beside an ordinary inferred type. It must
   not flow through `Type`, unification, or type rendering as inferred singleton
   narrowing. Explicit literal types and signature constraints remain valid.
2. All `@` parameter functions use the same demand machinery as the prelude
   `comptime`. A runtime dependency cannot count as known just because its
   inferred type resembles a singleton. Successful evaluation needs provenance.
3. A known **present optional** may implicitly satisfy a nonoptional expected
   type, including a literal type, when its payload satisfies that type.
   Annotated bindings and typed arguments are proof-demand sites. No `!`
   assertion operator is needed. An absent or unknown optional cannot use this
   conversion. Ordinary inferred optional types stay optional.
4. The user accepts that replacing an upstream known value with runtime input
   can break distant typed uses that relied on proof. Do not repeat the older
   claim that downstream breakage is impossible.
5. Unknown, absent, unsupported, deferred specialization, and evaluation failure
   are different states. Unsupported evaluation at an explicit demand fails
   conservatively; an ordinary opportunistic fold may remain unknown. Do not
   guess a value or discard a real type/bound error to try another evaluator.
6. Preserve presence, numeric kinds, named-family ownership, and runtime
   rendering behavior. A branded `Money` is not a raw integer merely because
   its payload is an integer. Proofs must agree with runtime elaboration.
7. Knowledge discovered inside a function does not silently strengthen its
   public signature. Known results may be established at particular calls by
   valid evaluation. Preserve runtime order and module/lexical isolation.

## Priority 0 — establish a reproducible baseline

- Check the branch, diff, and current tool versions. Use `nix develop` if needed
  for the repository's pinned toolchain. Do not overwrite unrelated changes.
- Read `PROGRESS.md`, `docs/comptime-implementation-progress.md`,
  `docs/comptime-prelude-progress.md`, and `docs/language-literals-and-comptime.md`.
  Older failed gates and temporary agent ownership in PROGRESS are historical;
  the September 11 accepted ordering checkpoint supersedes them.
- Run the normal gates below before modifying source. Record failures and
  timing separately from the accepted historical baseline.
- Reproduce the motivating positive and negative controls, using a rebuilt
  CLI with its normal prelude. Bare checker/evaluator APIs intentionally do
  not invent a `comptime` builtin.

```aven
join = (parts: Array(Text)): Text => parts.joinWith("\n")
script = comptime(join(["a", "b"]))
checked: "a\nb" = script
```

```aven
comptimeAdd = (@a: Int, @b: Int): Int => a + b
checked: 4 = comptimeAdd(1, 3)
# Separate negative program: checked: 5 = comptimeAdd(1, 3)
```

Record warm checker-only and representative CLI checking times, not just
workspace elapsed time. The existing `completion_tool.av` fixture is a useful
representative workload. Old measurements in PROGRESS used a different
checkpoint/toolchain and are context, not the comparison baseline.

## Priority 1 — slice 3: semantic knowledge and proof-based typed uses

This is the next source change. Make the replacement proof checks land with
removal of computed narrowing, so existing literal annotation programs stay
valid throughout the accepted commit.

### Implementation approach

- Introduce a checker-owned knowledge representation separate from `Type`.
  Retain actual semantic evaluator values or a faithful owned representation;
  do not serialize to display text and parse it back. Keep compiler artifacts
  such as reified types and label sets distinct from runtime value evidence.
- Design identity before adding a map. An expression span alone is unsafe
  across imports, lexical scopes, and repeated specializations. Use stable
  source/module and expression/binding identity plus the relevant lexical and
  specialization context, or an equivalently isolated per-context structure.
  Proof reuse must also respect execution/initialization context. Keep canonical
  recursive type identity independent of demand context.
- Start at the existing successful explicit-demand sites. Record proof beside
  the ordinary result type. Preserve it through the minimal binding/name path
  needed for `script` in the example above; full ordinary local propagation is
  slice 4. Represent optional absence and presence explicitly enough that
  absence cannot be mistaken for no knowledge.
- Route annotations and all typed argument paths through one proof rule,
  including named arguments and other call forms supported by the checker.
  Verify payload compatibility as well as presence; proof is not permission
  to bypass a family, bound, or literal mismatch.
- Remove computed singleton narrowing once the proof consumers work.
  Audit `literal_type_refines`, `narrow_to_literal`, and any related narrowing
  or numeric-finiteness helpers; remove obsolete ones and explain any retained
  uses. Keep explicit literal-type checking and ordinary literal inference.
- Review numeric evidence carefully, including integer/float distinction and
  nonfinite results. Removing a narrowing-only guard is not permission to
  accept a value the runtime or expected type cannot represent.
- Make mismatch diagnostics use the known evidence when relevant, without
  exposing internal named-family owner IDs.
- Inspect family elaboration before certifying family-dependent values.
  Supply the runtime-equivalent plan where feasible. Otherwise explicitly
  reject that demand conservatively and document the remaining support gap;
  never accept an incorrectly rendered or unbranded proof. Broad family support
  may be a separate reviewed commit before slice 5.

### Source map (verify names; line numbers drift)

- `crates/aven-check/src/checker/inference.rs`:
  `infer_comptime_param_call`, `evaluate_comptime_param_argument`,
  `narrow_to_literal`, and `literal_type_refines`.
- `crates/aven-check/src/checker/type_checking.rs`:
  `check_value_against_target` and `check_call_arg_against_param`.
  Audit inference/value/annotation callers rather than fixing just one path.
- `crates/aven-check/src/checker.rs`: checker state and demand outcomes.
- `crates/aven-check/src/comptime.rs`: `ComptimeValue`, evaluator conversion,
  execution contexts, and specialization identity.
- `crates/aven-eval/src/lib.rs`: semantic `Value`, scopes, closures, and bounded
  lazy evaluation. Check wrapper/owner fidelity rather than assuming its
  equality behavior is suitable for proof comparison.
- `crates/aven-compiler/src/modules.rs`: module identity, checked metadata,
  family coercions, and primitive-family runtime plans.

### Acceptance tests

- The two positive examples above pass; wrong literal annotations fail.
- Computed `@` results retain base types; declared literal return types remain
  literal. Assert inferred types directly, not merely successful checking.
- Known present optional payload 1 satisfies `Int` and literal `1` at both
  bindings and typed arguments; literal `2` fails. Absent and runtime-unknown
  optionals fail nonoptional demands. The inferred optional type remains intact.
- Mixed `1 | 1.0` joins retain their type; known-value discovery does not choose
  a branch's type. Named-family evidence cannot satisfy an unrelated raw type.
- Same source spans in different modules and repeated specializations cannot
  share another value's proof. Negative controls expose incorrect reuse.
- All accepted ordering, artifact-scalar, prelude-capture, and recursive-equality
  regressions remain green. Include check/run agreement for accepted proofs.
- Re-test the historical runtime-dependent fold example in PROGRESS. It must
  not certify a value from singleton-shaped inference without evaluation.

## Priority 2 — slice 4: lexical binding propagation

- Propagate knowledge through ordinary initialized local and top-level bindings,
  aliases, captures, and supported expression forms without changing types.
  `x = 1 + 1; checked: 2 = x` should obtain evidence through the knowledge path.
- Distinguish actual local helpers/literals from blocked runtime parameters;
  repair the historical case where a pin inside a function cannot see its
  local bindings. Do not let a blocked parameter resolve to a same-named module
  binding, or assume a runtime parameter is known during unspecialized checking.
- Respect `:=` shadowing with binding identities and correct proof lifetimes.
  Keep specializations and imported captures isolated.
- Reuse valid known results rather than repeatedly reconstructing an evaluator.
  Do not reuse results across incompatible initialization boundaries.
- Test nested scope positives, unknown-parameter negatives, shadowing, captures,
  repeated calls with different inputs, and import isolation. Existing
  unknown-runtime lambda tests must not be relaxed without an actual proof of
  invocation availability.

## Priority 3 — semantic transport and evaluator lifetime gate

Before broad folding, audit supported semantic values: scalars, absence,
containers, variants/results, and named families. Expand transport where it
preserves runtime semantics; retain explicit unsupported outcomes elsewhere.
Maintain a support table so a conservative rejection is visible rather than
mistaken for completed support.

The existing ownership concern needs measurement and resolution: a scope can
memoize a closure that captures that same scope through `Rc`; ambient methods
also capture environments. Repeated evaluation can retain scopes and ASTs.
Inspect lifecycle/cleanup or sharing, test retained memory under repeated
checks/folds, and preserve closure captures. Do not introduce a long-lived
global evaluator cache as a shortcut. Record whether the concern is reproduced
and how the chosen design prevents unbounded retention in this workload.

## Priority 4 — slice 5: opportunistic evaluation at ordinary calls

- Evaluate ordinary calls when their demanded inputs are known; record results
  beside the unchanged inferred type. No new syntax or special function names.
- Add a cheap preflight **before** constructing/cloning evaluator environments.
  It must account for the callee, lexical captures, and demanded dependencies,
  not just visible arguments. Known arguments alone do not prove a closure pure
  of runtime dependencies. Evaluation stays capability-free and fuel-bounded.
- Unknown/unsupported opportunistic computations may stay unknown. Explicit
  `@` and typed proof demands still enforce their contract and report failures.
- Test known map lookups with present/absent results, local helper calls,
  captures, family rendering, mixed-type joins, and runtime-dependent negatives.
- Revisit the 36 historical failing tests from `47fda25` individually. Record
  each changed expectation's semantic reason; do not blanket-update snapshots.
- Compare repeated warm checker and representative CLI timings against the
  fresh baseline, as well as workspace times and memory. If checking regresses
  by more than a few percent, investigate and optimize before acceptance;
  report an unresolved cost rather than silently shipping it.

## Priority 5 — remaining findings and final documentation

Reproduce these historical findings on the current tree before changing code:

- Multiline `${...}` expressions inside triple-quoted strings: repair rejection
  of otherwise valid continued expressions under the existing grammar. Preserve
  nested string/interpolation parsing and add formatter round-trip coverage.
- Margin validation inside interpolation holes: document the current rule and
  test it. If a different rule needs a language decision, separate that decision
  from this implementation; do not invent new syntax.
- Malformed triple-string opener: avoid duplicate diagnostics for one defect.
- Formatter string-value tests: use the parser's interpolation-fragment decoding
  path so a discrepancy between fragment and standalone decoding is detectable.

The old closed-literal-union refinement finding should be covered by slice 3's
proof checks, not by reintroducing narrowing. Confirm this explicitly.

Update `docs/language-literals-and-comptime.md`, the relevant implementation
notes, and historical status cards in `docs/language-proposals-review.html`
to distinguish implemented behavior from remaining limitations. Review the
parent `/home/dave/w/clex/docs/language-spec.md` for stale builtin/narrowing
claims; if it is outside writable scope, prepare an exact patch and record
that application is outstanding. Do not silently claim it was changed.

Re-run older-shell verification using existing repository shell tests and the
previous bash 5.2.21 / fish 3.7.0 setup where available. Recheck generated output
equivalence when relevant. Run MSRV 1.91.0 checking, or record the precise
environment blocker; ordinary current-toolchain tests do not establish MSRV.

## Validation and handoff discipline

For each coherent source slice, review the diff for unsound acceptance and
checker/runtime disagreement, run focused meaningful positive/negative tests,
then the final workspace gates:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

Use the pinned 1.98.1 development environment. For MSRV use an actual 1.91.0
toolchain with `cargo check --workspace --all-targets` (see CI's explicit
`RUSTUP_TOOLCHAIN` override). The CLI package is `aven`, not `aven-cli`.
Rebuild after changes to embedded `std/*.av` sources.

Some host tests require local socket access. Distinguish environment failures
from test failures; a partial sandboxed run is not a complete green gate.
Explain test-count changes, especially decreases. Use direct CLI check/run
controls as well as unit tests for evaluator agreement. Do not weaken existing
soundness tests just to regain a green suite.

Keep one source owner for overlapping checker files. If using agents, the user
prefers frugal Luna/Terra coding with independent quality review. A second agent
should have a genuinely separate bounded task. Update PROGRESS during work
with current commit, exact remaining task, owner, tests, and known failures;
clearly label any WIP checkpoint. Commit named paths at reviewable boundaries;
never `git add -A`, never include scratch files, and never push without a new
explicit request. Do not treat the presence of an origin tracking branch as
push authorization.

Completion means slices 3–5 meet their acceptance criteria, remaining findings
have a documented disposition, check/runtime agreement and performance gates
pass, and docs describe the actual behavior. Report any unsupported semantic
forms or unavailable external checks as limitations rather than declaring them
implemented or verified.
