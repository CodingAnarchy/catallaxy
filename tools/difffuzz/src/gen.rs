//! Generator of well-typed, terminating, deterministic Catallaxy programs.
//!
//! The generator is type-directed: it tracks every variable in scope with its type and produces
//! expressions of a requested type from productions that are known to type-check. Termination and
//! bounded resource use come from construction:
//!
//! - every loop has a literal iteration bound, and loops nest at most two deep;
//! - recursion goes through fixed helpers whose depth argument is a small literal;
//! - integer variables are reassigned only modulo a constant or by adding a literal, and `*` always
//!   has a literal operand, so numbers grow at most linearly in bits per iteration;
//! - list and string reassignments are truncated (`take`, `slice`).
//!
//! Determinism: nothing prints from another OS thread; results crossing threads are joined in a
//! fixed order, collected values are sorted, and `par_reduce` uses only associative operators.
//! Fibers on one thread interleave deterministically, so their logs are printed as is.
//!
//! Programs that `catx check` rejects are discarded by the driver (and counted); they indicate a gap
//! in the generator's model of the type system.

use crate::rng::Rng;

/// `self.line(&format!(...))`, with the arguments evaluated before `self` is borrowed.
macro_rules! emit {
    ($g:expr, $($fmt:tt)*) => {{
        let s = format!($($fmt)*);
        $g.line(&s)
    }};
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    Int,
    Bool,
    Str,
    List,
    Dict,
    Set,
    Opt,
    Frozen,
    Obj(usize),
    Enum(usize),
    Res,
    Fun,
}

impl Ty {
    /// May be captured by a `spawn` or `par_*` block.
    fn sendable(self) -> bool {
        matches!(self, Ty::Int | Ty::Bool | Ty::Str | Ty::Frozen | Ty::Opt | Ty::Enum(_))
    }
}

#[derive(Clone)]
struct Var {
    name: String,
    ty: Ty,
    assignable: bool,
    /// Declared by the program (not a parameter): its contents may be mutated. Function
    /// parameters are not, or a function called in a loop could grow a list without bound.
    mutable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Normal,
    /// A `spawn`/`par_*` block: outer variables are visible only if sendable, and read-only.
    Thread,
    /// Outer variables are visible but may not be reassigned (fiber blocks, to stay conservative).
    NoAssign,
}

struct Scope {
    vars: Vec<Var>,
    kind: ScopeKind,
}

struct Func {
    name: String,
    params: Vec<Ty>,
    ret: Ty,
}

struct Method {
    name: String,
    params: Vec<Ty>,
    /// `None` for a mutating method returning nothing.
    ret: Option<Ty>,
}

struct Class {
    name: String,
    parent: Option<usize>,
    /// All fields, inherited ones first (constructor order).
    fields: Vec<(String, Ty)>,
    methods: Vec<Method>,
}

struct EnumDef {
    name: String,
    variants: Vec<(String, Vec<Ty>)>,
}

const STRINGS: &[&str] = &["", "a", "ab", "foo", "Bar", "hello world", "x,y,z", "ÿé", "  pad  ", "42", "-7", "k1"];
const KEYS: &[&str] = &["k0", "k1", "k2", "k3", "k4"];

pub struct Gen {
    rng: Rng,
    out: String,
    ind: usize,
    scopes: Vec<Scope>,
    fresh: usize,
    funcs: Vec<Func>,
    classes: Vec<Class>,
    enums: Vec<EnumDef>,
    /// Names of `Tree` helpers: (enum, build fn, sum fn).
    trees: Vec<(String, String, String)>,
    /// Names of bounded-recursion helpers `fn r(n: Int, acc: Int) -> Int`.
    recs: Vec<String>,
    loop_depth: usize,
    block_depth: usize,
    /// > 0 when output must not be printed (functions, threads).
    quiet: usize,
    fiber_depth: usize,
    /// The class whose methods are being generated: never instantiated or called there (its
    /// overrides would recurse forever).
    defining: Option<usize>,
    /// Nonzero inside code that runs many times (big parallel blocks, hot thread loops): no
    /// calls to generated functions or methods, no recursion helpers, no nested parallel blocks.
    cheap: usize,
    budget: i64,
}

/// Generates the program for `seed`.
pub fn generate(seed: u64) -> String {
    let mut g = Gen {
        rng: Rng::new(seed),
        out: String::new(),
        ind: 0,
        scopes: vec![],
        fresh: 0,
        funcs: vec![],
        classes: vec![],
        enums: vec![],
        trees: vec![],
        recs: vec![],
        loop_depth: 0,
        block_depth: 0,
        quiet: 0,
        fiber_depth: 0,
        defining: None,
        cheap: 0,
        budget: 0,
    };
    g.program(seed);
    g.out
}

impl Gen {
    // ---------------------------------------------------------------- output helpers

    fn line(&mut self, s: &str) {
        for _ in 0..self.ind {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.fresh += 1;
        format!("{prefix}{}", self.fresh)
    }

    fn annot(&self, ty: Ty) -> String {
        match ty {
            Ty::Int => "Int".into(),
            Ty::Bool => "Bool".into(),
            Ty::Str => "String".into(),
            Ty::List => "List[Int]".into(),
            Ty::Dict => "Dict[String, Int]".into(),
            Ty::Set => "Set[Int]".into(),
            Ty::Opt => "Int?".into(),
            Ty::Frozen => "frozen List[Int]".into(),
            Ty::Obj(i) => self.classes[i].name.clone(),
            Ty::Enum(i) => self.enums[i].name.clone(),
            Ty::Res => "Result[Int, String]".into(),
            Ty::Fun => "fn(Int) -> Int".into(),
        }
    }

    // ---------------------------------------------------------------- scopes

    fn push_scope(&mut self, kind: ScopeKind) {
        self.scopes.push(Scope { vars: vec![], kind });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str, ty: Ty, assignable: bool) {
        self.scopes.last_mut().unwrap().vars.push(Var { name: name.into(), ty, assignable, mutable: assignable });
    }

    fn visible(&self) -> Vec<Var> {
        let mut res = vec![];
        let mut sendable_only = false;
        let mut read_only = false;
        for s in self.scopes.iter().rev() {
            for v in &s.vars {
                if sendable_only && (!v.ty.sendable() || v.name.starts_with("self.")) {
                    continue;
                }
                let mut v = v.clone();
                if read_only {
                    v.assignable = false;
                }
                res.push(v);
            }
            match s.kind {
                ScopeKind::Thread => {
                    sendable_only = true;
                    read_only = true;
                }
                ScopeKind::NoAssign => read_only = true,
                ScopeKind::Normal => {}
            }
        }
        res
    }

    fn is_subclass(&self, c: usize, of: usize) -> bool {
        let mut k = Some(c);
        while let Some(i) = k {
            if i == of {
                return true;
            }
            k = self.classes[i].parent;
        }
        false
    }

    fn fits(&self, have: Ty, want: Ty) -> bool {
        match (have, want) {
            (Ty::Obj(a), Ty::Obj(b)) => self.is_subclass(a, b),
            _ => have == want,
        }
    }

    fn var_of(&mut self, ty: Ty) -> Option<String> {
        let vs: Vec<Var> = self.visible().into_iter().filter(|v| self.fits(v.ty, ty)).collect();
        if vs.is_empty() {
            None
        } else {
            Some(self.rng.pick(&vs).name.clone())
        }
    }

    /// Runs `f` with a fresh scope binding block parameters of the given types; returns `{ |..| body }`.
    fn block(&mut self, kind: ScopeKind, params: &[Ty], f: impl FnOnce(&mut Self, &[String]) -> String) -> String {
        let names: Vec<String> = params.iter().map(|_| self.fresh("p")).collect();
        self.push_scope(kind);
        for (n, t) in names.iter().zip(params) {
            self.bind(n, *t, false);
        }
        if kind == ScopeKind::Thread {
            self.quiet += 1;
        }
        let body = f(self, &names);
        if kind == ScopeKind::Thread {
            self.quiet -= 1;
        }
        self.pop_scope();
        format!("{{ |{}| {} }}", names.join(", "), body)
    }

    // ---------------------------------------------------------------- expressions

    fn expr(&mut self, ty: Ty, d: u32) -> String {
        match ty {
            Ty::Int => self.int(d),
            Ty::Bool => self.boolean(d),
            Ty::Str => self.string(d),
            Ty::List => self.list(d),
            Ty::Dict => self.dict(d),
            Ty::Set => self.set(d),
            Ty::Opt => self.opt(d),
            Ty::Frozen => match self.var_of(Ty::Frozen) {
                Some(v) if self.rng.chance(70) => v,
                _ => format!("freeze({}.dup())", Self::recv(self.list(d.saturating_sub(1)))),
            },
            Ty::Obj(c) => self.obj(c, d),
            Ty::Enum(e) => self.enum_val(e, d),
            Ty::Res => self.res(d),
            Ty::Fun => match self.var_of(Ty::Fun) {
                Some(v) if self.rng.chance(60) => v,
                _ => {
                    let p = self.fresh("p");
                    self.push_scope(ScopeKind::Normal);
                    self.bind(&p, Ty::Int, false);
                    let body = self.int(d.saturating_sub(1));
                    self.pop_scope();
                    format!("{{ |{p}: Int| {body} }}")
                }
            },
        }
    }

    /// Parenthesizes literals so a method call on them parses as intended.
    fn recv(e: String) -> String {
        if e.starts_with(|c: char| c.is_ascii_digit() || c == '-' || c == '"' || c == '[') {
            format!("({e})")
        } else {
            e
        }
    }

    fn small(&mut self) -> i64 {
        self.rng.range(0, 6)
    }

    fn nonzero(&mut self) -> String {
        let mut n = self.rng.range(-7, 9);
        if n == 0 {
            n = 3;
        }
        if n < 0 {
            format!("({n})")
        } else {
            n.to_string()
        }
    }

    fn int_lit(&mut self) -> String {
        match self.rng.weighted(&[70, 15, 8, 4, 3]) {
            0 => self.rng.range(0, 20).to_string(),
            1 => format!("({})", self.rng.range(-50, -1)),
            2 => self.rng.range(100, 100_000).to_string(),
            3 => {
                self.rng.pick(&["12345678901234567890123", "4_611_686_018_427_387_904", "0xffff_ffff_ffff"]).to_string()
            }
            _ => self.rng.pick(&["(2 ** 64)", "(2 ** 62)", "(-(2 ** 63))", "0b1011", "0o17"]).to_string(),
        }
    }

    fn int_leaf(&mut self) -> String {
        let vs = self.visible();
        let mut cands: Vec<String> = vec![];
        for v in &vs {
            match v.ty {
                Ty::Int => {
                    cands.push(v.name.clone());
                    cands.push(v.name.clone());
                }
                Ty::List => cands.push(format!("{}.length", v.name)),
                Ty::Str => cands.push(format!("{}.length", v.name)),
                Ty::Opt => cands.push(format!("({} ?? {})", v.name, self.rng.range(-3, 9))),
                Ty::Dict => cands.push(format!("{}.fetch(\"{}\", 0)", v.name, self.rng.pick(KEYS))),
                Ty::Set => cands.push(format!("{}.length", v.name)),
                Ty::Frozen => cands.push(format!("{}.sum()", v.name)),
                Ty::Obj(c) => {
                    let ints: Vec<String> =
                        self.classes[c].fields.iter().filter(|f| f.1 == Ty::Int).map(|f| f.0.clone()).collect();
                    if !ints.is_empty() {
                        cands.push(format!("{}.{}", v.name, self.rng.pick(&ints)));
                    }
                }
                _ => {}
            }
        }
        if cands.is_empty() || self.rng.chance(35) {
            self.int_lit()
        } else {
            self.rng.pick(&cands).clone()
        }
    }

    fn int(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(20) {
            return self.int_leaf();
        }
        let d1 = d - 1;
        loop {
            let choice = self.rng.below(30);
            if self.cheap > 0 && matches!(choice, 12 | 13 | 23..=26) {
                continue;
            }
            match choice {
                0..=3 => {
                    let op = *self.rng.pick(&["+", "-", "+", "-", "&", "|", "^"]);
                    return format!("({} {} {})", self.int(d1), op, self.int(d1));
                }
                4 | 5 => {
                    let k = self.rng.range(-9, 9);
                    let k = if k < 0 { format!("({k})") } else { k.to_string() };
                    return format!("({} * {})", self.int(d1), k);
                }
                6 => {
                    let op = *self.rng.pick(&["/", "%"]);
                    return format!("({} {} {})", self.int(d1), op, self.nonzero());
                }
                7 => return format!("{}.abs()", Self::recv(self.int(d1))),
                8 => {
                    let m = *self.rng.pick(&["max", "min"]);
                    return format!("{}.{m}({})", Self::recv(self.int(d1)), self.int(d1));
                }
                9 => return format!("(if {} {{ {} }} else {{ {} }})", self.boolean(d1), self.int(d1), self.int(d1)),
                10 if !self.enums.is_empty() => {
                    let e = self.rng.below(self.enums.len());
                    return self.enum_match(e, d1);
                }
                11 => {
                    let o = self.opt(d1);
                    let p = self.fresh("p");
                    self.push_scope(ScopeKind::Normal);
                    self.bind(&p, Ty::Int, false);
                    let body = self.int(d1);
                    self.pop_scope();
                    let none = self.int_lit(); // the subject may be narrowed to Nil here
                    return format!("(match {o} {{ nil => {none}, {p}: Int => {body} }})");
                }
                12 => {
                    if let Some(call) = self.call_returning(Ty::Int, d1) {
                        return call;
                    }
                }
                13 => {
                    if let Some(call) = self.method_returning(Ty::Int, d1) {
                        return call;
                    }
                }
                14 if !self.enums.is_empty() => {
                    let e = self.rng.below(self.enums.len());
                    return format!("{}.score()", Self::recv(self.enum_val(e, d1)));
                }
                15 => {
                    let xs = Self::recv(self.list(d1));
                    let k = self.rng.range(1, 7);
                    let b = self.block(ScopeKind::Normal, &[Ty::Int, Ty::Int], |g, ps| {
                        format!("(({} * {k} + {}) % 1000)", ps[0], g.int(d1))
                    });
                    return format!("({xs}.reduce(0) {b})");
                }
                16 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.boolean(d1));
                    return format!("({xs}.count {b})");
                }
                17 => {
                    // Small indices: a huge one is a RangeError, not nil.
                    return format!("({}.get({} % 8) ?? {})", Self::recv(self.list(d1)), self.int(d1), self.int_lit());
                }
                18 => {
                    let m = *self.rng.pick(&["sum()", "length", "sum()"]);
                    return format!("{}.{m}", Self::recv(self.list(d1)));
                }
                19 => return format!("({}.max() ?? 0)", Self::recv(self.list(d1))),
                20 => {
                    let s = Self::recv(self.string(d1));
                    if self.rng.chance(50) {
                        return format!("{s}.length");
                    }
                    let lit = *self.rng.pick(STRINGS);
                    return format!("({s}.index_of(\"{lit}\") ?? -1)");
                }
                21 => {
                    if let Some(f) = self.var_of(Ty::Fun) {
                        return format!("{f}({})", self.int(d1));
                    }
                }
                22 => {
                    if self.funcs.iter().any(|f| f.ret == Ty::Res) {
                        return format!("{}.unwrap_or({})", self.res(d1), self.int_leaf());
                    }
                }
                23 if !self.trees.is_empty() => {
                    let (_, build, sum) = self.rng.pick(&self.trees).clone();
                    let depth = self.rng.range(0, 5);
                    return format!("{sum}({build}({depth}, {}))", self.int(d1));
                }
                24 if !self.recs.is_empty() => {
                    let r = self.rng.pick(&self.recs).clone();
                    let n = self.rng.range(0, 60);
                    return format!("{r}({n}, {})", self.int(d1));
                }
                25 => {
                    let xs = Self::recv(self.list(d1));
                    let op = *self.rng.pick(&["{ |a, b| a + b }", "{ |a, b| a.max(b) }", "{ |a, b| a.min(b) }"]);
                    return format!("({xs}.par_reduce(0) {op})");
                }
                26 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Thread, &[Ty::Int], |g, _| g.int(d1));
                    return format!("({xs}.par_map {b}).sum()");
                }
                27 => {
                    let dd = Self::recv(self.dict(d1));
                    if self.rng.chance(50) {
                        return format!("{dd}.length");
                    }
                    return format!("{dd}.fetch(\"{}\", {})", self.rng.pick(KEYS), self.int_leaf());
                }
                28 => return format!("{}.length", Self::recv(self.set(d1))),
                29 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.int(d1));
                    return format!("({xs}.sum_by {b})");
                }
                _ => {}
            }
        }
    }

    fn boolean(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(15) {
            if let Some(v) = self.var_of(Ty::Bool) {
                if self.rng.chance(60) {
                    return v;
                }
            }
            return match self.rng.below(4) {
                0 => "true".into(),
                1 => "false".into(),
                2 => format!("{}.is_even()", Self::recv(self.int_leaf())),
                _ => format!("({} < {})", self.int_leaf(), self.int_leaf()),
            };
        }
        let d1 = d - 1;
        loop {
            let choice = self.rng.below(16);
            if self.cheap > 0 && choice == 14 {
                continue;
            }
            match choice {
                0..=2 => {
                    let op = *self.rng.pick(&["<", "<=", ">", ">=", "==", "!="]);
                    return format!("({} {op} {})", self.int(d1), self.int(d1));
                }
                3 => return format!("(not {})", self.boolean(d1)),
                4 => {
                    let op = *self.rng.pick(&["and", "or"]);
                    return format!("({} {op} {})", self.boolean(d1), self.boolean(d1));
                }
                5 => return format!("{}.contains({})", Self::recv(self.list(d1)), self.int(d1)),
                6 => {
                    let m = *self.rng.pick(&["starts_with", "contains", "ends_with"]);
                    let lit = *self.rng.pick(STRINGS);
                    return format!("{}.{m}(\"{lit}\")", Self::recv(self.string(d1)));
                }
                7 => return format!("{}.has_key(\"{}\")", Self::recv(self.dict(d1)), self.rng.pick(KEYS)),
                8 => return format!("{}.contains({})", Self::recv(self.set(d1)), self.int(d1)),
                9 => return format!("({} == nil)", self.opt(d1)),
                10 => {
                    let m = *self.rng.pick(&["any", "all", "none"]);
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.boolean(d1));
                    return format!("({xs}.{m} {b})");
                }
                11 => {
                    if self.funcs.iter().any(|f| f.ret == Ty::Res) {
                        return format!("{}.is_ok()", self.res(d1));
                    }
                }
                12 => return format!("({} == {})", self.string(d1), self.string(d1)),
                13 => return format!("({} == {})", self.list(d1), self.list(d1)),
                14 => {
                    if let Some(call) = self.call_returning(Ty::Bool, d1) {
                        return call;
                    }
                }
                15 => return format!("{}.is_empty()", Self::recv(self.list(d1))),
                _ => {}
            }
        }
    }

    fn str_lit(&mut self) -> String {
        format!("\"{}\"", self.rng.pick(STRINGS))
    }

    fn string(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(20) {
            if let Some(v) = self.var_of(Ty::Str) {
                if self.rng.chance(60) {
                    return v;
                }
            }
            return if self.rng.chance(70) {
                self.str_lit()
            } else {
                format!("{}.to_s()", Self::recv(self.int_leaf()))
            };
        }
        let d1 = d - 1;
        loop {
            let choice = self.rng.below(17);
            if self.cheap > 0 && matches!(choice, 9 | 13) {
                continue;
            }
            match choice {
                0 | 1 => {
                    let a = self.int(d1);
                    let b = self.string(d1);
                    return format!("\"s#{{{a}}}-#{{{b}}}\"");
                }
                2 => return format!("({} + {})", self.string(d1), self.string(d1)),
                3 => {
                    let m = *self.rng.pick(&["upcase", "downcase", "reverse", "capitalize", "strip", "swapcase"]);
                    return format!("{}.{m}()", Self::recv(self.string(d1)));
                }
                4 => {
                    let (a, b) = (*self.rng.pick(STRINGS), *self.rng.pick(STRINGS));
                    return format!("{}.replace(\"{a}\", \"{b}\")", Self::recv(self.string(d1)));
                }
                5 => {
                    let (s, l) = (self.small(), self.rng.range(0, 10));
                    return format!("{}.slice({s}, {l})", Self::recv(self.string(d1)));
                }
                6 => {
                    let xs = Self::recv(self.list(d1));
                    let sep = *self.rng.pick(&[",", "", " | "]);
                    return format!("({xs}.map {{ |x| x.to_s() }}).join(\"{sep}\")");
                }
                7 => {
                    return format!(
                        "(if {} {{ {} }} else {{ {} }})",
                        self.boolean(d1),
                        self.string(d1),
                        self.string(d1)
                    )
                }
                8 => return format!("({} * {})", self.string(d1), self.rng.range(0, 3)),
                9 => {
                    if let Some(call) = self.method_returning(Ty::Str, d1) {
                        return call;
                    }
                }
                10 => {
                    let t = *self.rng.pick(&[Ty::List, Ty::Dict, Ty::Set, Ty::Opt, Ty::Bool]);
                    return format!("inspect({})", self.expr(t, d1));
                }
                11 => return format!("{}.keys().join(\"/\")", Self::recv(self.dict(d1))),
                12 => {
                    if self.funcs.iter().any(|f| f.ret == Ty::Res) {
                        let r = self.res(d1);
                        let (v, e) = (self.fresh("p"), self.fresh("p"));
                        return format!("(match {r} {{ Ok({v}) => \"ok #{{{v}}}\", Err({e}) => \"err \" + {e} }})");
                    }
                }
                13 => {
                    if let Some(call) = self.call_returning(Ty::Str, d1) {
                        return call;
                    }
                }
                14 if !self.enums.is_empty() => {
                    let e = self.rng.below(self.enums.len());
                    return format!("\"#{{{}}}\"", self.enum_val(e, d1));
                }
                15 => {
                    let w = self.rng.range(0, 12);
                    let m = *self.rng.pick(&["center", "ljust", "rjust"]);
                    return format!("{}.{m}({w}, \"*\")", Self::recv(self.string(d1)));
                }
                16 => {
                    let sb = self.fresh("p");
                    let xs = Self::recv(self.list(d1));
                    let i = self.fresh("p");
                    return format!(
                        "(fn() -> String {{ {sb} = StringBuilder.new(); for {i} in {xs} {{ {sb}.push_value({i}) }}; {sb}.to_s() }})()"
                    );
                }
                _ => {}
            }
        }
    }

    fn list_lit(&mut self, d: u32) -> String {
        let n = self.rng.range(1, 5);
        let items: Vec<String> =
            (0..n).map(|_| if d == 0 || self.rng.chance(50) { self.int_lit() } else { self.int(d - 1) }).collect();
        format!("[{}]", items.join(", "))
    }

    fn list(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(20) {
            if let Some(v) = self.var_of(Ty::List) {
                if self.rng.chance(60) {
                    return v;
                }
            }
            return match self.rng.below(3) {
                0 => format!("(0...{}).to_list()", self.rng.range(0, 12)),
                _ => self.list_lit(0),
            };
        }
        let d1 = d - 1;
        loop {
            let choice = self.rng.below(21);
            if self.cheap > 0 && matches!(choice, 10 | 11 | 14 | 15) {
                continue;
            }
            match choice {
                0 | 1 => return self.list_lit(d),
                2 | 3 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.int(d1));
                    return format!("({xs}.map {b})");
                }
                4 => {
                    let xs = Self::recv(self.list(d1));
                    let m = *self.rng.pick(&["select", "reject", "take_while", "drop_while"]);
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.boolean(d1));
                    return format!("({xs}.{m} {b})");
                }
                5 => {
                    let m = *self.rng.pick(&["sort()", "reverse()", "uniq()", "rotate()", "dup()", "sort().reverse()"]);
                    return format!("{}.{m}", Self::recv(self.list(d1)));
                }
                6 => {
                    let m = *self.rng.pick(&["take", "drop"]);
                    return format!("{}.{m}({})", Self::recv(self.list(d1)), self.small());
                }
                7 => return format!("({} + {})", self.list(d1), self.list(d1)),
                8 => return format!("{}.values()", Self::recv(self.dict(d1))),
                9 => return format!("{}.to_list()", Self::recv(self.set(d1))),
                10 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Thread, &[Ty::Int], |g, _| g.int(d1));
                    return format!("({xs}.par_map {b})");
                }
                11 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Thread, &[Ty::Int], |g, _| g.boolean(d1));
                    return format!("({xs}.par_select {b})");
                }
                12 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, ps| format!("[{}, {}]", ps[0], g.int(d1)));
                    return format!("({xs}.flat_map {b})");
                }
                13 => {
                    if let Some(v) = self.var_of(Ty::Frozen) {
                        return format!("{v}.dup()");
                    }
                }
                14 => {
                    if let Some(call) = self.call_returning(Ty::List, d1) {
                        return call;
                    }
                }
                15 => {
                    if let Some(call) = self.method_returning(Ty::List, d1) {
                        return call;
                    }
                }
                16 => {
                    let xs = Self::recv(self.list(d1));
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.int(d1));
                    return format!("({xs}.sort_by {b})");
                }
                17 => return format!("{}.abs().digits()", Self::recv(self.int(d1))),
                18 => return format!("{}.bytes()", Self::recv(self.string(d1))),
                19 => {
                    let xs = Self::recv(self.list(d1));
                    return format!("({xs}.chunk({}).map {{ |c| c.sum() }})", self.rng.range(1, 4));
                }
                20 => {
                    let (a, b) = (self.rng.range(-3, 5), self.rng.range(0, 9));
                    let op = *self.rng.pick(&["..", "..."]);
                    return format!("({a}{op}{b}).to_list()");
                }
                _ => {}
            }
        }
    }

    fn key_expr(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(50) {
            format!("\"{}\"", self.rng.pick(KEYS))
        } else {
            format!("\"k#{{{} % 5}}\"", self.int(d - 1))
        }
    }

    fn dict(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(25) {
            if let Some(v) = self.var_of(Ty::Dict) {
                if self.rng.chance(60) {
                    return v;
                }
            }
            let n = self.rng.range(1, 4);
            let items: Vec<String> =
                (0..n).map(|_| format!("\"{}\": {}", self.rng.pick(KEYS), self.int_lit())).collect();
            return format!("[{}]", items.join(", "));
        }
        let d1 = d - 1;
        match self.rng.below(5) {
            0 => {
                let xs = Self::recv(self.list(d1));
                format!("({xs}.tally_by {{ |x| \"k#{{x % 5}}\" }})")
            }
            1 => format!("{}.merge({})", Self::recv(self.dict(d1)), self.dict(d1)),
            2 => {
                let dd = Self::recv(self.dict(d1));
                let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.int(d1));
                format!("({dd}.map_values {b})")
            }
            3 => {
                let dd = Self::recv(self.dict(d1));
                let b = self.block(ScopeKind::Normal, &[Ty::Str, Ty::Int], |g, _| g.boolean(d1));
                format!("({dd}.select_entries {b})")
            }
            _ => {
                let n = self.rng.range(1, 4);
                let items: Vec<String> = (0..n).map(|_| format!("({}): {}", self.key_expr(d1), self.int(d1))).collect();
                format!("[{}]", items.join(", "))
            }
        }
    }

    fn set(&mut self, d: u32) -> String {
        if d == 0 || self.rng.chance(25) {
            if let Some(v) = self.var_of(Ty::Set) {
                if self.rng.chance(60) {
                    return v;
                }
            }
            return format!("Set.from({})", self.list_lit(0));
        }
        let d1 = d - 1;
        match self.rng.below(4) {
            0 => format!("Set.from({})", self.list(d1)),
            1 => format!("{}.to_set()", Self::recv(self.list(d1))),
            2 => {
                let op = *self.rng.pick(&["|", "&", "-"]);
                format!("({} {op} {})", self.set(d1), self.set(d1))
            }
            _ => {
                let m = *self.rng.pick(&["union", "intersection", "difference"]);
                format!("{}.{m}({})", Self::recv(self.set(d1)), self.set(d1))
            }
        }
    }

    fn opt(&mut self, d: u32) -> String {
        if let Some(v) = self.var_of(Ty::Opt) {
            if d == 0 || self.rng.chance(30) {
                return v;
            }
        }
        let d1 = d.saturating_sub(1);
        match self.rng.below(7) {
            0 => format!("{}.first()", Self::recv(self.list(d1))),
            1 => format!("{}.get({} % 8)", Self::recv(self.list(d1)), self.int(d1)),
            2 => format!("{}[{}]", Self::recv(self.dict(d1)), self.key_expr(d1)),
            3 => {
                let xs = Self::recv(self.list(d1));
                let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.boolean(d1));
                format!("({xs}.find {b})")
            }
            4 => format!("{}.max()", Self::recv(self.list(d1))),
            5 => format!("{}.to_i()", Self::recv(self.string(d1))),
            _ => {
                if let Some(call) = if self.cheap > 0 { None } else { self.call_returning(Ty::Opt, d1) } {
                    call
                } else {
                    format!("{}.last()", Self::recv(self.list(d1)))
                }
            }
        }
    }

    fn obj(&mut self, c: usize, d: u32) -> String {
        if let Some(v) = self.var_of(Ty::Obj(c)) {
            if d == 0 || self.rng.chance(60) {
                return v;
            }
        }
        // Possibly construct a subclass instead.
        let subs: Vec<usize> = (0..self.classes.len())
            .filter(|&k| self.is_subclass(k, c) && (k == c || Some(k) != self.defining))
            .collect();
        let k = *self.rng.pick(&subs);
        let d1 = d.saturating_sub(1);
        let tys: Vec<Ty> = self.classes[k].fields.iter().map(|f| f.1).collect();
        let args: Vec<String> = tys.into_iter().map(|t| self.expr(t, d1)).collect();
        format!("{}.new({})", self.classes[k].name, args.join(", "))
    }

    fn enum_val(&mut self, e: usize, d: u32) -> String {
        if let Some(v) = self.var_of(Ty::Enum(e)) {
            if d == 0 || self.rng.chance(50) {
                return v;
            }
        }
        let d1 = d.saturating_sub(1);
        let vi = self.rng.below(self.enums[e].variants.len());
        let (vn, tys) = self.enums[e].variants[vi].clone();
        let name = self.enums[e].name.clone();
        if tys.is_empty() {
            format!("{name}.{vn}")
        } else {
            let args: Vec<String> = tys.into_iter().map(|t| self.expr(t, d1)).collect();
            format!("{name}.{vn}({})", args.join(", "))
        }
    }

    fn enum_match(&mut self, e: usize, d: u32) -> String {
        let subject = self.enum_val(e, d);
        let variants = self.enums[e].variants.clone();
        let mut arms = vec![];
        let wildcard = variants.len() > 1 && self.rng.chance(25);
        let n = if wildcard { variants.len() - 1 } else { variants.len() };
        for (vn, tys) in variants.into_iter().take(n) {
            self.push_scope(ScopeKind::Normal);
            let mut names = vec![];
            for t in &tys {
                let p = self.fresh("p");
                self.bind(&p, *t, false);
                names.push(p);
            }
            let body = self.int(d);
            self.pop_scope();
            if names.is_empty() {
                arms.push(format!(".{vn} => {body}"));
            } else {
                arms.push(format!(".{vn}({}) => {body}", names.join(", ")));
            }
        }
        if wildcard {
            arms.push(format!("_ => {}", self.int_leaf()));
        }
        format!("(match {subject} {{ {} }})", arms.join(", "))
    }

    fn res(&mut self, d: u32) -> String {
        if let Some(v) = self.var_of(Ty::Res) {
            if d == 0 || self.rng.chance(40) {
                return v;
            }
        }
        self.call_returning(Ty::Res, d).expect("res requires a Result function")
    }

    fn call_returning(&mut self, ty: Ty, d: u32) -> Option<String> {
        let idx: Vec<usize> = (0..self.funcs.len()).filter(|&i| self.funcs[i].ret == ty).collect();
        if idx.is_empty() {
            return None;
        }
        let i = *self.rng.pick(&idx);
        let params = self.funcs[i].params.clone();
        let args: Vec<String> = params.into_iter().map(|t| self.expr(t, d)).collect();
        Some(format!("{}({})", self.funcs[i].name, args.join(", ")))
    }

    fn methods_of(&self, c: usize) -> Vec<(String, Vec<Ty>, Option<Ty>)> {
        let mut out: Vec<(String, Vec<Ty>, Option<Ty>)> = vec![];
        let mut k = Some(c);
        while let Some(i) = k {
            for m in &self.classes[i].methods {
                if !out.iter().any(|o| o.0 == m.name) {
                    out.push((m.name.clone(), m.params.clone(), m.ret));
                }
            }
            k = self.classes[i].parent;
        }
        out
    }

    fn method_returning(&mut self, ty: Ty, d: u32) -> Option<String> {
        if self.classes.is_empty() {
            return None;
        }
        let c = self.rng.below(self.classes.len());
        if Some(c) == self.defining {
            return None;
        }
        let ms: Vec<_> = self.methods_of(c).into_iter().filter(|m| m.2 == Some(ty)).collect();
        if ms.is_empty() {
            return None;
        }
        let (name, params, _) = self.rng.pick(&ms).clone();
        let recv = self.obj(c, d);
        let args: Vec<String> = params.into_iter().map(|t| self.expr(t, d)).collect();
        Some(format!("{recv}.{name}({})", args.join(", ")))
    }

    // ---------------------------------------------------------------- statements

    fn printable_types(&self) -> Vec<Ty> {
        let mut ts = vec![Ty::Int, Ty::Int, Ty::Bool, Ty::Str, Ty::Str, Ty::List, Ty::List, Ty::Dict, Ty::Set, Ty::Opt];
        for i in 0..self.classes.len() {
            ts.push(Ty::Obj(i));
        }
        for i in 0..self.enums.len() {
            ts.push(Ty::Enum(i));
        }
        if self.funcs.iter().any(|f| f.ret == Ty::Res) {
            ts.push(Ty::Res);
        }
        ts
    }

    fn decl_types(&self) -> Vec<Ty> {
        let mut ts = self.printable_types();
        ts.push(Ty::Fun);
        ts.push(Ty::Frozen);
        ts
    }

    fn block_stmts(&mut self, kind: ScopeKind, n: usize) {
        self.ind += 1;
        self.block_depth += 1;
        self.push_scope(kind);
        for _ in 0..n {
            self.stmt();
        }
        self.pop_scope();
        self.block_depth -= 1;
        self.ind -= 1;
    }

    fn inner_count(&mut self) -> usize {
        let max = 4usize.saturating_sub(self.block_depth).max(1);
        1 + self.rng.below(max)
    }

    fn stmt(&mut self) {
        self.budget -= 1;
        let nested_ok = self.block_depth < 3 && self.budget > 0;
        let loop_ok = nested_ok && self.loop_depth < 2;
        loop {
            match self.rng.below(24) {
                0..=4 => return self.decl(),
                5 | 6 => {
                    if self.assign() {
                        return;
                    }
                }
                7 | 8 => {
                    if self.mutate() {
                        return;
                    }
                }
                9..=11 if self.quiet == 0 => {
                    let ts = self.printable_types();
                    let t = *self.rng.pick(&ts);
                    let e = self.expr(t, 3);
                    return emit!(self, "puts({e})");
                }
                12 | 13 if nested_ok => {
                    let c = self.boolean(2);
                    emit!(self, "if {c} {{");
                    let n = self.inner_count();
                    self.block_stmts(ScopeKind::Normal, n);
                    if self.rng.chance(50) {
                        self.line("} else {");
                        let n = self.inner_count();
                        self.block_stmts(ScopeKind::Normal, n);
                    }
                    return self.line("}");
                }
                14 if loop_ok => {
                    let i = self.fresh("i");
                    let (a, b) = (self.rng.range(-2, 3), self.rng.range(0, 12));
                    let op = *self.rng.pick(&["..", "..."]);
                    emit!(self, "for {i} in ({a}{op}{b}) {{");
                    return self.loop_body(&[(i, Ty::Int)], true);
                }
                15 if loop_ok => {
                    let x = self.fresh("x");
                    let xs = Self::recv(self.list(2));
                    emit!(self, "for {x} in {xs}.take({}) {{", self.rng.range(0, 10));
                    return self.loop_body(&[(x, Ty::Int)], true);
                }
                16 if loop_ok => {
                    let w = self.fresh("w");
                    emit!(self, "{w} = 0");
                    emit!(self, "while {w} < {} {{", self.rng.range(0, 10));
                    self.ind += 1;
                    emit!(self, "{w} += 1");
                    self.ind -= 1;
                    return self.loop_body(&[], true);
                }
                17 if loop_ok => {
                    let k = self.fresh("w");
                    emit!(self, "{k} = 0");
                    self.line("loop {");
                    self.ind += 1;
                    emit!(self, "{k} += 1");
                    emit!(self, "if {k} > {} {{ break }}", self.rng.range(0, 8));
                    self.ind -= 1;
                    return self.loop_body(&[], true);
                }
                18 if loop_ok => {
                    let x = self.fresh("x");
                    let xs = Self::recv(self.list(2));
                    let m = if self.rng.chance(50) { "each" } else { "each_with_index" };
                    if m == "each" {
                        emit!(self, "{xs}.take(8).each {{ |{x}|");
                        return self.loop_body(&[(x, Ty::Int)], false);
                    }
                    let j = self.fresh("x");
                    emit!(self, "{xs}.take(8).each_with_index {{ |{x}, {j}|");
                    return self.loop_body(&[(x, Ty::Int), (j, Ty::Int)], false);
                }
                19 if nested_ok && !self.enums.is_empty() => {
                    let e = self.rng.below(self.enums.len());
                    let subject = self.enum_val(e, 2);
                    emit!(self, "match {subject} {{");
                    self.ind += 1;
                    for (vn, tys) in self.enums[e].variants.clone() {
                        let names: Vec<String> = tys.iter().map(|_| self.fresh("p")).collect();
                        if names.is_empty() {
                            emit!(self, ".{vn} => {{");
                        } else {
                            emit!(self, ".{vn}({}) => {{", names.join(", "));
                        }
                        self.ind += 1;
                        self.block_depth += 1;
                        self.push_scope(ScopeKind::Normal);
                        for (n, t) in names.iter().zip(&tys) {
                            self.bind(n, *t, false);
                        }
                        let k = self.inner_count();
                        for _ in 0..k {
                            self.stmt();
                        }
                        self.line("nil");
                        self.pop_scope();
                        self.block_depth -= 1;
                        self.ind -= 1;
                        self.line("}");
                    }
                    self.ind -= 1;
                    return self.line("}");
                }
                20 if nested_ok => {
                    let p = self.fresh("p");
                    let o = self.opt(2);
                    emit!(self, "if let {p} = {o} {{");
                    self.ind += 1;
                    self.block_depth += 1;
                    self.push_scope(ScopeKind::Normal);
                    self.bind(&p, Ty::Int, false);
                    let n = self.inner_count();
                    for _ in 0..n {
                        self.stmt();
                    }
                    self.pop_scope();
                    self.block_depth -= 1;
                    self.ind -= 1;
                    self.line("} else {");
                    let n = self.inner_count();
                    self.block_stmts(ScopeKind::Normal, n);
                    return self.line("}");
                }
                21 => {
                    if self.mutate_obj() {
                        return;
                    }
                }
                22 if self.rng.chance(30) => return self.line("gc()"),
                23 if self.fiber_depth > 0 => return self.line("Fiber.yield()"),
                _ => {}
            }
        }
    }

    fn loop_body(&mut self, binds: &[(String, Ty)], can_break: bool) {
        self.ind += 1;
        self.block_depth += 1;
        self.loop_depth += 1;
        self.push_scope(ScopeKind::Normal);
        for (n, t) in binds {
            self.bind(n, *t, false);
        }
        let n = self.inner_count();
        for _ in 0..n {
            self.stmt();
        }
        if can_break && self.rng.chance(15) {
            let c = self.boolean(1);
            let kw = if self.rng.chance(50) { "break" } else { "continue" };
            // `continue` would skip a while loop's counter only if placed before it; the counter
            // increments first, so both are safe.
            emit!(self, "if {c} {{ {kw} }}");
        }
        self.pop_scope();
        self.loop_depth -= 1;
        self.block_depth -= 1;
        self.ind -= 1;
        self.line("}");
    }

    fn decl(&mut self) {
        let ts = self.decl_types();
        let t = *self.rng.pick(&ts);
        let name = self.fresh("v");
        let annotate = self.rng.chance(30) || matches!(t, Ty::Opt | Ty::Res);
        let rhs = if self.rng.chance(15) && matches!(t, Ty::List | Ty::Dict) {
            if t == Ty::List {
                "[]".to_string()
            } else {
                "[:]".to_string()
            }
        } else {
            self.expr(t, 3)
        };
        let annotate = annotate || rhs == "[]" || rhs == "[:]";
        if annotate {
            let a = self.annot(t);
            emit!(self, "{name}: {a} = {rhs}");
        } else {
            emit!(self, "{name} = {rhs}");
        }
        self.bind(&name, t, true);
    }

    fn assign(&mut self) -> bool {
        let vs: Vec<Var> = self
            .visible()
            .into_iter()
            .filter(|v| v.assignable && matches!(v.ty, Ty::Int | Ty::Str | Ty::List | Ty::Bool | Ty::Opt | Ty::Set))
            .collect();
        if vs.is_empty() {
            return false;
        }
        let v = self.rng.pick(&vs).clone();
        let n = &v.name;
        let s = match v.ty {
            Ty::Int => match self.rng.below(3) {
                0 => format!("{n} += {}", self.int_lit()),
                1 => format!("{n} -= {}", self.rng.range(0, 9)),
                _ => format!("{n} = {} % 1000", Self::recv(self.int(3))),
            },
            Ty::Str => {
                if self.rng.chance(40) {
                    format!("{n} += {}", self.str_lit())
                } else {
                    format!("{n} = {}.slice(0, 16)", Self::recv(self.string(3)))
                }
            }
            Ty::List => format!("{n} = {}.take(8)", Self::recv(self.list(3))),
            Ty::Bool => format!("{n} = {}", self.boolean(3)),
            Ty::Opt => {
                if self.rng.chance(30) {
                    format!("{n} = nil")
                } else if self.rng.chance(50) {
                    format!("{n} ??= {}", self.int(2))
                } else {
                    format!("{n} = {}", self.opt(3))
                }
            }
            Ty::Set => format!("{n} = {}", self.set(2)),
            _ => unreachable!(),
        };
        self.line(&s);
        true
    }

    fn mutate(&mut self) -> bool {
        let vs: Vec<Var> =
            self.visible().into_iter().filter(|v| v.mutable && matches!(v.ty, Ty::List | Ty::Dict | Ty::Set)).collect();
        if vs.is_empty() {
            return false;
        }
        let v = self.rng.pick(&vs).clone();
        let n = &v.name;
        let s = match v.ty {
            Ty::List => match self.rng.below(10) {
                0..=2 => format!("{n}.push({})", self.int(2)),
                3 => format!("{n}.pop()"),
                4 => {
                    let i = self.rng.range(0, 4);
                    format!("if {n}.length > {i} {{ {n}[{i}] = {} }}", self.int(2))
                }
                5 => format!("{n}.insert(0, {})", self.int(2)),
                6 => format!("{n}.{}()", self.rng.pick(&["sort_in_place", "reverse_in_place", "shift"])),
                7 => {
                    let b = self.block(ScopeKind::Normal, &[Ty::Int], |g, _| g.boolean(1));
                    format!("{n}.delete_if {b}")
                }
                8 => format!("{n}.concat({}.take(4))", Self::recv(self.list(2))),
                _ => format!("{n}.unshift({})", self.int(2)),
            },
            Ty::Dict => match self.rng.below(4) {
                0 | 1 => format!("{n}[{}] = {}", self.key_expr(2), self.int(2)),
                2 => format!("{n}.delete({})", self.key_expr(1)),
                _ => format!("{n}.update({})", self.dict(1)),
            },
            Ty::Set => match self.rng.below(3) {
                0 | 1 => format!("{n}.add({})", self.int(2)),
                _ => format!("{n}.remove({})", self.int(2)),
            },
            _ => unreachable!(),
        };
        self.line(&s);
        true
    }

    fn mutate_obj(&mut self) -> bool {
        let vs: Vec<(String, usize)> = self
            .visible()
            .into_iter()
            .filter_map(|v| if let Ty::Obj(c) = v.ty { Some((v.name, c)) } else { None })
            .collect();
        if vs.is_empty() {
            return false;
        }
        let (n, c) = self.rng.pick(&vs).clone();
        if self.rng.chance(60) {
            let ms: Vec<_> = self.methods_of(c).into_iter().filter(|m| m.2.is_none()).collect();
            if let Some((mname, params, _)) = ms.first().cloned() {
                let args: Vec<String> = params.into_iter().map(|t| self.expr(t, 2)).collect();
                emit!(self, "{n}.{mname}({})", args.join(", "));
                return true;
            }
        }
        let fields = self.classes[c].fields.clone();
        let (f, t) = self.rng.pick(&fields).clone();
        let rhs = match t {
            Ty::Int => format!("{} % 1000", Self::recv(self.int(2))),
            Ty::Str => format!("{}.slice(0, 12)", Self::recv(self.string(2))),
            Ty::List => format!("{}.take(6)", Self::recv(self.list(2))),
            _ => unreachable!(),
        };
        emit!(self, "{n}.{f} = {rhs}");
        true
    }

    // ---------------------------------------------------------------- declarations

    fn gen_enum(&mut self) {
        let name = self.fresh("E");
        let nv = self.rng.range(2, 4) as usize;
        let mut variants = vec![];
        for k in 0..nv {
            let np = self.rng.range(0, 2) as usize;
            let tys: Vec<Ty> = (0..np).map(|_| if self.rng.chance(60) { Ty::Int } else { Ty::Str }).collect();
            variants.push((format!("V{k}"), tys));
        }
        emit!(self, "enum {name} {{");
        self.ind += 1;
        for (vn, tys) in &variants {
            if tys.is_empty() {
                self.line(vn);
            } else {
                let ts: Vec<&str> = tys.iter().map(|t| if *t == Ty::Int { "Int" } else { "String" }).collect();
                emit!(self, "{vn}({})", ts.join(", "));
            }
        }
        self.line("");
        self.enums.push(EnumDef { name: name.clone(), variants: variants.clone() });
        self.line("fn score(self) -> Int {");
        self.ind += 1;
        self.line("match self {");
        self.ind += 1;
        // Bodies may not use the enum itself recursively (score is being defined).
        let saved = std::mem::take(&mut self.enums);
        for (vn, tys) in &variants {
            self.push_scope(ScopeKind::Normal);
            let names: Vec<String> = tys.iter().map(|_| self.fresh("p")).collect();
            for (n, t) in names.iter().zip(tys) {
                self.bind(n, *t, false);
            }
            let body = self.int(2);
            self.pop_scope();
            if names.is_empty() {
                emit!(self, ".{vn} => {body}");
            } else {
                emit!(self, ".{vn}({}) => {body}", names.join(", "));
            }
        }
        self.enums = saved;
        self.ind -= 1;
        self.line("}");
        self.ind -= 1;
        self.line("}");
        self.ind -= 1;
        self.line("}");
    }

    fn gen_tree(&mut self) {
        let t = self.fresh("Tree");
        let build = self.fresh("build");
        let sum = self.fresh("tsum");
        let (a, b) = (self.rng.range(1, 5), self.rng.range(1, 5));
        emit!(self, "enum {t} {{");
        self.line("  Leaf(Int)");
        emit!(self, "  Node({t}, {t})");
        self.line("}");
        emit!(self, "fn {build}(d: Int, s: Int) -> {t} {{");
        emit!(self, "  if d <= 0 {{ {t}.Leaf(s % 97) }} else {{ {t}.Node({build}(d - 1, s * {a} + 1), {build}(d - 1, s + {b})) }}");
        self.line("}");
        emit!(self, "fn {sum}(t: {t}) -> Int {{");
        emit!(self, "  match t {{ .Leaf(v) => v, .Node(l, r) => {sum}(l) * 3 % 10007 + {sum}(r) }}");
        self.line("}");
        self.trees.push((t, build, sum));
    }

    fn gen_rec(&mut self) {
        let r = self.fresh("rec");
        let (a, b) = (self.rng.range(1, 9), self.rng.range(0, 99));
        emit!(self, "fn {r}(n: Int, acc: Int) -> Int {{");
        if self.rng.chance(50) {
            emit!(self, "  if n <= 0 {{ acc }} else {{ {r}(n - 1, (acc * {a} + {b} + n) % 1000003) }}");
        } else {
            // Non-tail recursion keeps every frame live.
            emit!(self, "  if n <= 0 {{ acc % 1000 }} else {{ ({r}(n - 1, acc + {b}) + n * {a}) % 1000003 }}");
        }
        self.line("}");
        self.recs.push(r);
    }

    fn gen_results(&mut self) {
        let div = self.fresh("checked");
        let chain = self.fresh("chain");
        emit!(self, "fn {div}(a: Int, b: Int) -> Result[Int, String] {{");
        emit!(self, "  if b == 0 {{ return Err(\"zero divisor for #{{a}}\") }}");
        emit!(self, "  if a < -{} {{ Err(\"too small\") }} else {{ Ok(a / b) }}", self.rng.range(0, 100));
        self.line("}");
        emit!(self, "fn {chain}(a: Int, b: Int, c: Int) -> Result[Int, String] {{");
        emit!(self, "  x = {div}(a, b)?");
        emit!(self, "  y = {div}(x + {}, c % 4)?", self.rng.range(0, 9));
        self.line("  Ok(x + y)");
        self.line("}");
        self.funcs.push(Func { name: div, params: vec![Ty::Int, Ty::Int], ret: Ty::Res });
        self.funcs.push(Func { name: chain, params: vec![Ty::Int, Ty::Int, Ty::Int], ret: Ty::Res });
        // An optional-returning function using `?` on optionals.
        let f = self.fresh("firsts");
        emit!(self, "fn {f}(xs: List[Int], ys: List[Int]) -> Int? {{");
        self.line("  a = xs.first()?");
        self.line("  b = ys.last()?");
        self.line("  a * 2 + b");
        self.line("}");
        self.funcs.push(Func { name: f, params: vec![Ty::List, Ty::List], ret: Ty::Opt });
    }

    fn gen_func(&mut self) {
        let name = self.fresh("f");
        let np = self.rng.range(0, 3) as usize;
        let ptys: Vec<Ty> =
            (0..np).map(|_| *self.rng.pick(&[Ty::Int, Ty::Int, Ty::Str, Ty::List, Ty::Bool, Ty::Opt])).collect();
        let ret = *self.rng.pick(&[Ty::Int, Ty::Int, Ty::Str, Ty::List, Ty::Bool, Ty::Opt]);
        let pnames: Vec<String> = ptys.iter().map(|_| self.fresh("a")).collect();
        let sig: Vec<String> = pnames.iter().zip(&ptys).map(|(n, t)| format!("{n}: {}", self.annot(*t))).collect();
        let pubk = if self.rng.chance(20) { "pub " } else { "" };
        emit!(self, "{pubk}fn {name}({}) -> {} {{", sig.join(", "), self.annot(ret));
        let saved = std::mem::take(&mut self.scopes);
        self.push_scope(ScopeKind::Normal);
        for (n, t) in pnames.iter().zip(&ptys) {
            self.bind(n, *t, false);
        }
        self.quiet += 1;
        self.ind += 1;
        self.budget = 8;
        let n = self.rng.range(0, 3);
        for _ in 0..n {
            self.stmt();
        }
        let e = self.expr(ret, 3);
        self.line(&e);
        self.ind -= 1;
        self.quiet -= 1;
        self.scopes = saved;
        self.line("}");
        self.funcs.push(Func { name, params: ptys, ret });
    }

    fn gen_class(&mut self) {
        let name = self.fresh("K");
        let parent = if !self.classes.is_empty() && self.rng.chance(35) {
            Some(self.rng.below(self.classes.len()))
        } else {
            None
        };
        let mut fields = parent.map(|p| self.classes[p].fields.clone()).unwrap_or_default();
        let nf = self.rng.range(1, 3);
        let mut own = vec![];
        for _ in 0..nf {
            let f = self.fresh("fld");
            let t = *self.rng.pick(&[Ty::Int, Ty::Int, Ty::Str, Ty::List]);
            fields.push((f.clone(), t));
            own.push((f, t));
        }
        let idx = self.classes.len();
        self.classes.push(Class { name: name.clone(), parent, fields: fields.clone(), methods: vec![] });
        self.defining = Some(idx);
        match parent {
            Some(p) => emit!(self, "class {name} < {} {{", self.classes[p].name),
            None => emit!(self, "class {name} {{"),
        }
        self.ind += 1;
        for (f, t) in &own {
            let a = self.annot(*t);
            emit!(self, "{f}: {a}");
        }
        // Methods see `self.field` as variables.
        let saved = std::mem::take(&mut self.scopes);
        let mut methods: Vec<Method> = vec![];
        self.quiet += 1;
        // Overrides of an inherited Int method, calling super.
        if let Some(p) = parent {
            let inherited: Vec<_> =
                self.methods_of(p).into_iter().filter(|m| m.2 == Some(Ty::Int) && m.1 == vec![Ty::Int]).collect();
            if let Some((mname, _, _)) = inherited.first().cloned() {
                self.push_scope(ScopeKind::Normal);
                for (f, t) in &fields {
                    self.bind(&format!("self.{f}"), *t, false);
                }
                self.bind("a", Ty::Int, false);
                let body = self.int(2);
                self.pop_scope();
                emit!(self, "fn {mname}(self, a: Int) -> Int {{ (super.{mname}(a) + {body}) % 100000 }}");
            }
        }
        let nm = self.rng.range(1, 3);
        for _ in 0..nm {
            let mname = self.fresh("m");
            self.push_scope(ScopeKind::Normal);
            for (f, t) in &fields {
                self.bind(&format!("self.{f}"), *t, false);
            }
            match self.rng.below(4) {
                0 | 1 => {
                    self.bind("a", Ty::Int, false);
                    let body = self.int(3);
                    emit!(self, "fn {mname}(self, a: Int) -> Int {{ {body} }}");
                    methods.push(Method { name: mname, params: vec![Ty::Int], ret: Some(Ty::Int) });
                }
                2 => {
                    let body = self.string(3);
                    emit!(self, "fn {mname}(self) -> String {{ {body} }}");
                    methods.push(Method { name: mname, params: vec![], ret: Some(Ty::Str) });
                }
                _ => {
                    self.bind("a", Ty::Int, false);
                    let (f, t) = self.rng.pick(&own).clone();
                    let body = match t {
                        Ty::Int => format!("self.{f} = (self.{f} + a) % 1000"),
                        Ty::Str => format!("self.{f} = (self.{f} + a.to_s()).slice(0, 10)"),
                        _ => format!("self.{f}.push(a); if self.{f}.length > 6 {{ self.{f}.shift() }}"),
                    };
                    emit!(self, "fn {mname}(self, a: Int) {{");
                    emit!(self, "  {body}");
                    self.line("}");
                    methods.push(Method { name: mname, params: vec![Ty::Int], ret: None });
                }
            }
            self.pop_scope();
        }
        // A list-returning method.
        if self.rng.chance(40) {
            let mname = self.fresh("m");
            self.push_scope(ScopeKind::Normal);
            for (f, t) in &fields {
                self.bind(&format!("self.{f}"), *t, false);
            }
            let body = self.list(2);
            self.pop_scope();
            emit!(self, "fn {mname}(self) -> List[Int] {{ {body} }}");
            methods.push(Method { name: mname, params: vec![], ret: Some(Ty::List) });
        }
        self.quiet -= 1;
        self.scopes = saved;
        self.ind -= 1;
        self.line("}");
        self.classes[idx].methods = methods;
        self.defining = None;
    }

    // ---------------------------------------------------------------- concurrency templates

    /// Statements plus a final expression inside a thread or fiber body (no printing).
    fn thread_body(&mut self, ret: Ty) {
        self.ind += 1;
        let n = self.rng.range(0, 3);
        for _ in 0..n {
            self.stmt();
        }
        let e = self.expr(ret, 2);
        self.line(&e);
        self.ind -= 1;
    }

    fn tpl_spawn(&mut self) {
        let t = self.fresh("t");
        let ret = *self.rng.pick(&[Ty::Int, Ty::Str, Ty::List]);
        emit!(self, "{t} = spawn {{");
        self.push_scope(ScopeKind::Thread);
        self.quiet += 1;
        self.block_depth += 1;
        self.thread_body(ret);
        self.block_depth -= 1;
        self.quiet -= 1;
        self.pop_scope();
        self.line("}");
        emit!(self, "puts({t}.join().unwrap())");
    }

    fn tpl_spawn_move(&mut self) {
        let data = self.fresh("moved");
        let xs = Self::recv(self.list(2));
        emit!(self, "{data} = {xs}.dup()");
        let t = self.fresh("t");
        emit!(self, "{t} = spawn move {{");
        self.push_scope(ScopeKind::Thread);
        self.bind(&data, Ty::List, false);
        self.quiet += 1;
        self.block_depth += 1;
        self.ind += 1;
        emit!(self, "{data}.push({})", self.int(1));
        emit!(self, "{}.sum() + {data}.length", Self::recv(self.list(2)));
        self.ind -= 1;
        self.block_depth -= 1;
        self.quiet -= 1;
        self.pop_scope();
        self.line("}");
        emit!(self, "puts({t}.join())");
    }

    fn tpl_channel(&mut self) {
        let ch = self.fresh("ch");
        let ws = self.fresh("ws");
        let acc = self.fresh("acc");
        let w = self.rng.range(1, 4);
        let k = self.rng.range(1, 20);
        let cap = if self.rng.chance(50) { format!("capacity: {}", self.rng.range(1, 4)) } else { String::new() };
        emit!(self, "{ch}: Channel[Int] = Channel.new({cap})");
        let wi = self.fresh("p");
        let j = self.fresh("j");
        emit!(self, "{ws} = (1..{w}).map {{ |{wi}| spawn {{");
        self.push_scope(ScopeKind::Thread);
        self.bind(&wi, Ty::Int, false);
        self.bind(&j, Ty::Int, false);
        self.quiet += 1;
        let e = self.int(2);
        self.quiet -= 1;
        self.pop_scope();
        emit!(self, "  for {j} in 0...{k} {{ {ch}.send({e}) }}");
        self.line("  nil");
        self.line("} }");
        emit!(self, "{acc}: List[Int] = []");
        emit!(
            self,
            "{} {{ {acc}.push({ch}.recv()!) }}",
            if w * k > 0 { format!("{}.times", w * k) } else { "0.times".into() }
        );
        emit!(self, "for t in {ws} {{ t.join() }}");
        emit!(self, "{acc}.sort_in_place()");
        emit!(self, "puts({acc})");
        self.bind(&acc, Ty::List, true);
    }

    fn tpl_channel_close(&mut self) {
        let ch = self.fresh("ch");
        let t = self.fresh("t");
        let k = self.rng.range(0, 30);
        emit!(self, "{ch}: Channel[List[Int]] = Channel.new()");
        let i = self.fresh("p");
        self.push_scope(ScopeKind::Thread);
        self.bind(&i, Ty::Int, false);
        let e = Self::recv(self.list(2));
        self.pop_scope();
        emit!(self, "{t} = spawn {{");
        emit!(self, "  for {i} in 0...{k} {{ msg = {e}.dup(); {ch}.send(move msg) }}");
        emit!(self, "  {ch}.close()");
        self.line("}");
        let got = self.fresh("got");
        emit!(self, "{got}: List[List[Int]] = []");
        emit!(self, "for msg in {ch} {{ {got}.push(msg) }}");
        emit!(self, "{t}.join()");
        emit!(self, "puts({got})");
    }

    fn tpl_atomic_mutex(&mut self) {
        let a = self.fresh("atom");
        let m = self.fresh("mtx");
        let ts = self.fresh("ts");
        let w = self.rng.range(1, 6);
        let k = self.rng.range(1, 200);
        emit!(self, "{a} = Atomic.new(0)");
        emit!(self, "{m} = Mutex.new([0])");
        let wi = self.fresh("p");
        self.push_scope(ScopeKind::Thread);
        self.bind(&wi, Ty::Int, false);
        self.cheap += 1;
        let e = self.int(2);
        self.cheap -= 1;
        self.pop_scope();
        emit!(self, "{ts} = (1..{w}).map {{ |{wi}|");
        self.line("  spawn {");
        emit!(self, "    for _j in 0...{k} {{");
        emit!(self, "      {a}.add(({e}) % 100)");
        emit!(self, "      {m}.lock {{ |l| l[0] = l[0] + 1 }}");
        self.line("    }");
        self.line("  }");
        self.line("}");
        emit!(self, "for t in {ts} {{ t.join() }}");
        emit!(self, "puts({a}.get())");
        emit!(self, "puts({m}.get())");
    }

    fn tpl_shared(&mut self) {
        let c = self.fresh("Shared");
        let s = self.fresh("sh");
        let w = self.rng.range(1, 5);
        let k = self.rng.range(1, 50);
        emit!(self, "shared class {c} {{");
        self.line("  hits: Atomic[Int] = Atomic.new(0)");
        self.line("  total: Int = 0");
        self.line("}");
        emit!(self, "{s} = {c}.new()");
        emit!(self, "(1..{w}).map {{ |i| spawn {{");
        emit!(self, "  for j in 0...{k} {{");
        emit!(self, "    {s}.hits.add(1)");
        emit!(self, "    with lock({s}) {{ {s}.total = {s}.total + i * j }}");
        self.line("  }");
        self.line("} }.each { |t| t.join() }");
        emit!(self, "puts({s}.hits.get())");
        emit!(self, "puts({s}.total)");
    }

    fn tpl_fibers(&mut self) {
        let log = self.fresh("log");
        let fs = self.fresh("fs");
        let tot = self.fresh("tot");
        let n = self.rng.range(1, 6);
        let r = self.rng.range(1, 5);
        let i = self.fresh("p");
        let rr = self.fresh("p");
        emit!(self, "{log}: List[String] = []");
        emit!(self, "{fs} = (0...{n}).map {{ |{i}| Fiber.start {{");
        self.ind += 1;
        self.push_scope(ScopeKind::NoAssign);
        self.bind(&i, Ty::Int, false);
        self.fiber_depth += 1;
        self.block_depth += 1;
        emit!(self, "for {rr} in 0...{r} {{");
        emit!(self, "  {log}.push(\"f#{{{i}}}r#{{{rr}}}\")");
        emit!(self, "  if ({i} + {rr}) % {} == 0 {{ Fiber.yield() }}", self.rng.range(1, 3));
        self.line("}");
        let k = self.rng.range(0, 2);
        for _ in 0..k {
            self.stmt();
        }
        let e = self.int(2);
        self.line(&e);
        self.block_depth -= 1;
        self.fiber_depth -= 1;
        self.pop_scope();
        self.ind -= 1;
        self.line("} }");
        emit!(self, "{tot} = 0");
        emit!(self, "for f in {fs} {{ {tot} += f.join().unwrap() }}");
        emit!(self, "puts({tot})");
        emit!(self, "puts({log})");
        self.bind(&tot, Ty::Int, true);
    }

    fn tpl_fiber_channel(&mut self) {
        let ch = self.fresh("fch");
        let k = self.rng.range(0, 25);
        let cap = self.rng.range(0, 3);
        let p = self.fresh("prod");
        let c = self.fresh("cons");
        emit!(self, "{ch}: Channel[Int] = Channel.new({cap})");
        let i = self.fresh("p");
        self.push_scope(ScopeKind::NoAssign);
        self.bind(&i, Ty::Int, false);
        let e = self.int(2);
        self.pop_scope();
        emit!(self, "{p} = Fiber.start {{");
        emit!(self, "  for {i} in 0...{k} {{ {ch}.send({e}) }}");
        emit!(self, "  {ch}.close()");
        self.line("  \"sent\"");
        self.line("}");
        emit!(self, "{c} = Fiber.start {{");
        self.line("  got: List[Int] = []");
        emit!(self, "  {ch}.each {{ |x| got.push(x) }}");
        self.line("  got");
        self.line("}");
        emit!(self, "puts({p}.join().unwrap())");
        emit!(self, "puts({c}.join().unwrap())");
    }

    fn tpl_fibers_in_thread(&mut self) {
        let t = self.fresh("t");
        let n = self.rng.range(1, 6);
        let i = self.fresh("p");
        emit!(self, "{t} = spawn {{");
        self.push_scope(ScopeKind::Thread);
        self.quiet += 1;
        self.bind(&i, Ty::Int, false);
        let e = self.int(2);
        self.pop_scope();
        self.quiet -= 1;
        self.line("  order: List[Int] = []");
        emit!(self, "  fs = (0...{n}).map {{ |{i}| Fiber.start {{");
        emit!(self, "    order.push({i})");
        self.line("    Fiber.yield()");
        emit!(self, "    order.push({i} + 100)");
        emit!(self, "    {e}");
        self.line("  } }");
        self.line("  s = 0");
        self.line("  for f in fs { s += f.join().unwrap() }");
        self.line("  (s, order)");
        self.line("}");
        emit!(self, "puts({t}.join().unwrap())");
    }

    fn tpl_panics(&mut self) {
        let b = self.fresh("bad");
        if self.rng.chance(50) {
            emit!(self, "{b} = spawn {{ xs: List[Int] = []; xs[{}] }}", self.rng.range(0, 5));
        } else {
            emit!(self, "{b} = Fiber.start {{ panic(\"boom #{{{}}}\") }}", self.rng.range(0, 9));
        }
        emit!(self, "match {b}.join() {{");
        self.line("  Ok(_) => puts(\"unexpected\")");
        self.line("  Err(e) => puts(\"caught: #{e.lines()[0]}\")");
        self.line("}");
    }

    fn tpl_freeze(&mut self) {
        let fz = self.fresh("fz");
        let xs = Self::recv(self.list(3));
        emit!(self, "{fz} = freeze({xs}.dup())");
        self.bind(&fz, Ty::Frozen, false);
        let t = self.fresh("t");
        emit!(self, "{t} = spawn {{ {fz}.map {{ |x| x * 2 + 1 }}.sum() + {fz}.length }}");
        emit!(self, "puts({t}.join().unwrap())");
        let b = self.block(ScopeKind::Thread, &[Ty::Int], |g, _| g.int(2));
        emit!(self, "puts({fz}.par_map {b})");
    }

    fn tpl_par(&mut self) {
        let xs = self.fresh("big");
        let n = self.rng.range(0, 3000);
        emit!(self, "{xs} = (0...{n}).to_list()");
        let b = self.block(ScopeKind::Thread, &[Ty::Int], |g, ps| {
            format!("({} * {} + {}) % 9973", ps[0], g.rng.range(1, 9), g.int_leaf())
        });
        emit!(self, "puts(({xs}.par_map {b}).sum())");
        let b2 = self.block(ScopeKind::Thread, &[Ty::Int], |g, _| g.boolean(0));
        emit!(self, "puts(({xs}.par_select {b2}).length)");
        emit!(self, "puts({xs}.par_reduce(0) {{ |a, b| a + b }})");
        let atom = self.fresh("atom");
        emit!(self, "{atom} = Atomic.new(0)");
        emit!(self, "{xs}.par_each {{ |x| {atom}.add(x % 7) }}");
        emit!(self, "puts({atom}.get())");
        // Strings and nested allocations inside parallel workers.
        emit!(self, "puts(({xs}.take(200).par_map {{ |x| \"n#{{x}}\" * 2 }}).join(\"\").length)");
    }

    fn tpl_churn(&mut self) {
        let junk = self.fresh("junk");
        let n = self.rng.range(100, 3000);
        emit!(self, "{junk}: List[String] = []");
        let i = self.fresh("i");
        emit!(self, "for {i} in 0...{n} {{");
        emit!(self, "  {junk}.push(\"j#{{{i}}}-#{{{i} * 7 % 13}}\")");
        emit!(
            self,
            "  if {i} % {} == 0 {{ {junk} = {junk}.drop({}) }}",
            self.rng.range(50, 400),
            self.rng.range(0, 30)
        );
        self.line("}");
        emit!(self, "puts({junk}.length)");
        emit!(self, "puts({junk}.take(3))");
        let nest = self.fresh("nest");
        emit!(self, "{nest}: Dict[String, List[Int]] = [:]");
        emit!(self, "for {i} in 0...{} {{", self.rng.range(10, 400));
        emit!(self, "  k = \"g#{{{i} % 7}}\"");
        emit!(self, "  {nest}[k] = ({nest}[k] ?? []) + [{i}]");
        self.line("}");
        emit!(self, "puts({nest}.map {{ |e| e.1.length }})");
        if self.rng.chance(50) {
            self.line("gc()");
        }
    }

    /// Old objects pointing at young ones: a linked structure built across collections, with old
    /// nodes rewired to fresh ones (exercises the write barrier and the remembered set).
    fn tpl_old_young(&mut self) {
        let node = self.fresh("Node");
        let head = self.fresh("head");
        let n = self.rng.range(10, 600);
        let every = self.rng.range(7, 120);
        emit!(self, "class {node} {{");
        self.line("  val: Int");
        self.line("  tag: String");
        emit!(self, "  next: {node}?");
        self.line("  kids: List[Int]");
        self.line("}");
        emit!(self, "{head}: {node}? = nil");
        emit!(self, "for i in 0...{n} {{");
        emit!(self, "  {head} = {node}.new(i, \"n#{{i}}\", {head}, [i, i * 2])");
        emit!(self, "  if i % {every} == 0 {{ gc() }}");
        self.line("}");
        let keep = self.fresh("keep");
        emit!(self, "{keep}: List[{node}] = []");
        let cur = self.fresh("cur");
        emit!(self, "{cur} = {head}");
        emit!(self, "while {cur} != nil {{");
        emit!(self, "  c = {cur}!");
        emit!(self, "  if c.val % 3 == 0 {{ {keep}.push(c) }}");
        emit!(self, "  {cur} = c.next");
        self.line("}");
        self.line("gc()");
        // Rewire old nodes to point at young ones and grow their (old) lists.
        emit!(self, "for k in {keep} {{");
        emit!(self, "  k.next = {node}.new(k.val + 1000, k.tag + \"!\", nil, [k.val])");
        self.line("  k.kids.push(k.val * 7)");
        self.line("  k.tag = k.tag.upcase()");
        self.line("}");
        if self.rng.chance(50) {
            self.line("gc()");
        }
        let total = self.fresh("total");
        emit!(self, "{total} = 0");
        emit!(self, "for k in {keep} {{ {total} += k.next!.val + k.kids.sum() + k.tag.length }}");
        emit!(self, "puts({total})");
        emit!(self, "puts({keep}.length)");
        emit!(self, "puts(({keep}.take(3).map {{ |k| k.tag + \"/\" + k.next!.tag }}).join(\",\"))");
        self.bind(&total, Ty::Int, true);
    }

    /// Threads build object graphs and hand them back through `join` and channels.
    fn tpl_thread_graphs(&mut self) {
        let t = self.fresh("t");
        let n = self.rng.range(1, 400);
        let w = self.rng.range(1, 4);
        let ch = self.fresh("gch");
        emit!(self, "{ch}: Channel[Dict[String, List[Int]]] = Channel.new({})", self.rng.range(0, 2));
        emit!(self, "{t} = (0...{w}).map {{ |w| spawn {{");
        self.line("  rows: List[List[Int]] = []");
        emit!(self, "  for i in 0...{n} {{");
        self.line("    rows.push([w, i, i * i % 101])");
        emit!(self, "    if i % 50 == 0 {{ gc() }}");
        self.line("  }");
        self.line("  d: Dict[String, List[Int]] = [:]");
        self.line("  for r in rows { k = \"r#{r[2] % 4}\"; d[k] = (d[k] ?? []) + [r[1]] }");
        emit!(self, "  {ch}.send(move d)");
        self.line("  rows");
        self.line("} }");
        let got = self.fresh("got");
        emit!(self, "{got}: List[Int] = []");
        emit!(self, "for _w in 0...{w} {{");
        emit!(self, "  d = {ch}.recv()!");
        emit!(self, "  {got}.push(d.values().map {{ |l| l.sum() }}.sum() + d.length)");
        self.line("}");
        emit!(self, "{got}.sort_in_place()");
        emit!(self, "puts({got})");
        emit!(self, "for th in {t} {{");
        self.line("  rows = th.join().unwrap()");
        self.line("  gc()");
        self.line("  puts(rows.length)");
        self.line("  puts(rows.map { |r| r[2] }.sum())");
        self.line("}");
    }

    fn tpl_closure_counter(&mut self) {
        let mk = self.fresh("mk");
        let c = self.fresh("ctr");
        emit!(self, "{mk} = fn(start: Int) -> fn() -> Int {{");
        self.line("  count = start");
        self.line("  { || count += 1; count * 2 }");
        self.line("}");
        emit!(self, "{c} = {mk}({})", self.rng.range(0, 9));
        let k = self.rng.range(0, 6);
        emit!(self, "{k}.times {{ |_i| {c}() }}");
        emit!(self, "puts({c}())");
        let fs = self.fresh("adders");
        emit!(self, "{fs} = {}.map {{ |n| fn(x: Int) -> Int {{ x + n }} }}", Self::recv(self.list(1)));
        emit!(self, "puts({fs}.map {{ |f| f({}) }})", self.rng.range(0, 9));
    }

    // ---------------------------------------------------------------- program

    fn program(&mut self, seed: u64) {
        emit!(self, "# difffuzz program, seed {seed}");
        for _ in 0..self.rng.range(0, 2) {
            self.gen_enum();
        }
        if self.rng.chance(50) {
            self.gen_tree();
        }
        for _ in 0..self.rng.range(0, 2) {
            self.gen_rec();
        }
        if self.rng.chance(60) {
            self.gen_results();
        }
        for _ in 0..self.rng.range(0, 3) {
            self.gen_class();
        }
        for _ in 0..self.rng.range(1, 5) {
            self.gen_func();
        }
        self.push_scope(ScopeKind::Normal);
        let items = self.rng.range(6, 22);
        self.budget = 120;
        for _ in 0..items {
            match self.rng.below(34) {
                0 => self.tpl_spawn(),
                1 => self.tpl_spawn_move(),
                2 => self.tpl_channel(),
                3 => self.tpl_channel_close(),
                4 => self.tpl_atomic_mutex(),
                5 => self.tpl_shared(),
                6 => self.tpl_fibers(),
                7 => self.tpl_fiber_channel(),
                8 => self.tpl_fibers_in_thread(),
                9 => self.tpl_panics(),
                10 => self.tpl_freeze(),
                11 => self.tpl_par(),
                12 => self.tpl_churn(),
                13 => self.tpl_closure_counter(),
                14 => self.tpl_old_young(),
                15 => self.tpl_thread_graphs(),
                _ => self.stmt(),
            }
        }
        // Dump every top-level variable: corruption anywhere shows up in the output.
        let vars: Vec<Var> = self.scopes[0].vars.clone();
        for v in vars {
            if v.ty != Ty::Fun {
                emit!(self, "puts({})", v.name);
            }
        }
        self.pop_scope();
    }
}

#[cfg(test)]
mod tests {
    use super::generate;

    #[test]
    fn deterministic() {
        for seed in 0..20 {
            assert_eq!(generate(seed), generate(seed));
        }
    }

    #[test]
    fn balanced_and_varied() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..50 {
            let p = generate(seed);
            assert!(p.starts_with("# difffuzz program"));
            assert_eq!(p.matches('{').count(), p.matches('}').count(), "unbalanced braces for seed {seed}");
            seen.insert(p);
        }
        assert_eq!(seen.len(), 50);
    }
}
