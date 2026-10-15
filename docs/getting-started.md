# Getting started with Catallaxy

This guide takes you from installing `catx` to a small project with two files and tests, and an
editor that knows about the language. For the language itself, see the [reference](reference.md)
and the [standard library](stdlib.md).

## Install

On Linux or macOS, the install script fetches a prebuilt binary (releases from 0.4.0 on) and
checks its SHA-256:

```sh
curl -fsSL https://raw.githubusercontent.com/CodingAnarchy/catallaxy/main/install.sh | sh
catx version                   # catx is now in ~/.local/bin
```

The [README](../README.md#install) lists the script's options and the Windows download.

Catallaxy is written in Rust, so with a Rust toolchain (from [rustup.rs](https://rustup.rs)) you can
also install it from crates.io:

```sh
cargo install catallaxy        # installs the `catx` binary into ~/.cargo/bin
```

Or build from a checkout:

```sh
git clone https://github.com/CodingAnarchy/catallaxy
cd catallaxy
cargo build --release          # the binary is target/release/catx
```

`catx` runs on Linux, macOS and Windows.

## Hello, world

Save this as `hello.cx`:

```
puts("Hello, world!")
```

and run it:

```sh
catx run hello.cx              # or just: catx hello.cx
```

A file is a program: its top-level statements run in order, so no `main` function is needed.
`catx run` type-checks the whole program before running any of it. To check without running, use
`catx check hello.cx`. Errors come with a code, the place in the source, and often a suggested fix.
For a file containing `n: Int = "three"`:

```
error[E0300]: mismatched types in assignment: expected `Int`, found `String`
  --> hello.cx:1:10
    |
  1 | n: Int = "three"
    |          ^^^^^^^
1 error found
```

`catx repl` starts an interactive session, which is handy for trying out expressions
(`:type expr` shows a type, `:quit` leaves).

## A project with two files

A module is a file, and `import` names other files relative to the importing file. A
small project might look like this:

```
greet/
  main.cx
  lib/
    greeting.cx
```

`lib/greeting.cx` holds the reusable part. Only `pub` items can be seen from other modules, and
their parameter and result types must be written out:

```
pub fn greet(name: String, greeting: String = "Hello") -> String {
  "#{greeting}, #{name}!"
}

pub fn initials(name: String) -> String {
  name.split(" ").map { |w| w[0..0].upcase }.join("")
}
```

`main.cx` imports it, either the whole module (used as `greeting.greet(...)`) or selected items:

```
import lib.greeting
import lib.greeting.{initials}

names = args()
if names.is_empty() { names = ["Ada Lovelace", "Grace Hopper"] }
for name in names {
  puts(greeting.greet(name))
  puts(greeting.greet(name, greeting: "Goodbye"))
  puts(initials(name))
}
```

```sh
$ catx run main.cx "Alan Turing"
Hello, Alan Turing!
Goodbye, Alan Turing!
AT
```

Arguments after the file name are available as `args()`. The standard library (`File`, `Json`,
`Regex`, `Process`, `Http`, ...) is always in scope, so it needs no import.

`catx check` and `catx fmt` take directories as well as files. `catx check .` type-checks every
`.cx` file under the current directory, and `catx fmt .` formats them in place (`catx fmt --check .`
only lists the files that would change, which is useful in CI). `catx fix .` applies the fixes the
checker is sure of, such as adding `.new` to `Point(1)`. Add `--dry-run` to only list them. While
walking a directory, all three leave out tests that are meant to fail to compile (files starting
with `# expect:`, see below, and `reject/` directories). Every command explains itself with
`--help`. They exit with status 0 when all is well, 1 when they found problems, and 2 when they
could not run (a mistyped option or path); the reference, §10, has the table.

## Testing

`catx test dir` runs every `.cx` file under `dir` as a separate program, skipping `lib/`
directories, where modules live. It also takes files and several paths (`catx test a_test.cx b/`),
and with no path it runs the current directory. A path that does not exist, or a run that finds
no tests, is an error rather than a silent `0 passed`. There are two kinds of test:

- A program with a `.out` file next to it must print exactly that to standard output, and exit
  with status 0. A `# exit: N` line expects status `N` instead, and a `.in` file next to the
  program is fed to its standard input.
- A program whose first line is `# expect: CODE` must fail `catx check` with that diagnostic code.
  This pins down what the checker must reject.

For the project above, add `greeting_test.cx`:

```
import lib.greeting.{greet, initials}

assert(greet("Ann") == "Hello, Ann!", "default greeting")
puts(greet("Ann", greeting: "Hi"))
puts(initials("Barbara Liskov"))
```

with `greeting_test.out`:

```
Hi, Ann!
BL
```

and `misuse_test.cx`, which checks that a misspelled named argument is rejected:

```
# expect: E0336
import lib.greeting.{greet}
greet("Ann", salutation: "Hi")
```

`main.cx` is a test too, so it needs a `main.out` with the output of a run without arguments.
`catx run main.cx > main.out` creates one; check that the output is right before you rely on it.
Then:

```sh
$ catx test .
3 passed, 0 failed
```

A failing test prints a diff of the expected and actual output, with invisible characters such
as trailing spaces made visible. `assert` and `panic` make a program exit
with status 101, which fails the test. Each program gets 120 seconds (`CATX_TEST_TIMEOUT`
changes this). `catx test -j 8` runs eight tests at a time, `--filter greeting` runs only the
tests whose path contains `greeting`, `-v` lists every test with its time, and `--json` prints a
machine-readable report. The repository's own `examples/` and `tests/conformance/` directories use this
layout.

Line endings do not matter to `catx test`, which reads `\r\n` as `\n`, or to `catx fmt`, which
keeps each file's own line endings. On Windows, where Git may check text files out with `\r\n`
(`core.autocrlf`), a `.gitattributes` file at the root of the repository keeps sources and
expected outputs the same on every platform:

```
*.cx text eol=lf
*.out text eol=lf
*.in text eol=lf
```

## Editor setup

`catx lsp` is a language server that talks over stdin and stdout. It provides diagnostics as you
type, hover, go to definition, completion, find references, rename, signature help, inlay hints,
document and workspace symbols, formatting, and quick fixes for the checker's suggestions. Any
editor with an LSP client can use it. Configure the client to run `catx lsp` for `*.cx` files, with
the language id `catallaxy`.

Neovim (0.11 or later), in `init.lua`:

```lua
vim.filetype.add({ extension = { cx = "catallaxy" } })
vim.lsp.config("catx", { cmd = { "catx", "lsp" }, filetypes = { "catallaxy" }, root_markers = { ".git" } })
vim.lsp.enable("catx")
```

Helix, in `languages.toml`:

```toml
[language-server.catx]
command = "catx"
args = ["lsp"]

[[language]]
name = "catallaxy"
scope = "source.catallaxy"
file-types = ["cx"]
comment-token = "#"
indent = { tab-width = 2, unit = "  " }
language-servers = ["catx"]
```

Emacs (29 or later, with Eglot):

```elisp
(define-derived-mode catallaxy-mode prog-mode "Catallaxy"
  (setq-local comment-start "# "))
(add-to-list 'auto-mode-alist '("\\.cx\\'" . catallaxy-mode))
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(catallaxy-mode "catx" "lsp")))
```

VS Code has an extension in [`editors/vscode`](../editors/vscode) with syntax highlighting that
starts `catx lsp` for you. It is not on the Marketplace yet; build and install it from a checkout:

```sh
cd editors/vscode
npm ci
npm run package                # writes catallaxy.vsix
code --install-extension catallaxy.vsix
```

It runs the `catx` on your `PATH`; the `catallaxy.serverPath` setting points it at another binary.

## Next steps

- Read the programs in [`examples/`](../examples), each with its expected output.
- Skim the [reference](reference.md): §4 covers expressions, §5 classes and enums, §8 threads,
  channels and fibers.
- `catx check --json` gives machine-readable diagnostics, with fixes, for scripts and tools.
