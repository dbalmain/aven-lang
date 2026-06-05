# Aven

Aven is an experimental typed glue language.

This repository is the Rust implementation. The first milestone is not a full
language runtime; it is a tight toolchain loop:

- parse enough syntax to produce useful diagnostics
- format source consistently
- expose diagnostics through an LSP server
- keep the CLI stable as the compiler grows underneath it

## Commands

```bash
cargo run -p aven -- check examples/hello.av
cargo run -p aven -- explain parse.unclosed-delimiter
cargo run -p aven -- fmt --check examples/hello.av
cargo run -p aven -- lsp
```

## Workspace

- `crates/aven-core` - shared source spans and diagnostics
- `crates/aven-parser` - lexer/parser skeleton
- `crates/aven-fmt` - source formatter
- `crates/aven-lsp` - language server implementation
- `crates/aven-cli` - `aven` command-line interface

## Plans

- [`docs/tooling-first-plan.md`](docs/tooling-first-plan.md) - parser,
  diagnostics, formatter, CLI, and LSP milestones
