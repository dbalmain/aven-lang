# CLI arguments implementation — running done-note

Branch: `cli-args`, based on `b22ba54`. Never pushed.
The request supplied no `-o` path; this is the provisional output pending clarification.

## Q1

Sibling-field handler constraint: not spiked yet.

## Slices

- Layer 0 implemented (commit recorded in next update): `aven run` script arguments, direct shebang forwarding,
  explicit `Host::register_args(program_name, args)`, `args: Array(Text)` and
  `programName: Text` in the standard checker surface. Basename preserves the
  extension (`tool.av`), as requested by “basename”.
- Parser, aliases/help, commands, usage/examples, completions: not built yet.
- Exit-code audit and change: pending; will be a separate commit.

## Evidence and corrections

- Baseline source count verified: 1752 `#[test]` functions. Actual baseline
  collection: **1762 tests**, from `cargo test --workspace -- --list`.
- Confirmed argv is not script-visible at baseline; host session argv is only
  logging. Confirmed `Run` lacks trailing args and prints Int entry values.
- Cited checker feasibility tests and parser/checker `@param` machinery exist.
  Their wider implications for heterogeneous CLI output remain to be tested.
- No corrections to the design spec established yet.

## Compiler gaps / unsettled design

Not established yet. No new runtime dependencies.

## Validation

Layer 0: `cargo fmt --all`, both new CLI argument integration tests, and the
existing host/checker surface parity test pass. Baseline collection completed.
