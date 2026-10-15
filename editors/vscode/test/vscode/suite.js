// Runs inside VS Code's extension host (see run.js): every LSP feature through VS Code's own
// provider commands, so the client side (the extension, vscode-languageclient's conversions) is
// exercised too.
"use strict";

const assert = require("assert");
const vscode = require("vscode");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(what, fn, timeoutMs = 30000) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(200);
  }
}

function posOf(doc, needle, nth = 0, delta = 0) {
  const text = doc.getText();
  let at = -1;
  for (let k = 0; k <= nth; k++) {
    at = text.indexOf(needle, at + 1);
    assert(at >= 0, `\`${needle}\` not found`);
  }
  return doc.positionAt(at + delta);
}

async function run() {
  const folder = vscode.workspace.workspaceFolders[0].uri;
  const uri = vscode.Uri.joinPath(folder, "shapes.cx");
  const doc = await vscode.workspace.openTextDocument(uri);
  await vscode.window.showTextDocument(doc);
  assert.strictEqual(doc.languageId, "catallaxy");
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === "catallaxy" && e.packageJSON.publisher === "catallaxy");
  assert(ext, "extension not loaded");
  await until("activation", () => ext.isActive);

  let failures = 0;
  const check = async (name, fn) => {
    try {
      await fn();
      console.log(`ok   ${name}`);
    } catch (e) {
      failures++;
      console.error(`FAIL ${name}\n  ${(e && e.stack) || e}`);
    }
  };
  const exec = (cmd, ...args) => vscode.commands.executeCommand(cmd, ...args);

  // The server is ready once hover answers.
  await until("the language server", async () => {
    const h = await exec("vscode.executeHoverProvider", uri, posOf(doc, "by_area ="));
    return h && h.length > 0;
  }, 60000);

  await check("hover", async () => {
    const h = await exec("vscode.executeHoverProvider", uri, posOf(doc, "by_area ="));
    const text = h.flatMap((x) => x.contents.map((c) => c.value || c)).join("\n");
    assert.match(text, /List\[Shape\]/);
  });

  await check("hover shows documentation comments", async () => {
    const h = await exec("vscode.executeHoverProvider", uri, posOf(doc, "ByArea.new(s)", 0, 1));
    const text = h.flatMap((x) => x.contents.map((c) => c.value || c)).join("\n");
    assert.match(text, /A wrapper that orders shapes by area/);
  });

  await check("diagnostics appear for an error and clear when it is undone", async () => {
    const end = doc.positionAt(doc.getText().length);
    const edit = new vscode.WorkspaceEdit();
    edit.insert(uri, end, 'zz: Int = "nope"\n');
    await vscode.workspace.applyEdit(edit);
    const ds = await until("a diagnostic", () => {
      const d = vscode.languages.getDiagnostics(uri);
      return d.length ? d : null;
    });
    assert.strictEqual(ds[0].severity, vscode.DiagnosticSeverity.Error);
    assert.strictEqual(String(ds[0].code), "E0300");
    await exec("undo");
    await until("diagnostics to clear", () => vscode.languages.getDiagnostics(uri).length === 0);
  });

  await check("definition", async () => {
    const d = await exec("vscode.executeDefinitionProvider", uri, posOf(doc, "Square.new", 0, 2));
    const loc = d[0];
    const range = loc.targetRange || loc.range;
    assert.strictEqual(range.start.line, posOf(doc, "class Square").line);
  });

  await check("definition of a prelude function opens the prelude's source", async () => {
    const d = await exec("vscode.executeDefinitionProvider", uri, posOf(doc, "puts("));
    const loc = d[0];
    const target = loc.targetUri || loc.uri;
    const range = loc.targetRange || loc.range;
    const pdoc = await vscode.workspace.openTextDocument(target);
    assert.match(pdoc.lineAt(range.start.line).text, /fn puts/);
    // It is not a program: no diagnostics.
    await sleep(500);
    assert.deepStrictEqual(vscode.languages.getDiagnostics(target), []);
  });

  await check("document highlights, folding ranges and semantic tokens", async () => {
    const hs = await exec("vscode.executeDocumentHighlights", uri, posOf(doc, "fn total_area", 0, 3));
    assert.strictEqual(hs.length, 2);
    const folds = await exec("vscode.executeFoldingRangeProvider", uri);
    assert(folds.some((f) => f.start === posOf(doc, "class ByArea").line), JSON.stringify(folds));
    const toks = await exec("vscode.provideDocumentSemanticTokens", uri);
    assert(toks && toks.data.length > 0, "no semantic tokens");
  });

  await check("references", async () => {
    const r = await exec("vscode.executeReferenceProvider", uri, posOf(doc, "fn total_area", 0, 3));
    assert.strictEqual(r.length, 2);
  });

  await check("rename", async () => {
    const e = await exec("vscode.executeDocumentRenameProvider", uri, posOf(doc, "by_area =", 0, 1), "ordered");
    const edits = e.get(uri);
    assert.strictEqual(edits.length, (doc.getText().match(/\bby_area\b/g) || []).length);
    for (const ed of edits) assert.strictEqual(ed.newText, "ordered");
  });

  await check("completion after `.`", async () => {
    const list = await exec("vscode.executeCompletionItemProvider", uri, posOf(doc, "shapes.sort_with", 0, "shapes.".length), ".");
    const labels = list.items.map((i) => (typeof i.label === "string" ? i.label : i.label.label));
    for (const m of ["map", "sort_with", "length"]) assert(labels.includes(m), `no ${m}`);
  });

  await check("signature help", async () => {
    const s = await exec("vscode.executeSignatureHelpProvider", uri, posOf(doc, "Triangle.new(3.0, 4.0", 0, "Triangle.new(3.0, ".length));
    assert.match(s.signatures[0].label, /a: Float, b: Float, c: Float/);
    assert.strictEqual(s.activeParameter, 1);
  });

  await check("inlay hints", async () => {
    const range = new vscode.Range(0, 0, doc.lineCount, 0);
    const hints = await exec("vscode.executeInlayHintProvider", uri, range);
    const label = (h) => (typeof h.label === "string" ? h.label : h.label.map((p) => p.value).join(""));
    const line = posOf(doc, "by_area =").line;
    assert(hints.some((h) => h.position.line === line && /List\[Shape\]/.test(label(h))), JSON.stringify(hints.slice(0, 5)));
  });

  await check("document symbols", async () => {
    const syms = await exec("vscode.executeDocumentSymbolProvider", uri);
    const names = syms.map((s) => s.name);
    for (const n of ["Shape", "Circle", "largest", "total_area"]) assert(names.includes(n), `no ${n}`);
    const circle = syms.find((s) => s.name === "Circle");
    assert(circle.children.some((c) => c.name === "area"));
  });

  await check("workspace symbols", async () => {
    const syms = await exec("vscode.executeWorkspaceSymbolProvider", "Triangle");
    assert(syms.some((s) => s.name === "Triangle"));
  });

  await check("formatting", async () => {
    const none = await exec("vscode.executeFormatDocumentProvider", uri, { tabSize: 2, insertSpaces: true });
    assert(!none || none.length === 0, JSON.stringify(none));
  });

  await check("restart command", async () => {
    await exec("catallaxy.restartServer");
    await until("the restarted server", async () => {
      const h = await exec("vscode.executeHoverProvider", uri, posOf(doc, "by_area ="));
      return h && h.length > 0;
    });
  });

  if (failures) throw new Error(`${failures} VS Code check(s) failed`);
}

module.exports = { run };
