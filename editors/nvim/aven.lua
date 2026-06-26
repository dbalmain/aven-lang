-- Aven editor support for Neovim (0.10+), no plugins required.
--
-- Drop this file somewhere on your runtimepath and `require` it, e.g.
--   require("aven")              -- if placed at lua/aven.lua
-- or source it directly:
--   :luafile path/to/aven.lua
--
-- It registers the `.av` filetype and starts `aven lsp` (the language server
-- built into the `aven` CLI) for those buffers. The server provides semantic
-- token highlighting, diagnostics, hover, go-to-definition, completion, and
-- rename -- the real parser/checker is the single source of truth, so there is
-- no separate grammar to maintain.
--
-- Requires the `aven` binary on $PATH (`cargo build --release -p aven` and
-- symlink target/release/aven onto your PATH, or point `cmd` at an abs path).

-- Recognize `.av` as filetype "aven".
vim.filetype.add({ extension = { av = "aven" } })

-- Start the language server on Aven buffers. Neovim 0.10+ applies the server's
-- semantic tokens to `@lsp.type.*` highlight groups automatically.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "aven",
  desc = "Start aven-lsp",
  callback = function(args)
    vim.lsp.start({
      name = "aven",
      cmd = { "aven", "lsp" },
      -- Aven scripts are self-contained; fall back to the file's directory so
      -- the server attaches even for loose, project-less `.av` files.
      root_dir = vim.fs.root(args.buf, { ".git" }) or vim.fs.dirname(vim.api.nvim_buf_get_name(args.buf)),
    })
  end,
})
