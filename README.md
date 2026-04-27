# 🪜 lean-to

Zed extension that adds Lean 4 support with a more native feel through heterodox means.

## How it works

`lean-to` runs `lake serve` behind a proxy that augments the LSP with features Zed otherwise wouldn't get from a vanilla Lean language server.

## Features

Each feature is on by default and individually toggleable:

1. Inline goal state `leanTo.inlayHints`
2. Code lenses above theorems `leanTo.codeLens`
3. Augmented semantic tokens `leanTo.semanticTokens`
4. Hover with goal state `leanTo.hover`
5. Elaboration progress bar `leanTo.progress`
6. Auto-restart on outdated imports `leanTo.autoRestart`

All feature flags live under `initialization_options.leanTo` for the `lean4-lsp` server in your Zed `settings.json`:

```json
{
  "lsp": {
    "lean_to_proxy": {
      "inlayHints": true,
      "codeLens": true,
      "semanticTokens": true,
      "hover": true,
      "progress": true,
      "autoRestart": true
    }
  }
}
```

## Attribution

`find_lean4_lsp` in `src/lib.rs`, `snippets/lean.json` and tree-sitter queries in `languages\lean4` are derived from [owlx56/zed-lean4](https://github.com/owlx56/zed-lean4/), licensed under Apache-2.0. Modified for use in this project.
