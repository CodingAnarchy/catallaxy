// Grammar tests, run with the same TextMate engine VS Code uses (vscode-textmate + oniguruma):
//
// 1. Every .cx file in the repository's examples and conformance suite tokenizes without leaving
//    a rule open at the end of the file (an unbalanced string, interpolation or brace) and without
//    `invalid` tokens.
// 2. Specific tokens get specific scopes (keyword method names, operator methods, format specs...).
// 3. Snapshots of a few files catch unintended changes: `node test/grammar.test.js --update`
//    rewrites them after a deliberate change.
"use strict";

const fs = require("fs");
const path = require("path");
const assert = require("assert");
const vsctm = require("vscode-textmate");
const oniguruma = require("vscode-oniguruma");

const root = path.resolve(__dirname, "../../..");
const update = process.argv.includes("--update");

async function loadGrammar() {
  const wasm = fs.readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm"));
  await oniguruma.loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));
  const registry = new vsctm.Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (p) => new oniguruma.OnigScanner(p),
      createOnigString: (s) => new oniguruma.OnigString(s),
    }),
    loadGrammar: async (scope) => {
      if (scope !== "source.catallaxy") return null;
      const file = path.join(__dirname, "../syntaxes/catallaxy.tmLanguage.json");
      return vsctm.parseRawGrammar(fs.readFileSync(file, "utf8"), file);
    },
  });
  return registry.loadGrammar("source.catallaxy");
}

/** Tokens of a whole file: [{line, start, text, scopes}], plus the rule stack after the last line. */
function tokenize(grammar, text) {
  let stack = vsctm.INITIAL;
  const out = [];
  const lines = text.split(/\r?\n/);
  lines.forEach((line, i) => {
    const r = grammar.tokenizeLine(line, stack);
    for (const t of r.tokens) {
      out.push({ line: i + 1, start: t.startIndex, text: line.slice(t.startIndex, t.endIndex), scopes: t.scopes });
    }
    stack = r.ruleStack;
  });
  return { tokens: out, stack };
}

function cxFiles(dir) {
  const out = [];
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...cxFiles(p));
    else if (e.name.endsWith(".cx")) out.push(p);
  }
  return out;
}

let failures = 0;
function check(name, fn) {
  try {
    fn();
  } catch (e) {
    failures++;
    console.error(`FAIL ${name}\n  ${e.message.split("\n").join("\n  ")}`);
  }
}

/** The innermost scope of the token covering `needle` (the `nth` occurrence) on a line of `src`. */
function scopeAt(grammar, src, needle, nth = 0) {
  const { tokens } = tokenize(grammar, src);
  let from = -1;
  for (let k = 0; k <= nth; k++) from = src.indexOf(needle, from + 1);
  assert(from >= 0, `\`${needle}\` not in \`${src}\``);
  const t = tokens.find((t) => t.line === 1 && t.start <= from && from < t.start + t.text.length);
  return t.scopes;
}

function expectScope(grammar, src, needle, scope, nth = 0) {
  check(`${src} :: ${needle}`, () => {
    const scopes = scopeAt(grammar, src, needle, nth);
    assert(
      scopes.some((s) => s.startsWith(scope)),
      `\`${needle}\` in \`${src}\` has scopes ${scopes.join(" ")}, expected ${scope}`,
    );
  });
}

function expectNotScope(grammar, src, needle, scope, nth = 0) {
  check(`${src} :: ${needle} (not ${scope})`, () => {
    const scopes = scopeAt(grammar, src, needle, nth);
    assert(!scopes.some((s) => s.startsWith(scope)), `\`${needle}\` in \`${src}\` has ${scopes.join(" ")}`);
  });
}

function snapshot(grammar, file) {
  const { tokens } = tokenize(grammar, fs.readFileSync(file, "utf8"));
  return (
    tokens
      .filter((t) => t.text.trim() !== "")
      .map((t) => `${t.line}:${t.start} ${JSON.stringify(t.text)} ${t.scopes.slice(1).join(" ")}`)
      .join("\n") + "\n"
  );
}

async function main() {
  const grammar = await loadGrammar();

  // 1. Balanced and valid over every program in the repository.
  const files = [
    ...cxFiles(path.join(root, "examples")),
    ...cxFiles(path.join(root, "tests/conformance")),
    ...cxFiles(path.join(__dirname, "fixtures")),
  ];
  assert(files.length > 50, `only ${files.length} .cx files found under ${root}`);
  for (const f of files) {
    const rel = path.relative(root, f);
    check(`balanced: ${rel}`, () => {
      const { tokens, stack } = tokenize(grammar, fs.readFileSync(f, "utf8"));
      const open = tokens.length ? tokens[tokens.length - 1].scopes : [];
      assert(stack.depth === 1 || f.includes(`${path.sep}reject${path.sep}`), `ends inside ${open.join(" ")}`);
      const bad = tokens.find((t) => t.scopes.some((s) => s.startsWith("invalid")));
      assert(!bad || f.includes(`${path.sep}reject${path.sep}`), bad && `invalid token at ${bad.line}:${bad.start} ${JSON.stringify(bad.text)}`);
    });
  }

  // 2. Targeted scopes.
  const g = grammar;
  expectScope(g, "# a comment", "comment", "comment.line.number-sign");
  expectScope(g, "x = 1 # trailing", "trailing", "comment.line");
  expectScope(g, 'puts("a #{b} c")', "b", "meta.interpolation");
  expectScope(g, 'puts("a #{b} c")', "c", "string.quoted.double");
  expectScope(g, 'puts("#{xs.map { |x| x }} tail")', "tail", "string.quoted.double");
  expectScope(g, 'puts("#{xs.map { |x| x }} tail")', "x", "variable.parameter", 1);
  expectScope(g, 'puts("#{",".join(xs)}") # c', "c", "comment.line");
  expectScope(g, 'puts("#{x:>8.2f}")', ">8.2f", "constant.other.format-spec");
  expectScope(g, 'puts("#{x:08d} y")', "y", "string.quoted.double");
  expectNotScope(g, 'puts("#{f(k: 1)}")', "1", "constant.other.format-spec");
  expectNotScope(g, 'puts("#{[a: 1]}")', "1", "constant.other.format-spec");
  expectScope(g, "'raw #{no} \\' still'", "no", "string.quoted.single");
  expectNotScope(g, "'raw #{no}'", "no", "meta.interpolation");
  expectScope(g, "'raw \\' still' + x", "x", "source.catallaxy");
  expectNotScope(g, "'raw \\' still' + x", "x", "string");
  expectScope(g, '"esc \\n \\u{1F600} \\#"', "\\n", "constant.character.escape");
  expectScope(g, '"esc \\n \\u{1F600} \\#"', "\\u{1F600}", "constant.character.escape");
  expectScope(g, "fn add(a: Int, b) -> Int { a + b }", "add", "entity.name.function");
  expectScope(g, "fn add(a: Int, b) -> Int { a + b }", "a:", "variable.parameter");
  expectScope(g, "fn add(a: Int, b) -> Int { a + b }", "b", "variable.parameter");
  expectScope(g, "fn add(a: Int, b) -> Int { a + b }", "Int", "support.type.builtin");
  expectScope(g, "fn add(a: Int, b) -> Int { a + b }", "->", "keyword.operator.arrow");
  expectScope(g, "  fn match(self, p: String) -> Bool { true }", "match", "entity.name.function");
  expectScope(g, "  fn +(self, o: Vec) -> Vec { o }", "+", "entity.name.function");
  expectScope(g, "  fn []=(self, i: Int, v: T) { }", "[]=", "entity.name.function");
  expectScope(g, "  fn <=>(self, o: Self) -> Int { 0 }", "<=>", "entity.name.function");
  expectScope(g, "fn max[T: Comparable](xs: List[T]) -> T? { nil }", "Comparable", "entity.name.type");
  expectScope(g, 'fn greet(name: String, greeting: String = "Hi") { }', "greeting", "variable.parameter");
  expectScope(g, "sq = fn(x: Int) -> Int { x * x }", "x", "variable.parameter");
  expectScope(g, "class Square < Rect {", "Square", "entity.name.type.class");
  expectScope(g, "class Square < Rect {", "Rect", "entity.other.inherited-class");
  expectScope(g, "frozen class Item {", "frozen", "storage.modifier");
  expectScope(g, "shared class C {", "shared", "storage.modifier");
  expectScope(g, "interface Shape {", "Shape", "entity.name.type.interface");
  expectScope(g, "enum Shape[T] {", "Shape", "entity.name.type.enum");
  expectScope(g, "extend String {", "String", "entity.name.type");
  expectScope(g, "pub native fn f(x: Int) -> Int", "native", "storage.modifier");
  expectScope(g, "import lib.geometry.{Point, area}", "import", "keyword.control.import");
  expectScope(g, "import lib.geometry.{Point, area}", "Point", "entity.name.type");
  expectScope(g, "xs.map { |x, y: Int| x * 2 }", "y", "variable.parameter");
  expectScope(g, "xs.each { puts(it) }", "it", "variable.language");
  expectScope(g, "if x.match(y) { }", "match", "entity.name.function.member");
  expectNotScope(g, "if x.match(y) { }", "match", "keyword");
  expectScope(g, "t = x.spawn", "spawn", "variable.other.property");
  expectScope(g, "for x in 1..10 { }", "..", "keyword.operator.range");
  expectScope(g, "for x in 1..10 { }", "10", "constant.numeric.integer");
  expectScope(g, "for x in 1..10 { }", "in", "keyword.control");
  expectScope(g, "y = 3.times", "times", "variable.other.property");
  expectScope(g, "y = 3.times", "3", "constant.numeric.integer");
  expectScope(g, "y = 2.5e-3 + 1_000.5", "2.5e-3", "constant.numeric.float");
  expectScope(g, "y = 2.5e-3 + 1_000.5", "1_000.5", "constant.numeric.float");
  expectScope(g, "y = 0xff + 0b1010", "0xff", "constant.numeric.integer.hexadecimal");
  expectScope(g, "x = a ?? b", "??", "keyword.operator.nil-coalescing");
  expectScope(g, "x = a?.b", "b", "variable.other.property");
  expectScope(g, "r = f()?", "?", "keyword.operator.try");
  expectScope(g, "x = a as? String", "as", "keyword.operator.word");
  expectScope(g, "t = spawn move { 1 }", "spawn", "keyword.control");
  expectScope(g, "t = spawn move { 1 }", "move", "storage.modifier");
  expectScope(g, "x = nil", "nil", "constant.language");
  expectScope(g, "const MAX_ITER = 10", "MAX_ITER", "variable.other.constant");
  expectScope(g, "s = Shape.Circle(2.0)", "Circle", "variable.other.enummember");
  expectScope(g, "m = match s { .Circle(r) => r }", "=>", "keyword.operator.arrow.match");
  expectScope(g, "r = Ok(1)", "Ok", "support.function.result");
  expectScope(g, "self.x = 1", "self", "variable.language.self");

  // 3. Snapshots, of fixtures kept here (a copy of examples/shapes.cx among them), so editing
  // an example does not break this test.
  const snapDir = path.join(__dirname, "snapshots");
  const snapped = [path.join(__dirname, "fixtures/syntax.cx"), path.join(__dirname, "fixtures/shapes.cx")];
  fs.mkdirSync(snapDir, { recursive: true });
  for (const f of snapped) {
    const snapFile = path.join(snapDir, path.basename(f) + ".snap");
    const got = snapshot(grammar, f);
    if (update || !fs.existsSync(snapFile)) {
      fs.writeFileSync(snapFile, got);
      console.log(`wrote ${path.relative(process.cwd(), snapFile)}`);
      continue;
    }
    check(`snapshot ${path.basename(snapFile)}`, () => {
      const want = fs.readFileSync(snapFile, "utf8");
      if (got === want) return;
      const g = got.split("\n");
      const w = want.split("\n");
      const i = g.findIndex((l, k) => l !== w[k]);
      assert.fail(`differs at token ${i + 1}:\n  want ${w[i]}\n  got  ${g[i]}\n(run \`npm run snapshots\` to accept)`);
    });
  }

  if (failures) {
    console.error(`${failures} grammar check(s) failed`);
    process.exit(1);
  }
  console.log(`grammar ok: ${files.length} files balanced, snapshots match`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
