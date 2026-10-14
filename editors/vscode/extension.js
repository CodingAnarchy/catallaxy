// Starts `catx lsp` for Catallaxy files. Highlighting comes from the TextMate grammar alone.
"use strict";

const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

/** @type {LanguageClient | undefined} */
let client;
/** @type {vscode.OutputChannel | undefined} */
let output;

function serverPath() {
  const configured = vscode.workspace.getConfiguration("catallaxy").get("serverPath") || "catx";
  // Allow `~/...` (VS Code does not expand it).
  if (configured.startsWith("~/")) {
    return require("os").homedir() + configured.slice(1);
  }
  return configured;
}

async function start() {
  const command = serverPath();
  const executable = { command, args: ["lsp"] };
  client = new LanguageClient(
    "catallaxy",
    "Catallaxy",
    { run: executable, debug: executable },
    {
      documentSelector: [
        { scheme: "file", language: "catallaxy" },
        { scheme: "untitled", language: "catallaxy" },
      ],
      outputChannel: output,
    },
  );
  try {
    await client.start();
  } catch (e) {
    const choice = await vscode.window.showErrorMessage(
      `Could not start the Catallaxy language server (\`${command} lsp\`): ${e && e.message ? e.message : e}. ` +
        "Install catx or set catallaxy.serverPath.",
      "Open Settings",
    );
    if (choice === "Open Settings") {
      vscode.commands.executeCommand("workbench.action.openSettings", "catallaxy.serverPath");
    }
    client = undefined;
  }
}

async function stop() {
  if (client) {
    const c = client;
    client = undefined;
    await c.stop().catch(() => {});
  }
}

/** @param {vscode.ExtensionContext} context */
async function activate(context) {
  output = vscode.window.createOutputChannel("Catallaxy");
  context.subscriptions.push(
    output,
    vscode.commands.registerCommand("catallaxy.restartServer", async () => {
      await stop();
      await start();
    }),
    vscode.workspace.onDidChangeConfiguration(async (e) => {
      if (e.affectsConfiguration("catallaxy.serverPath")) {
        await stop();
        await start();
      }
    }),
  );
  await start();
}

function deactivate() {
  return stop();
}

module.exports = { activate, deactivate };
