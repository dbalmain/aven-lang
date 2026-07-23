# Check-fixture LSP surface goldens

Each `<stem>.hover` locks the top-level **binding** hover labels for the matching
valid check fixture:

```text
crates/aven-check/tests/fixtures/check/valid/<stem>.av
```

Report format (one line per named top-level binding that yields a hover, source
order):

```text
name : <type>
```

or, for comptime type bindings that hover as definitions:

```text
name = <type>
```

Labels are the fenced body from `hover_at_position` (markdown fences stripped).
Pattern/spread bindings without a single name are skipped. Multi-line hover
bodies are rejected by the report helper — keep goldens on single-line labels.

## Regenerating

There is **no auto-write** / `UPDATE_*` env. After intentional hover rendering
changes:

1. `cargo test -p aven-lsp --lib check_surface_goldens -- --nocapture`
2. On mismatch, copy the `--- actual ---` dump from the failure message into
   `tests/fixtures/check-surfaces/<stem>.hover` (keep the trailing newline).

Stems currently covered: `binding-forms`, `bound-local-binders`,
`bound-host-global`, `literal-types`, `nullability`,
`variant-match-exhaustiveness`, `value-keywords`, `string-interpolation`.
