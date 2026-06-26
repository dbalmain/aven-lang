# Editor support

Aven's editor integration is driven entirely by the language server built into
the `aven` CLI (`aven lsp`), so highlighting and IDE features share the real
parser and checker — there is no separate grammar to keep in sync.

## Build the binary

```bash
cargo build --release -p aven
# put it on $PATH, e.g.
ln -sf "$PWD/target/release/aven" ~/.local/bin/aven
```

## Neovim

`nvim/aven.lua` is a self-contained, plugin-free config (Neovim 0.10+). It
registers the `.av` filetype and starts `aven lsp` for those buffers; Neovim
applies the server's semantic tokens to `@lsp.type.*` highlight groups
automatically. See the file header for how to load it.

Features available once the server attaches: semantic-token highlighting,
diagnostics, hover, go-to-definition, completion, document symbols, and rename.

## Tree-sitter

Deliberately not provided yet. A tree-sitter grammar would be a second parser to
maintain against a still-moving spec; the LSP semantic tokens cover highlighting
from the canonical parser today. Revisit once the surface syntax stabilizes and
indentation / text objects / structural editing become worth the upkeep.
