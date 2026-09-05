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
- **cb26160**: Q1 checker spike and resumed findings.
- **36a952d**, minimal parser: real `std/cli` Aven module with typed decoder
  descriptors, long flags, separate/attached options, defaults and conversion
  errors. `cli.parse(spec, args)` infers `Result({ jobs: Int, verbose: Bool }, Text)`
  in a `type_at` test using the actual library source. CLI check/run and a six-case
  `.av` suite pass. This is a thin path: unknown tokens and option-value interactions
  still need the next tokenizer slice.
- **1726944**, committed by the user after the second interruption: `cli.define`
  prepares and captures keys and metadata once, returning the typed parser;
  short/long aliases, per-option help and value labels, required options,
  single-pass tokenization, repetition/unknown-argument errors, and usage plus
  examples in errors. All parsing/help rendering is Aven. This checkpoint was red.
- Commands and comptime completions: not built yet.
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
Defaulted comptime key sets derived from a runtime record's static shape now
specialize `parse(spec, argv)` to a heterogeneous result. General checker fixes:
optional comptime arguments, closed keys independent of unresolved payload types,
record-iteration binder deferral, namespace-qualified specialization and its error
propagation context. Runtime parameter mismatches now diagnose rather than silently
deferring this specialization path.

The decoder shape currently is `cli.option(cli.int, { default: 1 })`, not
`cli.option(Int, ...)`. `spec` is lowercase because it is an ordinary value.
Descriptors and normalized metadata are built once by `cli.define`. The returned
parser captures the key set; repeated `cli.parse` calls no longer recompute
`keysOf` or construct argument metadata. `define` still runs at runtime; this
is one-time initialization, not general compiler residualization.

Imported lowercase specialization now captures the defining module's private
polymorphic value schemes and type aliases. Imported body type spans are kept
out of the caller's inferred-type table, and specialization diagnostics are
anchored at the call to avoid foreign source offsets crashing CLI rendering.
The new private-helper capture integration test still needs the full gate run.

## Resume 2: reconcile red checkpoint first

- Mixed-kind `valuesOf` acceptance was **an accident**, not an intended language
  semantics change. The statement-check path validated only its broad record
  parameter, bypassing the inference path's homogeneous-field check. Both paths
  now call the same real implementation. Its failed unification now reports
  directly: the generic mismatch helper suppressed diagnostics for open literal
  rows, even when their base kinds were known to disagree. Arity is validated
  on this same path. The rejection test is retained and expanded with bound,
  annotated, and excess-argument calls.
- Correction to the resume's suspicion: `std/cli` does **not** call `valuesOf`
  on heterogeneous descriptors. It projects each descriptor to the same `Meta`
  record type first. Heterogeneous parsed values stay in a record comprehension.
  `valuesOf` itself is a new primitive introduced on this branch, not a baseline
  operation whose accepted input kinds were intentionally changed.
- Private uppercase functions were being inferred as runtime values while
  collecting the new lexical environment. Skip those declarations (retaining
  the existing exported-value behavior); comptime evaluation remains their owner.
  `nested_uppercase_comptime_applications_check_cleanly` is unchanged.
- New Rust primitive `valuesOf` bridges a homogeneous record to an Array. Aven
  can project fields by record comprehension but cannot otherwise collect them
  into an Array. Minimal motivating expression:
  `valuesOf({ keysOf(schema) -> k; (k, schema[k].meta(k)) })`.
  The primitive performs no CLI parsing, metadata policy, or help generation.

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

Resume thin-path validation: all 627 checker unit tests and seven CLI argument
integration tests pass; real Aven suite checks and runs. Full gates still pending.

Resume 2 reconciliation: all **628 checker unit tests pass**; workspace clippy
with `-D warnings` passes. Full workspace test run is in progress.

- **220c6fd**: fixes both checker regressions, saved as WIP when the workspace
  run reached two additional failures: the standard-library count omitted
  `std/cli` (9 → 10), and my new private-helper test expected the wrong spacing
  in the evaluator's record rendering. Both expectations are being corrected;
  the actual private polymorphic capture check and evaluation succeeded.

Resume 2 full gates: `cargo fmt --all` clean; workspace clippy with
`-D warnings` passes; `cargo test --workspace` **1775 passed / 0 failed**,
up from 1767. Existing HTTP tests require loopback socket permission, so the
full successful test run used the approved sandbox escalation. No tests skipped.
The private-helper lexical capture integration test and the 24-case Aven CLI
suite both pass. General comptime constant residualization remains unimplemented;
`define` already removes key-set recomputation from repeated parser calls.
