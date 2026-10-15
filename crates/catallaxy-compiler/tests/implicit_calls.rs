//! Methods the language calls implicitly (indexing, operators, equality, hashing, interpolation,
//! iteration) end narrowings like an explicit call when they are user-defined: they may run a
//! closure that assigns a local or change a field.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn error_codes(src: &str) -> Vec<String> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| d.code.to_string()).collect()
}

/// A class `V` whose implicitly called methods all run the stored closure `f`, which sets `x`
/// to nil, followed by `body`.
fn with_v(body: &str) -> String {
    format!(
        "x: Int? = 5\n\
         class V {{\n  f: fn() = fn() {{ }}\n\
         \x20 fn [](self, i: Int) -> Int {{ (self.f)(); i }}\n\
         \x20 fn []=(self, i: Int, v: Int) {{ (self.f)() }}\n\
         \x20 fn +(self, o: Int) -> V {{ (self.f)(); self }}\n\
         \x20 fn ==(self, o: V) -> Bool {{ (self.f)(); true }}\n\
         \x20 fn <=>(self, o: V) -> Int {{ (self.f)(); 0 }}\n\
         \x20 fn hash(self) -> Int {{ (self.f)(); 1 }}\n\
         \x20 fn to_s(self) -> String {{ (self.f)(); \"v\" }}\n\
         \x20 fn each(self, b: fn(Int)) {{ (self.f)(); b(1) }}\n}}\n\
         class W {{\n  v: V\n}}\n\
         v = V.new(f: fn() {{ x = nil }})\n\
         d: Dict[V, Int] = [:]\n\
         dv: Dyn = v\n\
         {body}\n"
    )
}

#[test]
fn implicit_user_calls_end_the_narrowing_of_a_closure_assigned_local() {
    let bad = [
        "if x != nil { w = v[0]; y: Int = x }",
        "if x != nil { v[0] = 1; y: Int = x }",
        "if x != nil { v += 1; y: Int = x }",
        "if x != nil { b = v == v; y: Int = x }",
        "if x != nil { b = [v] == [v]; y: Int = x }",
        "if x != nil { b = [v] < [v]; y: Int = x }",
        "if x != nil { d[v] = 1; y: Int = x }",
        "if x != nil { k = d[v]; y: Int = x }",
        "if x != nil { e = [(v): 1]; y: Int = x }",
        "if x != nil { s = \"#{v}\"; y: Int = x }",
        "if x != nil { s = \"#{[v]}\"; y: Int = x }",
        "if x != nil { s = \"#{W.new(v: v)}\"; y: Int = x }",
        "if x != nil { for i in v { }; y: Int = x }",
        "if x != nil { u = dv + 1; y: Int = x }",
        "if x != nil { s = \"#{dv}\"; y: Int = x }",
        "if x != nil {\n  match dv {\n    1 => 0\n    _ => 1\n  }\n  y: Int = x\n}",
        // In a condition, after the test.
        "if x != nil && v[0] > 0 { y: Int = x }",
        // A loop body that makes such a call runs it before the next iteration.
        "if x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    k = v[0]\n    i += 1\n  }\n}",
        "if x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    s = \"#{v}\"\n    i += 1\n  }\n}",
        "if x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    d[v] = 1\n    i += 1\n  }\n}",
        "if x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    for j in v { }\n    i += 1\n  }\n}",
    ];
    for body in bad {
        let src = with_v(body);
        assert_eq!(error_codes(&src), ["E0300"], "{body}");
    }
}

#[test]
fn builtin_implicit_operations_keep_the_narrowing() {
    let ok = [
        "xs = [1, 2]\nif x != nil { w = xs[0]; xs[1] = 3; y: Int = x }",
        "ds: Dict[String, Int] = [:]\nif x != nil { ds[\"a\"] = 1; k = ds[\"a\"]; y: Int = x }",
        "if x != nil { s = \"#{x} #{[1]} #{(1, true)}\"; y: Int = x }",
        "if x != nil { b = [1] == [1] && (1, \"a\") < (2, \"b\"); y: Int = x }",
        "if x != nil { for i in [1, 2] { }; for j in 0..2 { }; y: Int = x }",
        "if x != nil { n = 1; n += 2; y: Int = x }",
        // A class without user-defined methods is compared by identity and formatted by fields.
        "class P {\n  n: Int\n}\np = P.new(n: 1)\npd: Dict[P, Int] = [:]\n\
         if x != nil { pd[p] = 1; b = p == p; s = \"#{p}\"; y: Int = x }",
        // A loop whose body only indexes lists and interpolates numbers.
        "xs = [1, 2]\nif x != nil {\n  i = 0\n  while i < 2 {\n    y: Int = x\n    s = \"#{i} #{xs[i]}\"\n    i += 1\n  }\n}",
    ];
    for body in ok {
        let src = with_v(body);
        assert!(error_codes(&src).is_empty(), "{body}\n{:?}", error_codes(&src));
    }
}

#[test]
fn implicit_user_calls_end_field_path_narrowings() {
    let base = "class N {\n  n: Int\n  next: N?\n}\n\
                class C {\n  head: N\n  fn [](self, i: Int) -> Int { self.head.next = nil; i }\n  \
                fn to_s(self) -> String { self.head.next = nil; \"c\" }\n}\n\
                head = N.new(n: 1, next: N.new(n: 2, next: nil))\nc = C.new(head: head)\n";
    for (body, code) in [
        ("if head.next != nil {\n  k = c[0]\n  puts(head.next.n)\n}", "E0310"),
        ("if head.next != nil {\n  s = \"#{c}\"\n  m: N = head.next\n}", "E0300"),
    ] {
        let src = format!("{base}{body}\n");
        assert_eq!(error_codes(&src), [code], "{body}");
    }
}

#[test]
fn a_stable_field_path_ends_when_a_call_may_reassign_its_root() {
    let src = "frozen class H {\n  v: Int?\n}\nh = H.new(v: 1)\ng = fn() { h = H.new(v: nil) }\n\
               if h.v != nil {\n  g()\n  y: Int = h.v\n}\n";
    assert_eq!(error_codes(src), ["E0300"]);
    // Without a closure that reassigns `h`, the path stays narrowed across calls.
    let ok = "frozen class H {\n  v: Int?\n}\nh = H.new(v: 1)\nfn other() {}\n\
              if h.v != nil {\n  other()\n  y: Int = h.v\n}\n";
    assert!(error_codes(ok).is_empty(), "{:?}", error_codes(ok));
}
