// End-to-end test of `catx lsp` over stdio, driving it the way VS Code does: initialize, open
// documents, then every request the server advertises, checking each answer is sane.
//
//   CATX=path/to/catx node test/lsp.test.js     (default: target/release/catx in the repository)
"use strict";

const { spawn } = require("child_process");
const fs = require("fs");
const os = require("os");
const path = require("path");
const assert = require("assert");
const { pathToFileURL } = require("url");

const root = path.resolve(__dirname, "../../..");
const exe = path.join(root, "target/release/catx" + (process.platform === "win32" ? ".exe" : ""));
const catx = process.env.CATX || exe;

// ---- a minimal JSON-RPC client --------------------------------------------------------------

class Client {
  constructor(command, env = {}) {
    this.proc = spawn(command, ["lsp"], { stdio: ["pipe", "pipe", "inherit"], env: { ...process.env, ...env } });
    this.buf = Buffer.alloc(0);
    this.nextId = 1;
    this.pending = new Map();
    this.notifications = [];
    this.waiters = [];
    this.proc.stdout.on("data", (d) => this.onData(d));
    this.exited = new Promise((resolve) => this.proc.on("exit", (code) => resolve(code)));
  }

  onData(d) {
    this.buf = Buffer.concat([this.buf, d]);
    for (;;) {
      const sep = this.buf.indexOf("\r\n\r\n");
      if (sep < 0) return;
      const header = this.buf.slice(0, sep).toString();
      const m = /Content-Length: (\d+)/i.exec(header);
      if (!m) throw new Error(`bad header: ${header}`);
      const len = Number(m[1]);
      if (this.buf.length < sep + 4 + len) return;
      const msg = JSON.parse(this.buf.slice(sep + 4, sep + 4 + len).toString("utf8"));
      this.buf = this.buf.slice(sep + 4 + len);
      this.dispatch(msg);
    }
  }

  dispatch(msg) {
    if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      const p = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      if (msg.error) p.reject(Object.assign(new Error(msg.error.message), { code: msg.error.code }));
      else p.resolve(msg.result);
    } else if (msg.method) {
      this.notifications.push(msg);
      this.waiters = this.waiters.filter((w) => !w(msg));
    }
  }

  send(msg) {
    const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", ...msg }), "utf8");
    this.proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.proc.stdin.write(body);
  }

  request(method, params, timeoutMs = 30000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${method}: no answer in ${timeoutMs} ms`)), timeoutMs);
      this.pending.set(id, {
        resolve: (v) => (clearTimeout(timer), resolve(v)),
        reject: (e) => (clearTimeout(timer), reject(e)),
      });
      this.send({ id, method, params });
    });
  }

  notify(method, params) {
    this.send({ method, params });
  }

  /** Waits for a notification matching `pred` (including one already received). */
  waitFor(pred, timeoutMs = 30000) {
    const seen = this.notifications.find(pred);
    if (seen) return Promise.resolve(seen);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("timed out waiting for a notification")), timeoutMs);
      this.waiters.push((msg) => {
        if (!pred(msg)) return false;
        clearTimeout(timer);
        resolve(msg);
        return true;
      });
    });
  }
}

// ---- helpers --------------------------------------------------------------------------------

const uriOf = (file) => pathToFileURL(file).href;

/** LSP position (UTF-16 columns) of the `nth` occurrence of `needle` in `text`, plus `delta` chars. */
function pos(text, needle, nth = 0, delta = 0) {
  let at = -1;
  for (let k = 0; k <= nth; k++) {
    at = text.indexOf(needle, at + 1);
    assert(at >= 0, `\`${needle}\` (#${nth}) not found`);
  }
  at += delta;
  const before = text.slice(0, at);
  const line = before.split("\n").length - 1;
  return { line, character: at - (before.lastIndexOf("\n") + 1) };
}

/** The text a range covers. */
function slice(text, range) {
  const lines = text.split("\n");
  if (range.start.line === range.end.line) {
    return lines[range.start.line].slice(range.start.character, range.end.character);
  }
  return [
    lines[range.start.line].slice(range.start.character),
    ...lines.slice(range.start.line + 1, range.end.line),
    lines[range.end.line].slice(0, range.end.character),
  ].join("\n");
}

let failures = 0;
let passed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
    console.log(`ok   ${name}`);
  } catch (e) {
    failures++;
    console.error(`FAIL ${name}\n  ${(e && e.stack) || e}`);
  }
}

// ---- the test -------------------------------------------------------------------------------

async function main() {
  assert(fs.existsSync(catx), `${catx} not found: build it with \`cargo build --release\` or set CATX`);
  // Work on copies, so nothing in the repository is touched and the tests own their inputs.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "catx-lsp-"));
  for (const f of ["shapes.cx", "modules_demo.cx", "lib/geometry.cx", "lib/textutil.cx"]) {
    fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
    fs.copyFileSync(path.join(root, "examples", f), path.join(dir, f));
  }
  const shapesPath = path.join(dir, "shapes.cx");
  const shapes = fs.readFileSync(shapesPath, "utf8");
  const shapesUri = uriOf(shapesPath);
  const demoPath = path.join(dir, "modules_demo.cx");
  const demo = fs.readFileSync(demoPath, "utf8");
  const demoUri = uriOf(demoPath);
  const geometryUri = uriOf(path.join(dir, "lib/geometry.cx"));
  // A broken file that is never opened: the workspace's diagnostics report it.
  fs.writeFileSync(path.join(dir, "unopened_broken.cx"), 'zz: Int = "not an int"\n');
  const brokenUri = uriOf(path.join(dir, "unopened_broken.cx"));
  const doc = (uri) => ({ textDocument: { uri } });
  const at = (uri, text, needle, nth = 0, delta = 0) => ({ ...doc(uri), position: pos(text, needle, nth, delta) });

  // The prelude's sources are written under the cache directory for go to definition.
  const cacheHome = fs.mkdtempSync(path.join(os.tmpdir(), "catx-cache-"));
  const c = new Client(catx, { XDG_CACHE_HOME: cacheHome });
  try {
    const init = await c.request("initialize", {
      processId: process.pid,
      rootUri: uriOf(dir),
      capabilities: {
        textDocument: {
          hover: { contentFormat: ["markdown", "plaintext"] },
          rename: { prepareSupport: true },
          documentSymbol: { hierarchicalDocumentSymbolSupport: true },
        },
      },
      workspaceFolders: [{ uri: uriOf(dir), name: "examples" }],
    });
    c.notify("initialized", {});

    await check("initialize advertises the features", async () => {
      const caps = init.capabilities;
      for (const k of [
        "hoverProvider",
        "definitionProvider",
        "completionProvider",
        "documentFormattingProvider",
        "referencesProvider",
        "renameProvider",
        "signatureHelpProvider",
        "inlayHintProvider",
        "documentSymbolProvider",
        "workspaceSymbolProvider",
        "documentHighlightProvider",
        "foldingRangeProvider",
        "semanticTokensProvider",
        "documentRangeFormattingProvider",
      ]) {
        assert(caps[k], `missing ${k}`);
      }
      assert.strictEqual(init.serverInfo.name, "catx");
    });

    c.notify("textDocument/didOpen", { textDocument: { uri: shapesUri, languageId: "catallaxy", version: 1, text: shapes } });
    c.notify("textDocument/didOpen", { textDocument: { uri: demoUri, languageId: "catallaxy", version: 1, text: demo } });

    await check("diagnostics: a clean file publishes none", async () => {
      const n = await c.waitFor((m) => m.method === "textDocument/publishDiagnostics" && m.params.uri === shapesUri);
      assert.deepStrictEqual(n.params.diagnostics, []);
    });

    // The server publishes diagnostics synchronously while handling a change, so once a later
    // request is answered every notification for the change has arrived.
    const settle = () => c.request("textDocument/documentSymbol", doc(shapesUri));
    const diagnosticsAfter = async (uri, text, version) => {
      await settle();
      c.notifications = [];
      c.notify("textDocument/didChange", { textDocument: { uri, version }, contentChanges: [{ text }] });
      await settle();
      const ns = c.notifications.filter((m) => m.method === "textDocument/publishDiagnostics" && m.params.uri === uri);
      assert(ns.length > 0, "no diagnostics published");
      return ns[ns.length - 1].params.diagnostics;
    };

    await check("diagnostics: a workspace file that is not open gets them too", async () => {
      const n = await c.waitFor(
        (m) => m.method === "textDocument/publishDiagnostics" && m.params.uri === brokenUri && m.params.diagnostics.length > 0,
      );
      assert.strictEqual(n.params.diagnostics[0].code, "E0300");
    });

    await check("diagnostics: an edit with a type error publishes it, and fixing it clears it", async () => {
      const broken = shapes + '\nzz: Int = "not an int"\n';
      const ds = await diagnosticsAfter(shapesUri, broken, 2);
      assert.strictEqual(ds.length, 1, JSON.stringify(ds));
      assert.strictEqual(ds[0].severity, 1);
      assert.strictEqual(ds[0].code, "E0300");
      assert.strictEqual(slice(broken, ds[0].range), '"not an int"');
      assert.deepStrictEqual(await diagnosticsAfter(shapesUri, shapes, 3), []);
    });

    await check("hover: a local shows its type", async () => {
      const h = await c.request("textDocument/hover", at(shapesUri, shapes, "by_area =", 0));
      assert(h && h.contents && h.contents.value, JSON.stringify(h));
      assert.match(h.contents.value, /List\[Shape\]/);
      assert.strictEqual(slice(shapes, h.range), "by_area");
    });

    await check("hover: pattern bindings and block parameters show their own type", async () => {
      for (const [needle, delta, want] of [
        ["for s in shapes", 4, /s: Shape/],
        ["if let big", 7, /big: ByArea/],
        ["{ |acc, s|", 3, /acc: Float/],
      ]) {
        const h = await c.request("textDocument/hover", at(shapesUri, shapes, needle, 0, delta));
        assert(h && h.contents, `${needle}: ${JSON.stringify(h)}`);
        assert.match(h.contents.value, want);
        assert.strictEqual(slice(shapes, h.range), want.source.split(":")[0]);
      }
    });

    await check("hover and completion: documentation comments", async () => {
      // A user class's leading comment, and a prelude method's.
      const h = await c.request("textDocument/hover", at(shapesUri, shapes, "ByArea.new(s)", 0, 1));
      assert.match(h.contents.value, /class ByArea/);
      assert.match(h.contents.value, /A wrapper that orders shapes by area/);
      const s = await c.request("textDocument/hover", at(shapesUri, shapes, "sort_with", 0, 1));
      assert.match(s.contents.value, /A stable sort by the comparator/);
      const r = await c.request("textDocument/completion", at(shapesUri, shapes, "puts(wrapped[1]", 0, 5));
      const item = (Array.isArray(r) ? r : r.items).find((i) => i.label === "ByArea");
      assert(item && item.documentation, JSON.stringify(item));
      assert.strictEqual(item.documentation.kind, "markdown");
      assert.match(item.documentation.value, /orders shapes by area/);
    });

    await check("hover: a method call shows its signature", async () => {
      const h = await c.request("textDocument/hover", at(shapesUri, shapes, "describe()", 0, 1));
      assert(h, "no hover");
      assert.match(h.contents.value, /describe/);
      assert.match(h.contents.value, /String/);
    });

    await check("definition: a class name goes to its declaration", async () => {
      const d = await c.request("textDocument/definition", at(shapesUri, shapes, "Square.new", 0, 2));
      const loc = Array.isArray(d) ? d[0] : d;
      assert(loc, "no definition");
      assert.strictEqual(loc.uri, shapesUri);
      assert.strictEqual(loc.range.start.line, pos(shapes, "class Square").line);
    });

    await check("definition: across modules (an imported function)", async () => {
      const d = await c.request("textDocument/definition", at(demoUri, demo, "geometry.area(square)", 0, "geometry.".length));
      const loc = Array.isArray(d) ? d[0] : d;
      assert(loc, "no definition");
      assert.strictEqual(loc.uri, geometryUri);
      const geo = fs.readFileSync(path.join(dir, "lib/geometry.cx"), "utf8");
      assert.match(geo.split("\n")[loc.range.start.line], /fn area/);
    });

    await check("definition: a prelude function goes to a read-only copy of the prelude", async () => {
      const d = await c.request("textDocument/definition", at(shapesUri, shapes, "puts(", 0));
      const loc = Array.isArray(d) ? d[0] : d;
      assert(loc, "no definition");
      const file = require("url").fileURLToPath(loc.uri);
      assert(fs.realpathSync(file).startsWith(fs.realpathSync(cacheHome)), file);
      assert.match(fs.readFileSync(file, "utf8").split("\n")[loc.range.start.line], /fn puts/);
      assert.throws(() => fs.accessSync(file, fs.constants.W_OK));
    });

    await check("completion: members after `.`", async () => {
      // `xs.` where xs is a List: insert a probe line, ask at its end.
      const probe = shapes + "\nshapes.\n";
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 4 }, contentChanges: [{ text: probe }] });
      const line = probe.split("\n").length - 2;
      const r = await c.request("textDocument/completion", { ...doc(shapesUri), position: { line, character: "shapes.".length } });
      const items = Array.isArray(r) ? r : r.items;
      const labels = items.map((i) => i.label);
      for (const m of ["map", "length", "push"]) assert(labels.includes(m), `no \`${m}\` in ${labels.slice(0, 20)}...`);
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 5 }, contentChanges: [{ text: shapes }] });
    });

    await check("completion: names in scope", async () => {
      const r = await c.request("textDocument/completion", at(shapesUri, shapes, "puts(wrapped[1]", 0, 5));
      const labels = (Array.isArray(r) ? r : r.items).map((i) => i.label);
      for (const m of ["wrapped", "largest", "total_area", "Circle"]) assert(labels.includes(m), `no \`${m}\``);
    });

    await check("references: a function, with and without its declaration", async () => {
      const r = await c.request("textDocument/references", {
        ...at(shapesUri, shapes, "fn total_area", 0, 3),
        context: { includeDeclaration: true },
      });
      assert(Array.isArray(r) && r.length === 2, JSON.stringify(r));
      for (const loc of r) assert.strictEqual(slice(shapes, loc.range), "total_area");
      const r2 = await c.request("textDocument/references", {
        ...at(shapesUri, shapes, "fn total_area", 0, 3),
        context: { includeDeclaration: false },
      });
      assert.strictEqual(r2.length, 1);
    });

    await check("references: a field", async () => {
      const r = await c.request("textDocument/references", {
        ...at(shapesUri, shapes, "shape: Shape"),
        context: { includeDeclaration: true },
      });
      // The declaration, two uses in `<=>`, and three `.shape` reads at the end.
      assert(r.length >= 5, JSON.stringify(r));
      for (const loc of r) assert.strictEqual(slice(shapes, loc.range), "shape");
    });

    await check("prepareRename: a local gives its range; a prelude method is refused", async () => {
      const p = await c.request("textDocument/prepareRename", at(shapesUri, shapes, "by_perimeter =", 0, 1));
      const range = p.range || p;
      assert.strictEqual(slice(shapes, range), "by_perimeter");
      let refused = false;
      try {
        const q = await c.request("textDocument/prepareRename", at(shapesUri, shapes, "sort_with", 0, 1));
        refused = q === null;
      } catch (e) {
        refused = true;
      }
      assert(refused, "renaming `sort_with` (prelude) was not refused");
    });

    await check("references and rename: an interface member, its implementations and the calls", async () => {
      // Classes satisfy interfaces structurally. The ones used as a Shape (the `shapes` list)
      // implement the member, so their methods go with it: the declaration, the calls on
      // Shape-typed values (`self` in a default method, the block parameters of
      // `shapes.sort_with`), the classes' methods, and the call on a Triangle's `self`.
      const want = [
        pos(shapes, "perimeter(self) -> Float\n"),
        pos(shapes, "self.perimeter()", 0, 5),
        pos(shapes, "b.perimeter()", 0, 2),
        pos(shapes, "a.perimeter()", 0, 2),
        pos(shapes, "perimeter(self) -> Float { 2.0 * Math"),
        pos(shapes, "perimeter(self) -> Float { 2.0 * (self"),
        pos(shapes, "perimeter(self) -> Float { self.a"),
        pos(shapes, "self.perimeter() / 2.0", 0, 5),
      ].map((p) => `${p.line}:${p.character}`);
      const refs = await c.request("textDocument/references", {
        ...at(shapesUri, shapes, "b.perimeter()", 0, 2),
        context: { includeDeclaration: true },
      });
      assert.deepStrictEqual(refs.map((l) => `${l.range.start.line}:${l.range.start.character}`).sort(), want.sort());
      const e = await c.request("textDocument/rename", { ...at(shapesUri, shapes, "fn perimeter", 0, 3), newName: "girth" });
      const edits = e.changes[shapesUri];
      for (const ed of edits) {
        assert.strictEqual(slice(shapes, ed.range), "perimeter");
        assert.strictEqual(ed.newText, "girth");
      }
      assert.deepStrictEqual(edits.map((ed) => `${ed.range.start.line}:${ed.range.start.character}`).sort(), want.sort());
    });

    await check("hover and definition: a call through an interface shows and finds the member", async () => {
      const h = await c.request("textDocument/hover", at(shapesUri, shapes, "b.perimeter()", 0, 2));
      assert.match(h.contents.value, /fn Shape\.perimeter\(self\) -> Float/);
      const d = await c.request("textDocument/definition", at(shapesUri, shapes, "b.perimeter()", 0, 2));
      assert.strictEqual(d.range.start.line, pos(shapes, "perimeter(self) -> Float\n").line);
    });

    await check("rename: a field from its declaration renames its uses", async () => {
      const e = await c.request("textDocument/rename", { ...at(shapesUri, shapes, "shape: Shape"), newName: "inner" });
      const edits = e.changes[shapesUri];
      assert.strictEqual(edits.length, (shapes.match(/\bshape\b/g) || []).length, JSON.stringify(edits));
    });

    await check("rename: an imported function renames across modules", async () => {
      const e = await c.request("textDocument/rename", { ...at(demoUri, demo, "geometry.area(square)", 0, 9), newName: "surface" });
      const changes = e.changes || {};
      assert(changes[geometryUri] && changes[geometryUri].length >= 1, `no edit in geometry.cx: ${JSON.stringify(e)}`);
      assert(changes[demoUri] && changes[demoUri].length >= 2, `edits in modules_demo.cx: ${JSON.stringify(changes[demoUri])}`);
    });

    await check("rename: a keyword is rejected as a new name", async () => {
      let rejected = false;
      try {
        const e = await c.request("textDocument/rename", { ...at(shapesUri, shapes, "by_area =", 0, 1), newName: "match" });
        rejected = e === null;
      } catch (err) {
        rejected = true;
      }
      assert(rejected, "renaming to `match` was accepted");
    });

    await check("rename: a name already taken in scope is refused, naming the clash", async () => {
      let err;
      try {
        await c.request("textDocument/rename", { ...at(shapesUri, shapes, "by_area =", 0, 1), newName: "by_perimeter" });
      } catch (e) {
        err = e;
      }
      assert(err, "renaming `by_area` to `by_perimeter` was accepted");
      assert.strictEqual(err.code, -32803);
      assert.match(err.message, /would conflict with the variable `by_perimeter`/);
      // A method renamed to another method of its class.
      err = undefined;
      try {
        await c.request("textDocument/rename", { ...at(shapesUri, shapes, "fn perimeter", 0, 3), newName: "area" });
      } catch (e) {
        err = e;
      }
      assert(err && err.code === -32803, `renaming \`perimeter\` to \`area\`: ${err}`);
    });

    await check("signatureHelp: active parameter follows the commas", async () => {
      const s = await c.request("textDocument/signatureHelp", at(shapesUri, shapes, "Triangle.new(3.0, 4.0", 0, "Triangle.new(3.0, ".length));
      assert(s && s.signatures.length > 0, JSON.stringify(s));
      const sig = s.signatures[s.activeSignature || 0];
      assert.match(sig.label, /a: Float, b: Float, c: Float/);
      const active = sig.activeParameter ?? s.activeParameter;
      assert.strictEqual(active, 1);
    });

    await check("inlayHint: inferred types of locals and block parameters", async () => {
      const lines = shapes.split("\n").length;
      const hints = await c.request("textDocument/inlayHint", {
        ...doc(shapesUri),
        range: { start: { line: 0, character: 0 }, end: { line: lines, character: 0 } },
      });
      assert(Array.isArray(hints) && hints.length > 0, JSON.stringify(hints));
      const label = (h) => (typeof h.label === "string" ? h.label : h.label.map((p) => p.value).join(""));
      const byLine = (needle) => hints.filter((h) => h.position.line === pos(shapes, needle).line).map(label);
      assert(byLine("by_area =").some((l) => /List\[Shape\]/.test(l)), JSON.stringify(byLine("by_area =")));
      assert(byLine("{ |acc, s|").some((l) => /Float/.test(l)), JSON.stringify(byLine("{ |acc, s|")));
      // Annotated variables get no hint.
      assert.deepStrictEqual(byLine("shapes: List[Shape] ="), []);
      const sameLine = hints.filter((h) => h.position.line === pos(shapes, "by_area =").line)[0];
      assert.strictEqual(sameLine.position.character, "by_area".length);
    });

    await check("documentSymbol: outline of items and members", async () => {
      const syms = await c.request("textDocument/documentSymbol", doc(shapesUri));
      assert(Array.isArray(syms) && syms.length > 0);
      const names = syms.map((s) => s.name);
      for (const n of ["Shape", "Circle", "Square", "ByArea", "largest", "total_area"]) assert(names.includes(n), `no ${n} in ${names}`);
      const circle = syms.find((s) => s.name === "Circle");
      const kids = (circle.children || []).map((s) => s.name);
      for (const n of ["r", "area", "perimeter", "name"]) assert(kids.includes(n), `Circle lacks ${n}: ${kids}`);
      assert.strictEqual(circle.kind, 5); // Class
      assert(slice(shapes, circle.selectionRange) === "Circle");
    });

    await check("workspace/symbol: finds items in open documents and imported modules", async () => {
      const r = await c.request("workspace/symbol", { query: "quadrant" });
      assert(Array.isArray(r) && r.length > 0, JSON.stringify(r));
      assert(r.some((s) => s.location.uri === geometryUri), JSON.stringify(r.map((s) => s.location.uri)));
      const all = await c.request("workspace/symbol", { query: "" });
      assert(all.some((s) => s.name === "Triangle"));
    });

    await check("formatting: a formatted file needs no edits; a messy one is fixed", async () => {
      const none = await c.request("textDocument/formatting", { ...doc(shapesUri), options: { tabSize: 2, insertSpaces: true } });
      assert.deepStrictEqual(none, []);
      const messy = shapes.replace("fn largest[T: Comparable](xs: List[T]) -> T? {", "fn largest[T: Comparable]( xs:List[T] )->T?{");
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 6 }, contentChanges: [{ text: messy }] });
      const edits = await c.request("textDocument/formatting", { ...doc(shapesUri), options: { tabSize: 2, insertSpaces: true } });
      assert.strictEqual(edits.length, 1);
      assert.strictEqual(edits[0].newText, shapes);
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 7 }, contentChanges: [{ text: shapes }] });
    });

    await check("documentHighlight: a function's declaration and uses", async () => {
      const hs = await c.request("textDocument/documentHighlight", at(shapesUri, shapes, "fn total_area", 0, 3));
      assert.strictEqual(hs.length, 2, JSON.stringify(hs));
      for (const h of hs) assert.strictEqual(slice(shapes, h.range), "total_area");
      assert.deepStrictEqual(hs.map((h) => h.kind).sort(), [2, 3]); // read, write
    });

    await check("foldingRange: declarations and comment blocks", async () => {
      const rs = await c.request("textDocument/foldingRange", doc(shapesUri));
      const at = (needle) => pos(shapes, needle).line;
      assert(rs.some((r) => r.startLine === at("class ByArea") && !r.kind), JSON.stringify(rs));
      assert(rs.every((r) => r.endLine > r.startLine));
      const demoRanges = await c.request("textDocument/foldingRange", doc(demoUri));
      assert(demoRanges.some((r) => r.kind === "imports" && r.startLine === pos(demo, "import lib.geometry").line));
    });

    await check("semanticTokens: names classified by what they refer to", async () => {
      const legend = init.capabilities.semanticTokensProvider.legend;
      const r = await c.request("textDocument/semanticTokens/full", doc(shapesUri));
      const toks = [];
      let line = 0;
      let col = 0;
      for (let i = 0; i < r.data.length; i += 5) {
        const [dl, dc, len, ty, mods] = r.data.slice(i, i + 5);
        line += dl;
        col = dl ? dc : col + dc;
        toks.push({ line, col, len, type: legend.tokenTypes[ty], mods });
      }
      const find = (needle, delta = 0) => {
        const p = pos(shapes, needle, 0, delta);
        return toks.find((t) => t.line === p.line && t.col === p.character);
      };
      assert.strictEqual(find("class ByArea", 6).type, "class");
      assert.strictEqual(find("fn total_area", 3).type, "function");
      assert.strictEqual(find("by_area =").type, "variable");
      assert.strictEqual(find("puts(").type, "function");
    });

    await check("rangeFormatting: only the selected lines change", async () => {
      const messy = shapes
        .replace("fn largest[T: Comparable](xs: List[T]) -> T? {", "fn largest[T: Comparable]( xs:List[T] )->T?{")
        .replace("by_area = shapes", "by_area  =  shapes");
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 20 }, contentChanges: [{ text: messy }] });
      const line = pos(messy, "fn largest").line;
      const edits = await c.request("textDocument/rangeFormatting", {
        ...doc(shapesUri),
        range: { start: { line, character: 0 }, end: { line, character: 3 } },
        options: { tabSize: 2, insertSpaces: true },
      });
      assert.strictEqual(edits.length, 1, JSON.stringify(edits));
      assert.strictEqual(edits[0].range.start.line, line);
      assert.match(edits[0].newText, /^fn largest\[T: Comparable\]\(xs: List\[T\]\) -> T\? \{/);
      c.notify("textDocument/didChange", { textDocument: { uri: shapesUri, version: 21 }, contentChanges: [{ text: shapes }] });
    });

    await check("completion: import paths, a module's names, and no operators after `.`", async () => {
      const probe = "import lib.\n" + demo;
      c.notify("textDocument/didChange", { textDocument: { uri: demoUri, version: 2 }, contentChanges: [{ text: probe }] });
      const r = await c.request("textDocument/completion", { ...doc(demoUri), position: { line: 0, character: "import lib.".length } });
      assert.deepStrictEqual((Array.isArray(r) ? r : r.items).map((i) => i.label).sort(), ["geometry", "textutil"]);
      const probe2 = "import lib.geometry.{\n" + demo;
      c.notify("textDocument/didChange", { textDocument: { uri: demoUri, version: 3 }, contentChanges: [{ text: probe2 }] });
      const r2 = await c.request("textDocument/completion", { ...doc(demoUri), position: { line: 0, character: "import lib.geometry.{".length } });
      const names = (Array.isArray(r2) ? r2 : r2.items).map((i) => i.label);
      for (const n of ["Point", "Quadrant", "quadrant", "area"]) assert(names.includes(n), `no ${n} in ${names}`);
      const probe3 = demo + '\n"abc".\n';
      c.notify("textDocument/didChange", { textDocument: { uri: demoUri, version: 4 }, contentChanges: [{ text: probe3 }] });
      const r3 = await c.request("textDocument/completion", { ...doc(demoUri), position: { line: probe3.split("\n").length - 2, character: 6 } });
      const members = (Array.isArray(r3) ? r3 : r3.items).map((i) => i.label);
      assert(members.includes("upcase"));
      for (const op of ["+", "*", "<=>", "[]"]) assert(!members.includes(op), `operator ${op} offered`);
      c.notify("textDocument/didChange", { textDocument: { uri: demoUri, version: 5 }, contentChanges: [{ text: demo }] });
    });

    await check("an unknown request is answered with MethodNotFound, and the server keeps going", async () => {
      let code;
      try {
        await c.request("textDocument/linkedEditingRange", at(shapesUri, shapes, "by_area =", 0));
      } catch (e) {
        code = e.code;
      }
      assert.strictEqual(code, -32601);
      const h = await c.request("textDocument/hover", at(shapesUri, shapes, "by_area =", 0));
      assert(h);
    });

    await check("requests at odd positions answer null instead of failing", async () => {
      const far = { ...doc(shapesUri), position: { line: 10000, character: 5 } };
      for (const m of ["textDocument/hover", "textDocument/definition", "textDocument/signatureHelp"]) {
        const r = await c.request(m, far);
        assert(r === null || (Array.isArray(r) && r.length === 0) || typeof r === "object", `${m}: ${JSON.stringify(r)}`);
      }
      const inComment = at(shapesUri, shapes, "Heron's formula", 0, 2);
      assert.strictEqual(await c.request("textDocument/hover", inComment), null);
      assert.strictEqual(await c.request("textDocument/definition", inComment), null);
    });

    await check("didClose clears the document's diagnostics", async () => {
      c.notifications = [];
      c.notify("textDocument/didClose", doc(demoUri));
      const n = await c.waitFor((m) => m.method === "textDocument/publishDiagnostics" && m.params.uri === demoUri);
      assert.deepStrictEqual(n.params.diagnostics, []);
    });

    await check("references: files under the workspace folder count when they are not open", async () => {
      // Neither lib/geometry.cx nor modules_demo.cx (just closed) is open.
      const geo = fs.readFileSync(path.join(dir, "lib/geometry.cx"), "utf8");
      const r = await c.request("textDocument/references", {
        ...at(geometryUri, geo, "fn area", 0, 3),
        context: { includeDeclaration: true },
      });
      const where = r.map((l) => `${l.uri === geometryUri ? "geometry" : l.uri === demoUri ? "demo" : l.uri}:${l.range.start.line}`);
      assert.deepStrictEqual(where.sort(), [
        `demo:${pos(demo, "geometry.area(square)").line}`,
        `demo:${pos(demo, "geometry.area(triangle)").line}`,
        `geometry:${pos(geo, "fn area").line}`,
      ]);
    });

    await check("shutdown and exit", async () => {
      assert.strictEqual(await c.request("shutdown", null), null);
      c.notify("exit", null);
      const code = await Promise.race([c.exited, new Promise((r) => setTimeout(() => r("timeout"), 10000))]);
      assert.strictEqual(code, 0);
    });
  } finally {
    c.proc.kill();
    fs.rmSync(dir, { recursive: true, force: true });
    fs.rmSync(cacheHome, { recursive: true, force: true });
  }

  console.log(`\n${passed} passed, ${failures} failed`);
  process.exit(failures ? 1 : 0);
}

module.exports = { Client, pos, slice, uriOf };

if (require.main === module) {
  main().catch((e) => {
    console.error(e);
    process.exit(1);
  });
}
