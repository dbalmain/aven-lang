# Comptime implementation progress

## Slice 1 — `@` parameters use the strong demand path

`@` arguments now first use the type-position evaluator for compiler-only
values, then the capability-free Aven evaluator for ordinary helper bodies.
The old lowercase-call guard was only a diagnostic shortcut: it rejected
reducible helpers before the evaluator could inspect their dependencies. It
was not an effects or soundness boundary, since the evaluator has a fixed fuel
budget and installs no host capabilities.

Direct label sets still retain their member spans for domain diagnostics.
When ordinary evaluation fails, the `@` demand reports the dependency or
resource cause at the argument instead of silently treating the failure as an
unknown value. Unsupported structured evaluator results remain conservative
failures until the later known-value representation can carry them.

Probes reproduced before this change:

- `comptimeAdd(1, 3)` satisfies `4` and fails `5`.
- `pin(join(["a", "b"]))` previously failed while builtin
  `comptime(join(["a", "b"]))` passed.

The slice test now makes both forms satisfy `"a\\nb"`, and separately proves
a runtime parameter remains rejected with its `unbound name` evidence.

Staged, not fixed here:

- The old builtin pin's singleton-type shortcut can certify a runtime
  dependency without evaluator provenance; slice 2 removes that builtin path.
- Family interpolation and top-level forward-reference behavior still belong
  to later evaluator/environment work and retain their existing staged probes.

### Sequential top-level demand — verified September 11

Lazy evaluation carries an initialization boundary for each module binding.
Lookup checks availability before reading cached values and evaluates earlier
initializers under their own boundary. Closure bodies use the caller boundary,
so a closure may capture a binding initialized before the call.

Checker evaluation distinguishes compiler artifacts, unknown runtime execution,
and a known runtime initialization point. Type annotations use artifact context;
unspecialized runtime lambda demands conservatively reject unproven module
reads. This patch does not add invocation-sensitive knowledge propagation.
Specialization caches include execution context while recursive type identity
remains canonical. Direct/pinned/helper/import forward references and cache
isolation have regression coverage, alongside valid artifact-derived scalars.

Integration exposed repeated unfolding during equality of generic recursive
values. Equality now peels absence wrappers before checking recursive identity
and tracks visited type pairs, preserving mismatched-field checks.

Final validation: **1845 workspace tests pass**, including **673 checker tests**;
workspace clippy, formatting, and diff checks pass. The cache regression was
also mutation tested. Logs: `/tmp/aven-order-final-test.log` and
`/tmp/aven-order-final-clippy.log`. Root independently verified CLI failure and
success controls. Semantic knowledge remains the next implementation slice.

### Slice 1 repair — 2026-09-08

The old `pick(bad())` regression was a builtin-shadowing defect in the
**type-position** evaluator: it dispatched the builtin `pick` before looking
up the user function named `pick`. Ordinary literal-returning helper bodies
were already evaluable. User functions now take precedence for all the
reflection, selection, `typeOf`, and pin builtin names. Regression coverage
checks both matching and mismatching literal-valued fields.

Argument demands now carry `Known`, unresolved-specialization `Deferred`,
`Unsupported`, or `Failed` with diagnostic evidence. Failure reporting consumes
that result instead of repeating the evaluator. The temporary lowercase-call
and whole-annotation guards have been removed.

Two old expectations were updated with specific transport evidence: a bound key set cannot yet cross the
runtime-value conversion boundary, and a record containing a handler closure
cannot either. Silently accepting either at an explicit `@` demand conflicts
with the agreed conservative-unsupported rule. The sibling-handler test also
has its existing, separate `Args(spec.args)` annotation diagnostic.

Real type-evaluation errors (including parameter bounds) stop the demand;
only unsupported type walks may fall back to ordinary evaluation. Runtime
failure notes retain the cause, but their labels stay at the demand because
ambient-module source offsets have no source identity yet. Host value
parameters retain their existing optional-refinement contract; they are not
user-written `@` demands.

Validation: full `aven-check` passes **658 unit tests and 2 fixture tests**,
including the nested-bound regression, literal-type/provenance regression,
and builtin-shadowing matrix. Checker execution was **3.09 seconds**. Workspace
format and clippy checks passed before the final regression-only addition;
the final checker package clippy check also passed. The sandboxed workspace
run reached the host tests and hit five local-socket permission failures;
its log is `/tmp/aven-demand-repair-workspace.log`. The approved rerun with
socket access is `/tmp/aven-demand-repair-workspace-unsandboxed.log` (running
at this commit). Final checker log: `/tmp/aven-demand-repair-check-final.log`.
