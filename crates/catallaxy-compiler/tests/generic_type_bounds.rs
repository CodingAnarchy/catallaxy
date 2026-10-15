//! Bounds on the type parameters of generic classes, enums and interfaces are enforced at every
//! instantiation (#161).

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message, line)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String, usize)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| {
            let line = src[..d.span.start as usize].matches('\n').count() + 1;
            (d.code.to_string(), d.message.clone(), line)
        })
        .collect()
}

const SORTED: &str = "class SortedList[T: Comparable] {\n  items: List[T] = []\n  \
                      fn add(self, x: T) { self.items.push(x); self.items = self.items.sorted }\n}\n\
                      class P { n: Int = 0 }\n";

fn unmet(errs: &[(String, String, usize)], ty: &str, bound: &str, owner: &str) -> Vec<usize> {
    let msg = format!("`{ty}` does not satisfy the bound `{bound}` of type parameter `T` of `{owner}`");
    errs.iter().filter(|e| e.0 == "E0368" && e.1 == msg).map(|e| e.2).collect()
}

#[test]
fn explicit_type_arguments_and_annotations() {
    let src = format!(
        "{SORTED}s = SortedList[P].new()\nt: SortedList[P] = SortedList.new()\n\
         fn f(xs: SortedList[P]) -> Int {{ 0 }}\nclass W {{ s: SortedList[P] }}\n"
    );
    let errs = errors(&src);
    assert_eq!(unmet(&errs, "P", "Comparable", "SortedList"), vec![6, 7, 8, 9], "{errs:?}");
    assert_eq!(errs.len(), 4, "one error per written type, none for the inferred constructor: {errs:?}");
}

#[test]
fn inferred_type_arguments() {
    // Inferred from the constructor's arguments, and from a later use.
    let src = format!(
        "{SORTED}u = SortedList.new()\nu.add(P.new())\n\
         class Box[T: Hashable] {{ v: T }}\nBox.new(v: [1, 2])\nBox.new(v: 1)\nok = SortedList.new()\nok.add(3)\n"
    );
    let errs = errors(&src);
    assert_eq!(unmet(&errs, "P", "Comparable", "SortedList"), vec![6], "{errs:?}");
    assert_eq!(unmet(&errs, "List[Int]", "Hashable", "Box"), vec![9], "{errs:?}");
    assert_eq!(errs.len(), 2, "{errs:?}");
}

#[test]
fn enum_instantiations() {
    let src = "enum Heap[T: Comparable] {\n  Leaf\n  Node(T, Heap[T])\n}\nclass P { n: Int = 0 }\n\
               h: Heap[P] = .Leaf\nh2 = Heap.Node(P.new(), .Leaf)\nh3 = Heap.Node(3, .Leaf)\n";
    let errs = errors(src);
    assert_eq!(unmet(&errs, "P", "Comparable", "Heap"), vec![6, 7], "{errs:?}");
    assert_eq!(errs.len(), 2, "{errs:?}");
}

#[test]
fn type_parameters_need_the_bound_too() {
    let src = format!(
        "{SORTED}class W[T] {{ s: SortedList[T] }}\nclass W2[T: Comparable] {{ s: SortedList[T] }}\n\
         fn k[U](x: U) {{ SortedList.new().add(x) }}\nfn k2[U: Comparable](x: U) {{ SortedList.new().add(x) }}\n\
         interface Holder[T: Comparable] {{ fn get(self) -> T }}\nfn hp(h: Holder[P]) -> Int {{ 1 }}\n\
         class Sub[T] < SortedList[T] {{ }}\n"
    );
    let errs = errors(&src);
    assert_eq!(unmet(&errs, "T", "Comparable", "SortedList"), vec![6, 12], "{errs:?}");
    assert_eq!(unmet(&errs, "U", "Comparable", "SortedList"), vec![8], "{errs:?}");
    assert_eq!(unmet(&errs, "P", "Comparable", "Holder"), vec![11], "{errs:?}");
    assert_eq!(errs.len(), 4, "{errs:?}");
}

#[test]
fn untyped_parameters_get_the_bound_as_a_requirement() {
    let src = format!("{SORTED}fn g(x) {{ s = SortedList.new()\n  s.add(x)\n  s }}\ng(1)\ng(P.new())\n");
    let errs = errors(&src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0341", "{errs:?}");
    assert_eq!(errs[0].2, 10, "{errs:?}");
}
