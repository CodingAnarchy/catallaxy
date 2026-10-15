# Catallaxy for VS Code

Syntax highlighting for Catallaxy (`.cx`) files, plus the `catx lsp` language server:

- diagnostics as you type
- hover: types (including pattern bindings and block parameters), signatures with default values, and documentation comments
- go to definition, and find references and rename across every `.cx` file in the workspace folders, open or not
- completion, with documentation comments
- signature help
- inlay hints with inferred types
- document symbols, and workspace symbols over the workspace folders
- formatting (`catx fmt`)

## Install

It is not on the Marketplace yet. Build it and install the `.vsix`:

```sh
cd editors/vscode
npm ci
npm run package                         # writes catallaxy.vsix
code --install-extension catallaxy.vsix
```

The extension needs `catx` (see the [main README](../../README.md#install)). It runs `catx lsp`
from `PATH`. To use another binary, set **`catallaxy.serverPath`**, for example
`"catallaxy.serverPath": "~/src/catallaxy/target/release/catx"`. If you change the setting, or
rebuild `catx` and run **Catallaxy: Restart Language Server**, the server restarts.
`catallaxy.trace.server` (`messages` or `verbose`) logs the protocol traffic to the **Catallaxy**
output channel.

When it starts, and when a workspace folder is added, the server checks the workspace's `.cx` files
in the background so that references, rename and workspace symbols answer quickly. VS Code shows
the progress in the status bar, and the time taken goes to the **Catallaxy** output channel.

To work on the extension itself, open this folder in VS Code and press F5. That starts an
Extension Development Host with the extension loaded.

## Layout

| File | |
| --- | --- |
| `syntaxes/catallaxy.tmLanguage.json` | TextMate grammar (`source.catallaxy`) |
| `language-configuration.json` | comments, brackets, auto-closing pairs, indentation |
| `extension.js` | starts the language client (plain JavaScript, no build step) |
| `test/grammar.test.js` | grammar tests (see below) |
| `test/lsp.test.js` | `catx lsp` driven over stdio, as an editor would |
| `test/vscode/` | the extension running in a real VS Code |

## Tests

Build `catx` first (`cargo build --release` at the repository root). The tests use
`target/release/catx`; set `CATX` to use another binary.

```sh
npm run test:grammar   # every .cx file in examples/ and tests/conformance/ tokenizes with no rule left open
                       # and no invalid tokens; targeted scope checks; snapshots of a few files
npm run test:lsp       # initialize, diagnostics, hover, definition, completion, references,
                       # rename, signature help, inlay hints, symbols, formatting, shutdown
npm run test:vscode    # downloads VS Code and runs the extension in it (Linux: use xvfb-run -a)
```

After a deliberate grammar change, `npm run snapshots` rewrites `test/snapshots/`.

The grammar uses the same engine as VS Code (`vscode-textmate` with `vscode-oniguruma`). It
accepts an optional format spec at the end of an interpolation (`"#{x:>8.2f}"`) and scopes it
`constant.other.format-spec`. A `:` inside brackets or parentheses, as in `#{f(k: 1)}` or
`#{[a: 1]}`, is not treated as a format spec.
