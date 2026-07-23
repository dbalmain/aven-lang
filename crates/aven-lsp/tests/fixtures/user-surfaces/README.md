# User-code LSP surface goldens

Paired `.av` + `.hover` snippets for **user code** that uses host/std-shaped
types (Result from File/Json, Array methods, Map, user-defined records).

Each `.hover` is the same top-level binding report format as
`check-surfaces/` (see that README). Tests load via
`parsed_document_with_semantics`.

Host-shaped goldens (especially `result-open` with full `File.open` Result
shapes, and free type vars like `Map(a, b)`) are expected to update when host
API rendering or var printing changes — that is intentional.

Required stems: `result-open`, `array-methods`, `map-value`, `user-record-fn`.

## Regenerating

No auto-write. Run `cargo test -p aven-lsp --lib user_surface_goldens --
--nocapture` and, on mismatch, paste the `--- actual ---` dump into the
matching `.hover` sibling.
