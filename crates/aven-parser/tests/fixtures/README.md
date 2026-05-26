# Parser Fixtures

Parser fixture tests use paired golden files.

- `parser/valid/*.av` must parse without diagnostics.
- `parser/invalid/*.av` must have a matching `.diag` file with the same stem.
- `.diag` files contain structured diagnostic summaries, not terminal output:
  `severity code: message`, then indented `label start..end: message` lines,
  then indented `note: message` lines.
- After an intentional diagnostic change, run `cargo test -p aven-parser`,
  compare the failure's actual output, and update only the affected `.diag`.

