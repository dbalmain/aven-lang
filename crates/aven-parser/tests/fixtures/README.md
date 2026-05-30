# Parser And Lexer Fixtures

Parser and lexer fixture tests use paired golden files.

- `parser/valid/*.av` must parse without diagnostics.
- `parser/ast/valid/*.av` must parse without diagnostics and have a matching
  `.ast` file with the same stem.
- `parser/invalid/*.av` must have a matching `.diag` file with the same stem.
- `lexer/valid/*.av` must lex without diagnostics and have a matching
  `.tokens` file with the same stem.
- `lexer/invalid/*.av` must have a matching `.diag` file with the same stem.
- `layout/valid/*.av` must lex and layout without diagnostics and have a
  matching `.layout` file with the same stem.
- `layout/invalid/*.av` must have a matching `.diag` file with the same stem.
- `.tokens` files contain one structured token per line:
  `start..end description`.
- `.layout` files contain one parser-facing layout token per line:
  `start..end description`.
- `.diag` files contain structured diagnostic summaries, not terminal output:
  `severity code: message`, then indented `label start..end: message` lines,
  then indented `note: message` lines.
- `.ast` files contain compact parser tree summaries. They intentionally omit
  spans and semantic meaning so they only lock parser shape.
- After an intentional diagnostic change, run `cargo test -p aven-parser`,
  compare the failure's actual output, and update only the affected golden file.
