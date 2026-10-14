//! Sorting and ordering require `Comparable`; tuples and lists are comparable when their
//! elements are.

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

/// `(code, message, notes)` of each error in `src`.
fn errors(src: &str) -> Vec<(String, String, String)> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| {
            let notes: Vec<&str> = d.notes.iter().map(|(n, _)| n.as_str()).collect();
            (d.code.to_string(), d.message.clone(), notes.join("\n"))
        })
        .collect()
}

#[test]
fn sorting_needs_comparable_items() {
    let src = "class K { v: Int }\nxs = [K.new(2), K.new(1)]\nxs.sorted()\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0311");
    assert!(errs[0].2.contains("`sorted` needs `T: Comparable`, and `K` is not `Comparable`"), "{errs:?}");
}

#[test]
fn sort_keys_need_comparable() {
    let src = "class K { v: Int }\nxs = [K.new(2)]\nxs.sort_by { |k| (k.v, k) }\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0368");
    assert!(errs[0].1.contains("`(Int, K)` does not satisfy the bound `Comparable`"), "{errs:?}");
}

#[test]
fn tuples_and_lists_of_comparable_items_are_comparable() {
    let src = "a = (1, \"b\") < (1, \"c\")\nb = [[2, 1]].sort()\nc = [(1, 2.5)].sorted()\nd = [1] <=> [1, 2]\n\
               fn top[T: Comparable](xs: List[T]) -> T? { xs.max() }\ne = top([(1, [\"x\"])])\n";
    assert!(errors(src).is_empty(), "{:?}", errors(src));
}

#[test]
fn tuple_order_needs_comparable_elements() {
    let src = "class K { v: Int }\nx = (1, K.new(1)) < (1, K.new(2))\n";
    let errs = errors(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "E0316");
}
