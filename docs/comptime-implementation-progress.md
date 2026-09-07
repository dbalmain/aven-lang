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
