//! End-to-end tests of `catx lsp` and `catx repl`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

const CATX: &str = env!("CARGO_BIN_EXE_catx");

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<Value>,
}

impl Lsp {
    fn start() -> Lsp {
        let mut child = Command::new(CATX)
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn catx lsp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length: ") {
                        len = v.parse().unwrap();
                    }
                }
                let mut buf = vec![0; len];
                if r.read_exact(&mut buf).is_err() {
                    return;
                }
                if tx.send(serde_json::from_slice(&buf).unwrap()).is_err() {
                    return;
                }
            }
        });
        Lsp { child, stdin, rx }
    }

    fn send(&mut self, v: Value) {
        let body = v.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Waits for a message satisfying `pred`.
    fn wait(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let v = self.rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if pred(&v) {
                return v;
            }
        }
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.wait(method, |v| v["id"] == json!(id))
    }
}

fn file_uri(name: &str) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("catx-lsp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    (format!("file://{}", path.display()), path)
}

#[test]
fn lsp_diagnostics_hover_definition_completion() {
    let mut lsp = Lsp::start();
    let init = lsp.request(1, "initialize", json!({"processId": null, "rootUri": null, "capabilities": {}}));
    assert!(init["result"]["capabilities"]["hoverProvider"].as_bool().unwrap_or(false), "{init}");
    lsp.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));

    let (uri, _) = file_uri("main.cx");
    let text = "fn f(s: String) -> Int { s.length }\ncount = 41\nf(42)\ntotal = count + 1\n";
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": uri, "languageId": "catallaxy", "version": 1, "text": text}
    }}));
    let diags = lsp.wait("publishDiagnostics", |v| v["method"] == "textDocument/publishDiagnostics");
    let list = diags["params"]["diagnostics"].as_array().unwrap();
    assert!(list.iter().any(|d| d["code"] == "E0300" && d["range"]["start"]["line"] == 2), "{diags}");

    // Hover over `total` (line 3) shows its type.
    let h = lsp.request(2, "textDocument/hover", json!({"textDocument": {"uri": uri}, "position": {"line": 3, "character": 1}}));
    let value = h["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("total: Int"), "{h}");

    // Hover over the call of `f` shows its signature.
    let h = lsp.request(3, "textDocument/hover", json!({"textDocument": {"uri": uri}, "position": {"line": 2, "character": 0}}));
    let value = h["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("fn f(s: String) -> Int"), "{h}");

    // Definition of `count` in `total = count + 1` is on line 1.
    let d = lsp.request(4, "textDocument/definition", json!({"textDocument": {"uri": uri}, "position": {"line": 3, "character": 9}}));
    assert_eq!(d["result"]["range"]["start"]["line"], 1, "{d}");

    // Member completion after `.` on a String.
    let text2 = "name = \"ada\"\nname.\n";
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
        "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": text2}]
    }}));
    let c = lsp.request(5, "textDocument/completion", json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 5}}));
    let labels: Vec<&str> = c["result"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(labels.contains(&"upcase") && labels.contains(&"length"), "{labels:?}");

    // Scope completion offers locals and prelude functions.
    let c = lsp.request(6, "textDocument/completion", json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 0}}));
    let labels: Vec<&str> = c["result"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(labels.contains(&"name") && labels.contains(&"puts"), "{labels:?}");

    // Class receivers: fields and methods; class names: static members; lists: interface defaults.
    let class = "class P {\n  x: Int\n  fn dbl(self) -> Int { self.x * 2 }\n}\np = P.new(1)\n";
    let cases: [(&str, u32, &[&str]); 3] =
        [("p.", 2, &["x", "dbl"]), ("P.", 2, &["new"]), ("[1, 2].", 7, &["map", "push", "length"])];
    for (i, (line, col, want)) in cases.into_iter().enumerate() {
        let text = format!("{class}{line}\n");
        lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": {"uri": uri, "version": 3 + i}, "contentChanges": [{"text": text}]
        }}));
        let c = lsp.request(8 + i as i64, "textDocument/completion",
            json!({"textDocument": {"uri": uri}, "position": {"line": 5, "character": col}}));
        let labels: Vec<&str> = c["result"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
        for w in want {
            assert!(labels.contains(w), "{line}: missing {w} in {labels:?}");
        }
    }

    let r = lsp.request(7, "shutdown", Value::Null);
    assert!(r.get("error").is_none(), "{r}");
    lsp.send(json!({"jsonrpc": "2.0", "method": "exit"}));
    let status = lsp.child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn repl_piped_session() {
    let input = "x = 40\nx + 2\nfn sq(n: Int) -> Int {\n  n * n\n}\nsq(3)\ny: Int = \"s\"\nputs(\"hi\")\n:type [x]\npanic(\"boom\")\nx\nnil\n:reset\n:quit\nputs(\"unreachable\")\n";
    let mut child = Command::new(CATX)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn catx repl");
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success());
    assert_eq!(stdout, "=> 42\n=> 9\nhi\nList[Int]\n=> 40\nsession cleared\n", "stderr:\n{stderr}");
    assert!(stderr.contains("E0300"), "{stderr}");
    assert!(stderr.contains("panic: boom"), "{stderr}");
}
