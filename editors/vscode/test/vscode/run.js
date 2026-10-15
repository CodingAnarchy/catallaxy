// Runs the extension inside a real VS Code (downloaded by @vscode/test-electron) against a
// scratch workspace with copies of a few examples, and checks the language features end to end.
// On Linux without a display, run it under `xvfb-run -a`.
//
//   CATX=path/to/catx node test/vscode/run.js     (default: target/release/catx in the repository)
"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");
const { runTests } = require("@vscode/test-electron");

const root = path.resolve(__dirname, "../../../..");
const catx = path.resolve(
  process.env.CATX || path.join(root, "target/release/catx" + (process.platform === "win32" ? ".exe" : "")),
);

async function main() {
  if (!fs.existsSync(catx)) throw new Error(`${catx} not found: build it with \`cargo build --release\` or set CATX`);
  const ws = fs.mkdtempSync(path.join(os.tmpdir(), "catx-vscode-"));
  for (const f of ["shapes.cx", "modules_demo.cx", "lib/geometry.cx", "lib/textutil.cx"]) {
    fs.mkdirSync(path.dirname(path.join(ws, f)), { recursive: true });
    fs.copyFileSync(path.join(root, "examples", f), path.join(ws, f));
  }
  fs.mkdirSync(path.join(ws, ".vscode"));
  fs.writeFileSync(path.join(ws, ".vscode/settings.json"), JSON.stringify({ "catallaxy.serverPath": catx }, null, 2));
  const userData = fs.mkdtempSync(path.join(os.tmpdir(), "catx-vscode-user-"));
  // Where the server writes the prelude's sources (go to definition of `puts`).
  const cache = fs.mkdtempSync(path.join(os.tmpdir(), "catx-vscode-cache-"));
  try {
    await runTests({
      extensionDevelopmentPath: path.resolve(__dirname, "../.."),
      extensionTestsPath: path.join(__dirname, "suite.js"),
      launchArgs: [ws, "--disable-extensions", "--disable-workspace-trust", "--user-data-dir", userData],
      extensionTestsEnv: { XDG_CACHE_HOME: cache },
    });
  } finally {
    fs.rmSync(ws, { recursive: true, force: true });
    fs.rmSync(userData, { recursive: true, force: true });
    fs.rmSync(cache, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
