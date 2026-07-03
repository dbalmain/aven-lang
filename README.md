# Aven

Aven is an experimental typed glue language: Hindley-Milner inference with
row-polymorphic records/variants and literal types, a Zig-style comptime layer,
and a type-safe host/script boundary, built tooling-first (diagnostics,
formatter, and LSP grew ahead of the runtime). Programs run today through a
tree-walking evaluator with a host-provided platform (files, stdio, HTTP, JSON,
structured logging).

## Commands

```bash
cargo run -p aven -- check examples/hello.av        # parse + name + type checks
cargo run -p aven -- check --format json file.av    # machine-readable diagnostics
cargo run -p aven -- check --timings file.av        # phase timings
cargo run -p aven -- run examples/maps.av           # execute against the host
cargo run -p aven -- fmt --check examples/hello.av  # formatter
cargo run -p aven -- explain parse.unclosed-delimiter
cargo run -p aven -- tokens file.av                 # lexer debug
cargo run -p aven -- layout file.av                 # layout debug
cargo run -p aven -- lsp                            # language server (stdio)
```

## Workspace

- `crates/aven-core` — source spans, structured diagnostics, the diagnostic code
  registry and explanations
- `crates/aven-parser` — lexer, layout pass, unified-grammar parser (types and
  patterns are ordinary expression terms), declarations, name analysis
- `crates/aven-fmt` — source formatter
- `crates/aven-check` — semantic checking: annotation lowering, HM inference,
  row polymorphism, literal types, the comptime evaluator
- `crates/aven-compiler` — compiler database: snapshots, timings,
  declaration-keyed artifacts and invalidation
- `crates/aven-eval` — tree-walking evaluator and runtime values
- `crates/aven-host` — the typed host boundary: value+type registration,
  typed-fn adapter, host comptime resolvers, file/stream/HTTP/JSON capabilities
- `crates/aven-lsp` — language server implementation
- `crates/aven-cli` — the `aven` command-line interface

## Examples

`examples/*.av` are executable documentation, locked by integration tests: every
example must check cleanly, and hermetic ones also run with asserted output.

## Plans

- [`docs/tooling-first-plan.md`](docs/tooling-first-plan.md) — the milestone
  plan and its current queue (the authoritative "what's next")
- Editor setup lives in [`editors/`](editors/README.md)
