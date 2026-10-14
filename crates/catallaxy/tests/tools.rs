//! End-to-end tests of `catx lsp`, `catx repl`, `catx fmt` and `catx check`, and of `catx run`
//! under a fixed `TZ`.

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
            let v = self
                .rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
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
    // A real file URI on every platform: `file:///C:/Users/...` on Windows, percent-encoded the way
    // the server encodes the locations it returns.
    let p = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file://");
    if !p.starts_with('/') {
        uri.push('/');
    }
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    (uri, path)
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
    let h = lsp.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 3, "character": 1}}),
    );
    let value = h["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("total: Int"), "{h}");

    // Hover over the call of `f` shows its signature.
    let h = lsp.request(
        3,
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 2, "character": 0}}),
    );
    let value = h["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("fn f(s: String) -> Int"), "{h}");

    // Definition of `count` in `total = count + 1` is on line 1.
    let d = lsp.request(
        4,
        "textDocument/definition",
        json!({"textDocument": {"uri": uri}, "position": {"line": 3, "character": 9}}),
    );
    assert_eq!(d["result"]["range"]["start"]["line"], 1, "{d}");

    // Member completion after `.` on a String.
    let text2 = "name = \"ada\"\nname.\n";
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
        "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": text2}]
    }}));
    let c = lsp.request(
        5,
        "textDocument/completion",
        json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 5}}),
    );
    let labels: Vec<&str> = c["result"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(labels.contains(&"upcase") && labels.contains(&"length"), "{labels:?}");

    // Scope completion offers locals and prelude functions.
    let c = lsp.request(
        6,
        "textDocument/completion",
        json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 0}}),
    );
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
        let c = lsp.request(
            8 + i as i64,
            "textDocument/completion",
            json!({"textDocument": {"uri": uri}, "position": {"line": 5, "character": col}}),
        );
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

impl Lsp {
    /// Starts a server and completes the initialize handshake, returning the capabilities.
    fn init() -> (Lsp, Value) {
        let mut lsp = Lsp::start();
        let init = lsp.request(1, "initialize", json!({"processId": null, "rootUri": null, "capabilities": {}}));
        lsp.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        let caps = init["result"]["capabilities"].clone();
        (lsp, caps)
    }

    /// Opens a document (also written to disk, so imports resolve either way) and waits for its
    /// diagnostics.
    fn open(&mut self, name: &str, text: &str) -> String {
        let (uri, path) = file_uri(name);
        std::fs::write(&path, text).unwrap();
        self.send(json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
            "textDocument": {"uri": uri, "languageId": "catallaxy", "version": 1, "text": text}
        }}));
        let u = uri.clone();
        self.wait("publishDiagnostics", move |v| {
            v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == json!(u)
        });
        uri
    }

    fn finish(mut self) {
        let r = self.request(999, "shutdown", Value::Null);
        assert!(r.get("error").is_none(), "{r}");
        self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        assert!(self.child.wait().unwrap().success());
    }
}

/// LSP position of the `nth` occurrence of `needle` on line `line` of `text`.
fn pos(text: &str, line: usize, needle: &str, nth: usize) -> Value {
    let l = text.lines().nth(line).unwrap();
    let mut from = 0;
    for _ in 0..nth {
        from += l[from..].find(needle).unwrap() + needle.len();
    }
    let byte = from + l[from..].find(needle).unwrap_or_else(|| panic!("{needle} not on line {line}"));
    json!({"line": line, "character": l[..byte].encode_utf16().count()})
}

/// `(uri, line, character)` of each location (or text edit with its uri), sorted.
fn spots(items: impl IntoIterator<Item = (String, Value)>) -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = items
        .into_iter()
        .map(|(u, r)| (u, r["start"]["line"].as_u64().unwrap(), r["start"]["character"].as_u64().unwrap()))
        .collect();
    out.sort();
    out
}

fn spot(uri: &str, p: Value) -> (String, u64, u64) {
    (uri.to_string(), p["line"].as_u64().unwrap(), p["character"].as_u64().unwrap())
}

/// Locations from a references response.
fn ref_spots(r: &Value) -> Vec<(String, u64, u64)> {
    let list = r["result"].as_array().unwrap_or_else(|| panic!("{r}"));
    spots(list.iter().map(|l| (l["uri"].as_str().unwrap().to_string(), l["range"].clone())))
}

/// Edits of a rename response, checking every one inserts `new`.
fn edit_spots(r: &Value, new: &str) -> Vec<(String, u64, u64)> {
    let changes = r["result"]["changes"].as_object().unwrap_or_else(|| panic!("{r}"));
    let mut items = Vec::new();
    for (uri, edits) in changes {
        for e in edits.as_array().unwrap() {
            assert_eq!(e["newText"], new, "{r}");
            items.push((uri.clone(), e["range"].clone()));
        }
    }
    spots(items)
}

const GEO: &str = "pub class Point {
  x: Int
  y: Int = 0
  pub fn sum(self) -> Int { self.x + self.y }
}
pub fn origin() -> Point { Point.new(0) }
";

const NAV_MAIN: &str = "import nav_geo.{Point, origin}
fn scale(p: Point, by: Int = 2) -> Int { p.sum * by }
x = 1
fn twice(x: Int) -> Int { x + x }
y = x + scale(Point.new(x, y: 2), by: 3)
[1, 2].each { |x| puts(x) }
puts(x + y + twice(y) + origin().x)
";

#[test]
fn lsp_references_and_rename() {
    let (mut lsp, caps) = Lsp::init();
    assert_eq!(caps["referencesProvider"], true, "{caps}");
    assert_eq!(caps["renameProvider"]["prepareProvider"], true, "{caps}");
    let geo = lsp.open("nav_geo.cx", GEO);
    let main = lsp.open("nav_main.cx", NAV_MAIN);
    let t = NAV_MAIN;
    let at = |p: Value| json!({"textDocument": {"uri": main}, "position": p});
    let refs = |lsp: &mut Lsp, id: i64, p: Value, decl: bool| {
        let mut params = at(p);
        params["context"] = json!({"includeDeclaration": decl});
        ref_spots(&lsp.request(id, "textDocument/references", params))
    };

    // The top-level `x` is distinct from the parameter `x` of `twice` and the block parameter `x`.
    let top_x = vec![spot(&main, pos(t, 2, "x", 0)), spot(&main, pos(t, 4, "x", 0)), spot(&main, pos(t, 4, "(x", 0))];
    let mut want = top_x.clone();
    want[2].2 += 1;
    want.push(spot(&main, pos(t, 6, "x", 0)));
    want.sort();
    assert_eq!(refs(&mut lsp, 2, pos(t, 4, "x", 0), true), want);
    let mut no_decl = want.clone();
    no_decl.retain(|s| s.1 != 2);
    assert_eq!(refs(&mut lsp, 3, pos(t, 6, "x", 0), false), no_decl);

    // Rename the top-level `x`: exactly its own occurrences change.
    let mut p = at(pos(t, 2, "x", 0));
    p["newName"] = json!("count");
    let r = lsp.request(4, "textDocument/rename", p);
    assert_eq!(edit_spots(&r, "count"), want);

    // Rename the parameter `x` of `twice`: declaration and both uses, nothing else.
    let mut p = at(pos(t, 3, "x", 1));
    p["newName"] = json!("n");
    let r = lsp.request(5, "textDocument/rename", p);
    let want: Vec<_> = (0..3).map(|i| spot(&main, pos(t, 3, "x", i))).collect();
    assert_eq!(edit_spots(&r, "n"), want);

    // The block parameter `x` shadows the top-level one.
    let mut p = at(pos(t, 5, "x", 1));
    p["newName"] = json!("item");
    let r = lsp.request(6, "textDocument/rename", p);
    let want = vec![spot(&main, pos(t, 5, "x", 0)), spot(&main, pos(t, 5, "x", 1))];
    assert_eq!(edit_spots(&r, "item"), want);

    // A parameter with a default renames its named argument at the call site too.
    let mut p = at(pos(t, 1, "by", 1));
    p["newName"] = json!("factor");
    let r = lsp.request(7, "textDocument/rename", p);
    let want = vec![spot(&main, pos(t, 1, "by", 0)), spot(&main, pos(t, 1, "by", 1)), spot(&main, pos(t, 4, "by", 0))];
    assert_eq!(edit_spots(&r, "factor"), want);

    // Cross-file: the field `x`, from a use in main; `Point.new(x, y: 2)` names the field `y`.
    let mut p = at(pos(t, 6, ".x", 0));
    p["position"]["character"] = json!(p["position"]["character"].as_u64().unwrap() + 1);
    p["newName"] = json!("px");
    let r = lsp.request(8, "textDocument/rename", p);
    let want = vec![spot(&geo, pos(GEO, 1, "x", 0)), spot(&geo, pos(GEO, 3, ".x", 0)), spot(&main, pos(t, 6, ".x", 0))];
    let want: Vec<_> = want.into_iter().map(|(u, l, c)| if l == 1 { (u, l, c) } else { (u, l, c + 1) }).collect();
    assert_eq!(edit_spots(&r, "px"), want);
    let mut p = at(pos(t, 4, "y:", 0));
    p["context"] = json!({"includeDeclaration": true});
    let r = ref_spots(&lsp.request(9, "textDocument/references", p));
    assert_eq!(
        r,
        vec![spot(&geo, pos(GEO, 2, "y", 0)), spot(&geo, pos(GEO, 3, ".y", 0)), spot(&main, pos(t, 4, "y:", 0))]
            .into_iter()
            .map(|(u, l, c)| if l == 3 { (u, l, c + 1) } else { (u, l, c) })
            .collect::<Vec<_>>()
    );

    // The class across files: declaration, type annotations, constructor calls and the import.
    let mut p = at(pos(t, 1, "Point", 0));
    p["newName"] = json!("Pt");
    let r = lsp.request(10, "textDocument/rename", p);
    let want = vec![
        spot(&geo, pos(GEO, 0, "Point", 0)),
        spot(&geo, pos(GEO, 5, "Point", 0)),
        spot(&geo, pos(GEO, 5, "Point", 1)),
        spot(&main, pos(t, 0, "Point", 0)),
        spot(&main, pos(t, 1, "Point", 0)),
        spot(&main, pos(t, 4, "Point", 0)),
    ];
    let mut want = want;
    want.sort();
    assert_eq!(edit_spots(&r, "Pt"), want);

    // Methods: references to `sum` from its use.
    let r = refs(&mut lsp, 11, pos(t, 1, "sum", 0), true);
    assert_eq!(r, vec![spot(&geo, pos(GEO, 3, "sum", 0)), spot(&main, pos(t, 1, "sum", 0))]);

    // prepareRename: the name's range; refused for prelude functions and invalid new names.
    let r = lsp.request(12, "textDocument/prepareRename", at(pos(t, 3, "twice", 0)));
    assert_eq!(r["result"]["start"], pos(t, 3, "twice", 0), "{r}");
    let r = lsp.request(13, "textDocument/prepareRename", at(pos(t, 6, "puts", 0)));
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("puts"), "{r}");
    let mut p = at(pos(t, 3, "twice", 0));
    p["newName"] = json!("fn");
    let r = lsp.request(14, "textDocument/rename", p);
    assert!(r.get("error").is_some(), "{r}");
    lsp.finish();
}

const ALIAS_MAIN: &str = "import alias_geo as g
p: g.Point = g.Point.new(1)
puts(g.origin().sum + p.sum)
";

#[test]
fn lsp_import_alias() {
    let (mut lsp, _) = Lsp::init();
    let geo = lsp.open("alias_geo.cx", GEO);
    let main = lsp.open("alias_main.cx", ALIAS_MAIN);
    let t = ALIAS_MAIN;
    let at = |p: Value| json!({"textDocument": {"uri": main}, "position": p});

    // Go to definition through the alias.
    let d = lsp.request(2, "textDocument/definition", at(pos(t, 2, "origin", 0)));
    assert_eq!(d["result"]["uri"], json!(geo), "{d}");
    assert_eq!(d["result"]["range"]["start"], pos(GEO, 5, "origin", 0), "{d}");
    let d = lsp.request(6, "textDocument/definition", at(pos(t, 1, "Point", 1)));
    assert_eq!(d["result"]["uri"], json!(geo), "{d}");
    assert_eq!(d["result"]["range"]["start"]["line"], 0, "{d}");

    // References and rename of the class cover the uses through the alias.
    let mut p = at(pos(t, 1, "Point", 0));
    p["context"] = json!({"includeDeclaration": true});
    let r = ref_spots(&lsp.request(3, "textDocument/references", p));
    let mut want = vec![
        spot(&geo, pos(GEO, 0, "Point", 0)),
        spot(&geo, pos(GEO, 5, "Point", 0)),
        spot(&geo, pos(GEO, 5, "Point", 1)),
        spot(&main, pos(t, 1, "Point", 0)),
        spot(&main, pos(t, 1, "Point", 1)),
    ];
    want.sort();
    assert_eq!(r, want);
    let mut p = at(pos(t, 1, "Point", 1));
    p["newName"] = json!("Pt");
    assert_eq!(edit_spots(&lsp.request(4, "textDocument/rename", p), "Pt"), want);

    // Completion after the alias lists the module's items.
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
        "textDocument": {"uri": main, "version": 2}, "contentChanges": [{"text": format!("{t}g.\n")}]
    }}));
    let c = lsp.request(5, "textDocument/completion", at(json!({"line": 3, "character": 2})));
    let labels: Vec<&str> = c["result"].as_array().unwrap().iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(labels.contains(&"origin") && labels.contains(&"Point"), "{labels:?}");
    lsp.finish();
}

const IFACE: &str = "interface Shape {
  fn area(self) -> Float
  fn twice(self) -> Float { self.area() * 2.0 }
}
class Sq {
  s: Float
  fn area(self) -> Float { self.s * self.s }
}
xs: List[Shape] = [Sq.new(3.0)]
puts(xs[0].area() + Sq.new(1.0).s)
";

/// From a field's declaration (where the generated constructor's parameter also sits), and
/// calls dispatched through an interface (which name the interface member).
#[test]
fn lsp_field_declarations_and_interface_calls() {
    let (mut lsp, _) = Lsp::init();
    let uri = lsp.open("nav_iface.cx", IFACE);
    let t = IFACE;
    let at = |p: Value| json!({"textDocument": {"uri": uri}, "position": p});
    let refs = |lsp: &mut Lsp, id: i64, p: Value| {
        let mut params = at(p);
        params["context"] = json!({"includeDeclaration": true});
        ref_spots(&lsp.request(id, "textDocument/references", params))
    };
    let field = vec![spot(&uri, pos(t, 5, "s", 0)), spot(&uri, pos(t, 6, ".s", 0)), spot(&uri, pos(t, 6, ".s", 1))];
    let mut field: Vec<_> = field.into_iter().map(|(u, l, c)| if l == 6 { (u, l, c + 1) } else { (u, l, c) }).collect();
    let mut last = spot(&uri, pos(t, 9, ".s", 0));
    last.2 += 1;
    field.push(last);
    assert_eq!(refs(&mut lsp, 2, pos(t, 5, "s", 0)), field);

    // The member, the calls through the interface, and the method of `Sq`, which is used as a
    // `Shape` (interfaces are structural, so they are renamed together).
    let member = vec![
        spot(&uri, pos(t, 1, "area", 0)),
        spot(&uri, pos(t, 2, "area", 0)),
        spot(&uri, pos(t, 6, "area", 0)),
        spot(&uri, pos(t, 9, "area", 0)),
    ];
    assert_eq!(refs(&mut lsp, 3, pos(t, 9, "area", 0)), member);
    assert_eq!(refs(&mut lsp, 4, pos(t, 1, "area", 0)), member);
    assert_eq!(refs(&mut lsp, 5, pos(t, 6, "area", 0)), member);

    let h = lsp.request(6, "textDocument/hover", at(pos(t, 9, "area", 0)));
    assert!(h["result"]["contents"]["value"].as_str().unwrap().contains("fn Shape.area(self) -> Float"), "{h}");
    let d = lsp.request(7, "textDocument/definition", at(pos(t, 2, "area", 0)));
    assert_eq!(d["result"]["range"]["start"], pos(t, 1, "area", 0), "{d}");
    lsp.finish();
}

const CONF_SHAPES: &str = "pub interface Shape {
  fn area(self) -> Float
}
pub fn total(xs: List[Shape]) -> Float {
  sum = 0.0
  for x in xs { sum = sum + x.area() }
  sum
}
";

const CONF_KINDS: &str = "pub class Sq {
  s: Float
  fn area(self) -> Float { self.s * self.s }
}
pub class Disc {
  r: Float
  fn area(self) -> Float { 3.0 * self.r * self.r }
}
pub class Tri {
  b: Float
  fn area(self) -> Float { self.b / 2.0 }
}
";

const CONF_MAIN: &str = "import conf_shapes.{Shape, total}
import conf_kinds.{Sq, Disc, Tri}
fn larger[T: Shape](a: T, b: T) -> Float { if a.area() > b.area() { a.area() } else { b.area() } }
sq = Sq.new(3.0)
puts(total([sq]) + sq.area())
puts(Disc.new(1.0).area())
puts(larger(Tri.new(1.0), Tri.new(2.0)))
";

/// Renaming an interface member renames the methods of the classes used as that interface
/// (a list element, a generic bound) in every open document, and the calls to them; a class
/// with a method of the same name that is never used as the interface keeps it.
#[test]
fn lsp_rename_interface_conformers() {
    let (mut lsp, _) = Lsp::init();
    let shapes = lsp.open("conf_shapes.cx", CONF_SHAPES);
    let kinds = lsp.open("conf_kinds.cx", CONF_KINDS);
    let main = lsp.open("conf_main.cx", CONF_MAIN);
    let at = |uri: &str, p: Value| json!({"textDocument": {"uri": uri}, "position": p});
    let refs = |lsp: &mut Lsp, id: i64, uri: &str, p: Value| {
        let mut params = at(uri, p);
        params["context"] = json!({"includeDeclaration": true});
        ref_spots(&lsp.request(id, "textDocument/references", params))
    };
    let rename = |lsp: &mut Lsp, id: i64, uri: &str, p: Value| {
        let mut params = at(uri, p);
        params["newName"] = json!("size");
        edit_spots(&lsp.request(id, "textDocument/rename", params), "size")
    };
    let mut family = vec![
        spot(&shapes, pos(CONF_SHAPES, 1, "area", 0)),
        spot(&shapes, pos(CONF_SHAPES, 5, "area", 0)),
        // `Sq` (a list element) and `Tri` (a bound), not `Disc`.
        spot(&kinds, pos(CONF_KINDS, 2, "area", 0)),
        spot(&kinds, pos(CONF_KINDS, 10, "area", 0)),
        spot(&main, pos(CONF_MAIN, 4, "area", 0)),
    ];
    family.extend((0..4).map(|i| spot(&main, pos(CONF_MAIN, 2, "area", i))));
    family.sort();

    // From the interface member, whose own module knows no class.
    assert_eq!(rename(&mut lsp, 2, &shapes, pos(CONF_SHAPES, 1, "area", 0)), family);
    // From the conforming classes' methods, and from calls.
    assert_eq!(rename(&mut lsp, 3, &kinds, pos(CONF_KINDS, 2, "area", 0)), family);
    assert_eq!(rename(&mut lsp, 4, &kinds, pos(CONF_KINDS, 10, "area", 0)), family);
    assert_eq!(refs(&mut lsp, 5, &main, pos(CONF_MAIN, 4, "area", 0)), family);
    assert_eq!(refs(&mut lsp, 6, &shapes, pos(CONF_SHAPES, 5, "area", 0)), family);
    let r = lsp.request(7, "textDocument/prepareRename", at(&kinds, pos(CONF_KINDS, 10, "area", 0)));
    assert_eq!(r["result"]["start"], pos(CONF_KINDS, 10, "area", 0), "{r}");

    // `Disc` is never used as a `Shape`: its method stays a symbol of its own.
    let disc = vec![spot(&kinds, pos(CONF_KINDS, 6, "area", 0)), spot(&main, pos(CONF_MAIN, 5, "area", 0))];
    assert_eq!(refs(&mut lsp, 8, &main, pos(CONF_MAIN, 5, "area", 0)), disc);
    assert_eq!(rename(&mut lsp, 9, &kinds, pos(CONF_KINDS, 6, "area", 0)), disc);
    lsp.finish();
}

const CONF_WAYS: &str = "interface Shape { fn area(self) -> Float }
class A { fn area(self) -> Float { 1.0 } }
class B { fn area(self) -> Float { 2.0 } }
class C { fn area(self) -> Float { 3.0 } }
class D { fn area(self) -> Float { 4.0 } }
fn make() -> Shape { A.new() }
s: Shape = B.new()
d: Dict[String, Shape] = [\"c\": C.new()]
puts(make().area() + s.area() + D.new().area() + d.size.to_f)
";

/// Conversions by return, assignment and dict element all count; `D` is never a `Shape`.
#[test]
fn lsp_rename_interface_conversions() {
    let (mut lsp, _) = Lsp::init();
    let uri = lsp.open("conf_ways.cx", CONF_WAYS);
    let t = CONF_WAYS;
    let mut p = json!({"textDocument": {"uri": uri}, "position": pos(t, 0, "area", 0)});
    p["newName"] = json!("size");
    let mut want: Vec<_> = (0..4).map(|l| spot(&uri, pos(t, l, "area", 0))).collect();
    want.extend((0..2).map(|i| spot(&uri, pos(t, 8, "area", i))));
    want.sort();
    assert_eq!(edit_spots(&lsp.request(2, "textDocument/rename", p), "size"), want);
    lsp.finish();
}

const CONF_BUILTIN: &str = "interface Sized {
  fn length(self) -> Int
}
fn measure(x: Sized) -> Int { x.length() }
puts(measure(\"abc\"))
";

/// A member implemented by a built-in type is not renamed: the type would stop conforming.
#[test]
fn lsp_rename_interface_member_implemented_by_builtin() {
    let (mut lsp, _) = Lsp::init();
    let uri = lsp.open("conf_builtin.cx", CONF_BUILTIN);
    let p = json!({"textDocument": {"uri": uri}, "position": pos(CONF_BUILTIN, 1, "length", 0)});
    let r = lsp.request(2, "textDocument/prepareRename", p.clone());
    assert!(r["error"]["message"].as_str().unwrap_or_default().contains("String.length"), "{r}");
    let mut p = p;
    p["newName"] = json!("size");
    let r = lsp.request(3, "textDocument/rename", p);
    assert!(r.get("error").is_some(), "{r}");
    lsp.finish();
}

#[test]
fn lsp_signature_help() {
    let (mut lsp, caps) = Lsp::init();
    assert!(caps["signatureHelpProvider"]["triggerCharacters"].as_array().unwrap().contains(&json!("(")), "{caps}");
    let head = "fn scale(n: Int, by: Int = 2) -> Int { n * by }\nfn apply(n: Int, f: fn(Int) -> Int) -> Int { f(n) }\nclass Box {\n  w: Int\n  h: Int = 1\n}\n";
    let cases: [(&str, i64, Option<&str>, &str); 5] = [
        ("scale(1, ", 1, Some("by: Int = 2"), "fn scale(n: Int, by: Int = 2) -> Int"),
        ("scale(", 0, Some("n: Int"), "fn scale("),
        ("scale(1, by: ", 1, Some("by: Int = 2"), "fn scale("),
        ("apply(3, ", 1, Some("f: fn(Int) -> Int"), "fn apply(n: Int, f: fn(Int) -> Int) -> Int"),
        ("Box.new(2, ", 1, Some("h: Int = 1"), "fn Box.new(w: Int, h: Int = 1)"),
    ];
    let uri = lsp.open("sig.cx", head);
    for (i, (call, active, param, label)) in cases.into_iter().enumerate() {
        let text = format!("{head}{call}\n");
        lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
            "textDocument": {"uri": uri, "version": 2 + i}, "contentChanges": [{"text": text}]
        }}));
        let line = head.lines().count();
        let r = lsp.request(
            10 + i as i64,
            "textDocument/signatureHelp",
            json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": call.len()}}),
        );
        let sig = &r["result"]["signatures"][0];
        let lab = sig["label"].as_str().unwrap_or_else(|| panic!("{call}: {r}"));
        assert!(lab.starts_with(label), "{call}: {lab}");
        assert_eq!(r["result"]["activeParameter"], active, "{call}: {r}");
        let [a, b] = [0, 1].map(|k| sig["parameters"][active as usize]["label"][k].as_u64().unwrap() as usize);
        let utf16: Vec<u16> = lab.encode_utf16().collect();
        assert_eq!(String::from_utf16(&utf16[a..b]).unwrap(), param.unwrap(), "{call}: {r}");
        if call.starts_with("apply") {
            let doc = sig["parameters"][1]["documentation"].as_str().unwrap_or_default();
            assert!(doc.contains("block"), "{r}");
        }
    }
    // Outside any call: no help.
    let r = lsp.request(
        30,
        "textDocument/signatureHelp",
        json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}}),
    );
    assert!(r["result"].is_null(), "{r}");
    lsp.finish();
}

#[test]
fn lsp_code_actions() {
    let (mut lsp, caps) = Lsp::init();
    assert_eq!(caps["codeActionProvider"]["codeActionKinds"], json!(["quickfix"]), "{caps}");
    let text = "class Point {\n  x: Int\n}\np = Point(1)\nlenght = 3\nputs(lengt)\n";
    let uri = lsp.open("actions.cx", text);
    let all = json!({"start": {"line": 0, "character": 0}, "end": {"line": 6, "character": 0}});
    let r = lsp.request(
        40,
        "textDocument/codeAction",
        json!({"textDocument": {"uri": uri}, "range": all, "context": {"diagnostics": []}}),
    );
    let actions = r["result"].as_array().unwrap_or_else(|| panic!("{r}"));
    let titles: Vec<&str> = actions.iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["call `new`", "did you mean `lenght`?"], "{r}");
    let new = &actions[0];
    assert_eq!(new["kind"], "quickfix");
    assert_eq!(new["isPreferred"], true);
    assert_eq!(new["diagnostics"][0]["code"], "E0331");
    let edit = &new["edit"]["changes"][uri.as_str()][0];
    assert_eq!(edit["newText"], ".new", "{r}");
    assert_eq!(edit["range"]["start"], json!({"line": 3, "character": 9}), "{r}");
    // A guess is offered but not preferred.
    assert!(actions[1]["isPreferred"].is_null(), "{r}");
    assert_eq!(actions[1]["edit"]["changes"][uri.as_str()][0]["newText"], "lenght", "{r}");

    // Only diagnostics touching the requested range.
    let line3 = json!({"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 3}});
    let r = lsp.request(
        41,
        "textDocument/codeAction",
        json!({"textDocument": {"uri": uri}, "range": line3, "context": {"diagnostics": []}}),
    );
    assert_eq!(r["result"].as_array().map(|a| a.len()), Some(0), "{r}");
    let r = lsp.request(
        42,
        "textDocument/codeAction",
        json!({"textDocument": {"uri": uri}, "range": all, "context": {"diagnostics": [], "only": ["refactor"]}}),
    );
    assert_eq!(r["result"].as_array().map(|a| a.len()), Some(0), "{r}");
    lsp.finish();
}

#[test]
fn lsp_inlay_hints() {
    let (mut lsp, caps) = Lsp::init();
    assert_eq!(caps["inlayHintProvider"], true, "{caps}");
    let text = "fn shout(x) { x.upcase }\nn = 41\nnames = [\"a\"]\ntyped: Int = 1\nfor s in names {\n  puts(shout(s))\n}\ntotal = names.map { |s| s.length }\n";
    let uri = lsp.open("hints.cx", text);
    let r = lsp.request(
        2,
        "textDocument/inlayHint",
        json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 8, "character": 0}}}),
    );
    let hints: Vec<(u64, u64, String)> = r["result"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter()
        .map(|h| {
            assert_eq!(h["kind"], 1, "{h}");
            (
                h["position"]["line"].as_u64().unwrap(),
                h["position"]["character"].as_u64().unwrap(),
                h["label"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let has = |line: u64, col: u64, label: &str| hints.iter().any(|h| h == &(line, col, label.to_string()));
    assert!(has(1, 1, ": Int"), "{hints:?}");
    assert!(has(2, 5, ": List[String]"), "{hints:?}");
    assert!(has(4, 5, ": String"), "{hints:?}");
    assert!(has(7, 5, ": List[Int]"), "{hints:?}");
    assert!(has(7, 22, ": String"), "{hints:?}");
    // The implicit-generic parameter shows its inferred requirement.
    assert!(hints.iter().any(|h| h.0 == 0 && h.1 == 10 && h.2.contains("upcase")), "{hints:?}");
    // Annotated variables get no hint.
    assert!(!hints.iter().any(|h| h.0 == 3), "{hints:?}");
    // Only hints in the requested range.
    let r = lsp.request(
        3,
        "textDocument/inlayHint",
        json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}}}),
    );
    assert_eq!(r["result"].as_array().map(|a| a.len()), Some(1), "{r}");
    lsp.finish();
}

#[test]
fn lsp_format_spec_interpolation() {
    let (mut lsp, _) = Lsp::init();
    let text =
        "price = 2.5\nputs(\"#{price:>8.2f}\")\nputs(\"#{[1].map { |v| v * 2 }.length:03}\")\nputs(\"#{\"s\":.2f}\")\n";
    let (uri, _) = file_uri("fmt_spec.cx");
    lsp.send(json!({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
        "textDocument": {"uri": uri, "languageId": "catallaxy", "version": 1, "text": text}
    }}));
    let diags = lsp.wait("publishDiagnostics", |v| v["method"] == "textDocument/publishDiagnostics");
    let list = diags["params"]["diagnostics"].as_array().unwrap();
    assert!(list.iter().any(|d| d["code"] == "E0380" && d["range"]["start"]["line"] == 3), "{diags}");
    assert_eq!(list.len(), 1, "{diags}");
    // Hover over `price` inside the formatted interpolation.
    let h = lsp.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": uri}, "position": {"line": 1, "character": 10}}),
    );
    let value = h["result"]["contents"]["value"].as_str().unwrap_or_default();
    assert!(value.contains("price: Float"), "{h}");
    // Block parameters inside a formatted interpolation still get inlay hints.
    let r = lsp.request(
        3,
        "textDocument/inlayHint",
        json!({"textDocument": {"uri": uri}, "range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}}),
    );
    let labels: Vec<&str> = r["result"].as_array().unwrap().iter().filter_map(|h| h["label"].as_str()).collect();
    assert!(labels.contains(&": Int"), "{r}");
    lsp.finish();
}

#[test]
fn lsp_document_and_workspace_symbols() {
    let (mut lsp, caps) = Lsp::init();
    assert_eq!(caps["documentSymbolProvider"], true, "{caps}");
    assert_eq!(caps["workspaceSymbolProvider"], true, "{caps}");
    let text = "class Shape {\n  sides: Int\n  fn describe(self) -> String { \"#{self.sides}\" }\n}\nenum Color {\n  Red\n  Green\n}\nconst LIMIT = 3\nfn main_shape() -> Shape { Shape.new(4) }\ncount = 2\n";
    let uri = lsp.open("outline.cx", text);
    let r = lsp.request(2, "textDocument/documentSymbol", json!({"textDocument": {"uri": uri}}));
    let syms = r["result"].as_array().unwrap_or_else(|| panic!("{r}"));
    let find = |list: &Vec<Value>, name: &str| list.iter().find(|s| s["name"] == name).cloned();
    let shape = find(syms, "Shape").unwrap_or_else(|| panic!("{r}"));
    assert_eq!(shape["kind"], 5); // class
    assert_eq!(shape["selectionRange"]["start"], json!({"line": 0, "character": 6}));
    let kids = shape["children"].as_array().unwrap();
    assert_eq!(find(kids, "sides").unwrap()["kind"], 8); // field
    let describe = find(kids, "describe").unwrap();
    assert_eq!(describe["kind"], 6); // method
    assert!(describe["detail"].as_str().unwrap().contains("-> String"), "{describe}");
    let color = find(syms, "Color").unwrap();
    assert_eq!(color["kind"], 10); // enum
    assert_eq!(color["children"].as_array().unwrap().len(), 2);
    assert_eq!(find(syms, "LIMIT").unwrap()["kind"], 14); // constant
    assert_eq!(find(syms, "main_shape").unwrap()["kind"], 12); // function
    assert_eq!(find(syms, "count").unwrap()["kind"], 13); // variable

    let r = lsp.request(3, "workspace/symbol", json!({"query": "shape"}));
    let names: Vec<&str> = r["result"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"Shape") && names.contains(&"main_shape"), "{names:?}");
    assert!(!names.contains(&"Color"), "{names:?}");
    let s = r["result"].as_array().unwrap().iter().find(|s| s["name"] == "Shape").unwrap();
    assert_eq!(s["location"]["uri"], json!(uri));
    lsp.finish();
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

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("catx-{tag}-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn catx(args: &[&std::ffi::OsStr]) -> (i32, String, String) {
    let out = Command::new(CATX).args(args).output().expect("run catx");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).replace('\\', "/"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn fmt_and_check_walk_directories() {
    let dir = scratch_dir("fmt");
    std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    std::fs::create_dir_all(dir.join("reject")).unwrap();
    std::fs::create_dir_all(dir.join(".hidden")).unwrap();
    std::fs::write(dir.join("a.cx"), "puts(1)\n").unwrap();
    std::fs::write(dir.join("sub/b.cx"), "x=1\nputs(x)\n").unwrap();
    std::fs::write(dir.join("sub/deeper/c.cx"), "puts( 3 )\n").unwrap();
    std::fs::write(dir.join("sub/notes.txt"), "not code (\n").unwrap();
    // Skipped while walking: deliberately broken programs and hidden directories.
    std::fs::write(dir.join("reject/bad.cx"), "fn (\n").unwrap();
    std::fs::write(dir.join(".hidden/h.cx"), "puts( 4 )\n").unwrap();

    let (code, out, err) = catx(&["fmt".as_ref(), "--check".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 1, "{out}{err}");
    let listed: Vec<&str> = out.lines().collect();
    assert_eq!(listed.len(), 2, "{out}");
    assert!(listed[0].ends_with("/sub/b.cx") && listed[1].ends_with("/sub/deeper/c.cx"), "{out}");

    let (code, out, err) = catx(&["fmt".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(std::fs::read_to_string(dir.join("sub/b.cx")).unwrap(), "x = 1\nputs(x)\n");
    assert_eq!(std::fs::read_to_string(dir.join("reject/bad.cx")).unwrap(), "fn (\n");
    assert_eq!(catx(&["fmt".as_ref(), "--check".as_ref(), dir.as_os_str()]).0, 0);

    // A file named directly is never skipped.
    assert_eq!(catx(&["fmt".as_ref(), "--check".as_ref(), dir.join("reject/bad.cx").as_os_str()]).0, 2);

    // `check` walks everything, `reject/` included.
    std::fs::remove_file(dir.join("reject/bad.cx")).unwrap();
    let (code, _, err) = catx(&["check".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 0, "{err}");
    std::fs::write(dir.join("sub/deeper/c.cx"), "x: Int = \"s\"\n").unwrap();
    let (code, _, err) = catx(&["check".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("E0300"), "{err}");
    assert!(err.contains("help: for more on E0300, see https://") && err.contains("/docs/errors.md#e0300"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn local_offset_follows_tz() {
    // `TZ=XYZ-5:30` (POSIX and the Windows CRT agree) is a fixed zone 5h30m east of UTC.
    let dir = scratch_dir("tz");
    let prog = dir.join("tz.cx");
    std::fs::write(&prog, "t = DateTime.of(2024, 7, 1, 12)\nputs(t.to_local())\nputs(DateTime.now().offset)\n")
        .unwrap();
    let cases = [("XYZ-5:30", "2024-07-01T17:30:00+05:30\n19800\n"), ("XYZ+3", "2024-07-01T09:00:00-03:00\n-10800\n")];
    for (tz, want) in cases {
        let out = Command::new(CATX).arg("run").arg(&prog).env("TZ", tz).output().expect("run catx");
        let got = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        assert_eq!(got, want, "TZ={tz}: {}", String::from_utf8_lossy(&out.stderr));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fix_applies_checker_fixes() {
    let dir = scratch_dir("fix");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let fixable = "class P {\n  x: Int\n}\np = P(1)\nputs(p.x)\n";
    std::fs::write(dir.join("a.cx"), fixable).unwrap();
    // A type error with no applicable fix stays, and keeps the exit status at 1.
    std::fs::write(dir.join("sub/b.cx"), "x: Int = \"s\"\n").unwrap();

    let (code, out, err) = catx(&["fix".as_ref(), "--dry-run".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("/a.cx:4:5: call `new` [E0331]"), "{out}");
    assert!(err.contains("would fix 1 file") && err.contains("1 error is left"), "{err}");
    assert_eq!(std::fs::read_to_string(dir.join("a.cx")).unwrap(), fixable);

    let (code, out, err) = catx(&["fix".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 1, "{out}{err}");
    assert_eq!(std::fs::read_to_string(dir.join("a.cx")).unwrap(), fixable.replace("P(1)", "P.new(1)"));
    assert_eq!(std::fs::read_to_string(dir.join("sub/b.cx")).unwrap(), "x: Int = \"s\"\n");

    // Nothing left to fix in `a.cx`; once `b.cx` is gone the run is clean.
    std::fs::remove_file(dir.join("sub/b.cx")).unwrap();
    let (code, out, err) = catx(&["fix".as_ref(), dir.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.is_empty() && err.contains("fixed 0 files"), "{out}{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn string_append_lint_warns_and_fixes() {
    let dir = scratch_dir("w0004");
    std::fs::create_dir_all(&dir).unwrap();
    let src = "s = \"\"\nfor i in 0..100 {\n  s += \"#{i % 10}\"\n}\nputs(s.length)\n";
    let file = dir.join("a.cx");
    std::fs::write(&file, src).unwrap();
    // A warning: `check` still succeeds.
    let (code, out, err) = catx(&["check".as_ref(), file.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(format!("{out}{err}").contains("warning[W0004]"), "{out}{err}");
    let (code, out, err) = catx(&["fix".as_ref(), file.as_os_str()]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("a.cx:3:3: use a StringBuilder [W0004]"), "{out}");
    let want =
        "s_sb = StringBuilder.new()\nfor i in 0..100 {\n  s_sb.push(\"#{i % 10}\")\n}\ns = s_sb.to_s\nputs(s.length)\n";
    assert_eq!(std::fs::read_to_string(&file).unwrap(), want);
    let (code, out, err) = catx(&["run".as_ref(), file.as_os_str()]);
    assert_eq!((code, out.as_str()), (0, "101\n"), "{err}");
    assert!(!err.contains("W0004"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
