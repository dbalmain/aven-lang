# CLI arguments implementation — running done-note

Branch: `cli-args`, based on `b22ba54`. Never pushed.
The resume explicitly selects this path. The resumed baseline is `637181e`,
with **1767 collected tests passing**. Layer 0 and Q2 are settled.

## Q1

Sibling-field handler constraint: **no** — `run: (Args(spec.args)) -> Int = spec.run`
inside `command = (@spec) => ...` rejects `spec.args` with
`comptime.argument-not-known` during generic checking.

Re-run on resume; captured by the real checker test
`sibling_derived_handler_annotation_reports_comptime_gap`. Command target: B.

## Slices

- Layer 0 implemented, **4817671**: `aven run` script arguments, direct shebang forwarding,
  explicit `Host::register_args(program_name, args)`, `args: Array(Text)` and
  `programName: Text` in the standard checker surface. Basename preserves the
  extension (`tool.av`), as requested by “basename”.
- Parser, aliases/help, commands, usage/examples, completions: not built yet.
- Exit codes implemented, **d2bf45b**: Int entry values select exit
  status, with portable range 0–255; invalid/oversized values diagnose instead
  of silently truncating. Other entry values keep their existing display/error
  behavior. `aven test` is independent.
- Exit audit: the corpus has no `.av` files; its JSON tasks generate suites run
  by `aven test` (`aven-bench/adapters/lang/aven.ts:742`). Twelve CLI tests relied
  on integer-entry printing; their computations remain covered using explicit
  text output. A syslog acceptance fixture now returns 0. No examples needed
  modification. This is real evidence against implicit Int exit semantics,
  reported before implementation; the requested choice is implemented.
- **72f1c97**, committed by the user after the interrupted run: combined
  positional argv and the shebang transport separator.
- **637181e**, by the user: update the exact standard-global names test for
  `args` and `programName`. My Layer 0 slice missed this test; the user fixed
  the only workspace failure and ran all gates successfully.

## Resumed implementation

The static-coercion guess is being replaced with typed decoder functions in
descriptors. This keeps coercion user-extensible and inferable through ordinary
function types; it does not require putting a CLI-specific type switch in Rust.
The next spike is defaulted comptime key sets derived from a runtime record's
static shape, so `parse(spec, argv)` can return a heterogeneous record while
metadata remains an ordinary descriptor value.

## Evidence and corrections

- Baseline source count verified: 1752 `#[test]` functions. Actual baseline
  collection: **1762 tests**, from `cargo test --workspace -- --list`.
- Confirmed argv is not script-visible at baseline; host session argv is only
  logging. Confirmed `Run` lacks trailing args and prints Int entry values.
- Cited checker feasibility tests and parser/checker `@param` machinery exist.
  Their wider implications for heterogeneous CLI output remain to be tested.
- The cited feasibility tests pass (in baseline collection), but do **not**
  establish the spec's claim “features 1 and 6 need no new compiler capability.”
  A single reflected label-set result is less demanding than heterogeneous
  descriptors with metadata, converters, and command payloads.
- `tagsOf` handles closed **tag-only** variants; it defers for literal rows and
  rejects Bool in type position. Thus free enum/Bool completions do not follow
  from the implemented primitive.
- Aven-defined receiverless statics are not implemented. `Int { fromArg =
  (text: Text) => text.toInt() }` is rejected (`parse.expected-record-entry`).
  `register_type_with_statics` does install Rust statics, but cannot establish
  the proposed Aven-level user-extensible coercion protocol.
- The language-spec `(k, v)` record-iteration binder is not implemented by the
  parser. The working form is `keysOf(spec) -> k; (k, spec[k])`.
- Module-qualified lowercase comptime calls do not specialize at baseline;
  destructured imported functions can. A small general fix is being assessed.
- Metadata in a type-bearing descriptor is reified into **types**, not ordinary
  runtime values. Reading a field at runtime can produce `runtime.missing-type-member`.
  The descriptor shape needs real structured comptime values, not just type
  reification masquerading as values.

## Compiler gaps / unsettled design

Minimal reproducers and prioritization will be expanded as spikes conclude.
No new runtime dependencies. No Rust argv tokenizer or shell generator.

## Validation

Layer 0: `cargo fmt --all`, both new CLI argument integration tests, and the
existing host/checker surface parity test pass. Baseline collection completed.

Exit-code slice: `cargo fmt --all` and **all CLI tests pass** (`cargo test -p aven`).
