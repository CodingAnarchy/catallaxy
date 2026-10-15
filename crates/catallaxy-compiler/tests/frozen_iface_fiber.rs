//! A frozen value is usable as an interface only if the methods implementing it do not mutate
//! (E0614), and a `Fiber`, pinned to the thread that started it, cannot move to another thread
//! (E0617).

use std::path::Path;

use catallaxy_compiler::driver::{compile_source, Options};

fn error_codes(src: &str) -> Vec<String> {
    let c = compile_source(Path::new("t.cx"), src, &Options { check_only: true });
    c.diags.iter().filter(|d| d.is_error()).map(|d| d.code.to_string()).collect()
}

const BAG: &str = "class Bag {\n  items: List[Int] = []\n  fn each(self, f: fn(Int)) {\n    self.items.push(0)\n    for i in self.items { f(i) }\n  }\n}\n\
                   class Ro {\n  items: List[Int] = [1]\n  fn each(self, f: fn(Int)) {\n    for i in self.items { f(i) }\n  }\n}\n\
                   fn total(e: Enumerable[Int]) -> Int { e.sum }\n\
                   interface Pusher { fn push(self, x: Int) }\n";

#[test]
fn a_frozen_value_is_not_an_interface_it_implements_with_a_mutating_method() {
    let bad = [
        // A native mutating method.
        "p: Pusher = freeze([1, 2])\np.push(3)",
        // An inferred mutating method of a user class, as an argument.
        "puts(total(freeze(Bag.new())))",
        // Through a generic bound.
        "fn sum_of[T: Enumerable[Int]](e: T) -> Int { e.sum }\nputs(sum_of(freeze(Bag.new())))",
        // A method that mutates through another method on `self`.
        "class Deep {\n  n: Int = 0\n  fn bump(self) { self.n += 1 }\n  fn push(self, x: Int) { self.bump() }\n}\np: Pusher = freeze(Deep.new())",
    ];
    for body in bad {
        let src = format!("{BAG}{body}\n");
        assert_eq!(error_codes(&src), ["E0614"], "{body}");
    }
    let ok = [
        "puts(total(freeze([1, 2])))",
        "puts(total(freeze(Ro.new())))",
        "puts(total(Bag.new()))",
        "p: Pusher = [1, 2]\np.push(3)",
    ];
    for body in ok {
        let src = format!("{BAG}{body}\n");
        assert!(error_codes(&src).is_empty(), "{body}\n{:?}", error_codes(&src));
    }
}

#[test]
fn a_fiber_cannot_move_to_another_thread() {
    let bad = [
        "ch: Channel[Fiber[Int]] = Channel.new()\nf = Fiber.start { 1 }\nch.send(move f)",
        "ch: Channel[List[Fiber[Int]]] = Channel.new()\nch.send(move [Fiber.start { 1 }])",
        "class W {\n  f: Fiber[Int]\n}\nw = W.new(Fiber.start { 1 })\nt = spawn move { w.f.is_done() }",
        "t = spawn { Fiber.start { 1 } }",
        "r = [1, 2].par_map { |i| Fiber.start { i } }",
        "m = Mutex.new(move [Fiber.start { 1 }])",
        "f = Fiber.start { 1 }\nt = spawn move { f.join() }",
        "enum Job { Running(Fiber[Int]), Idle }\nch: Channel[Job] = Channel.new()\nj = Job.Running(Fiber.start { 1 })\nch.send(move j)",
    ];
    for src in bad {
        let codes = error_codes(&format!("{src}\n"));
        assert!(!codes.is_empty() && codes.iter().all(|c| c == "E0617"), "{src}\n{codes:?}");
    }
    let ok = [
        "f = Fiber.start { 1 }\nputs(f.join())",
        "ch: Channel[Int] = Channel.new()\nf = Fiber.start { 1 }\nch.send(f.join().unwrap())",
        "t = spawn { Fiber.start { 1 }.join().unwrap() }\nputs(t.join())",
    ];
    for src in ok {
        let codes = error_codes(&format!("{src}\n"));
        assert!(codes.is_empty(), "{src}\n{codes:?}");
    }
}
