//! Patterns, `match`, and exhaustiveness.

use rustc_hash::FxHashMap;

use super::*;
use crate::ast::{Expr, ExprKind, MatchArm, Pattern, PatternKind};

impl Checker {
    /// The type a pattern was matched against (for codegen's runtime checks).
    pub fn ty_of_pattern_subject(&self, p: &Pattern) -> TyId {
        self.pattern_subjects.get(&p.id).copied().unwrap_or(self.types.error)
    }

    /// Binds an irrefutable pattern (assignment targets, `for` variables, `let`).
    pub fn bind_irrefutable(&mut self, p: &Pattern, ty: TyId) {
        if !self.is_irrefutable(p, ty) {
            self.err("E0361", "this pattern can fail to match; use `match` or `if let`", p.span);
        }
        self.check_pattern(p, ty, true);
    }

    pub fn is_irrefutable(&mut self, p: &Pattern, ty: TyId) -> bool {
        match &p.kind {
            PatternKind::Wildcard | PatternKind::Bind(_) => true,
            PatternKind::Tuple(ps) => match self.types.get(self.types.shallow_resolve(ty)).clone() {
                Ty::Tuple(ts) if ts.len() == ps.len() => ps.iter().zip(ts).all(|(p, t)| self.is_irrefutable(p, t)),
                Ty::Dyn | Ty::Error => true,
                _ => false,
            },
            PatternKind::Typed { ty: te, .. } => {
                let t = self.resolve_type(te, &mut self.type_scope());
                self.trial(|c| c.assignable(ty, t))
            }
            _ => false,
        }
    }

    /// Checks `p` against a subject of type `ty`, declaring its bindings in the current scope.
    pub fn check_pattern(&mut self, p: &Pattern, ty: TyId, irrefutable_ctx: bool) {
        let ty = self.types.shallow_resolve(ty);
        self.pattern_subjects.insert(p.id, ty);
        match &p.kind {
            PatternKind::Wildcard => {
                self.pats.insert(p.id, PatRes::Wildcard);
            }
            PatternKind::Bind(name) => {
                let l = self.declare_local(&name.name, ty, name.span);
                self.pats.insert(p.id, PatRes::Bind(l));
            }
            PatternKind::Literal(e) => {
                let t = self.check_expr(e, Some(ty));
                if !self.trial(|c| c.assignable(t, ty) || c.assignable(ty, t)) && !self.try_int_to_float(t, ty) {
                    let msg = format!("pattern of type `{}` can never match `{}`", self.show(t), self.show(ty));
                    self.err("E0362", msg, p.span);
                }
                if self.try_int_to_float(t, ty) {
                    self.coerce.insert(e.id, Coercion::IntToFloat);
                }
                self.pats.insert(p.id, PatRes::Literal);
            }
            PatternKind::Range { lo, hi, .. } => {
                let int = self.types.int;
                self.check_against(lo, int, "range pattern");
                self.check_against(hi, int, "range pattern");
                if !self.trial(|c| c.assignable(int, ty)) {
                    let msg = format!("range patterns match `Int`, not `{}`", self.show(ty));
                    self.err("E0362", msg, p.span);
                }
                self.pats.insert(p.id, PatRes::Literal);
            }
            PatternKind::Tuple(ps) => match self.tuple_in(ty, ps.len()) {
                Some(ts) => {
                    for (sp, t) in ps.iter().zip(ts) {
                        self.check_pattern(sp, t, irrefutable_ctx);
                    }
                    self.pats.insert(p.id, PatRes::Wildcard);
                }
                None if matches!(self.types.get(ty), Ty::Dyn | Ty::Error) => {
                    for sp in ps {
                        self.check_pattern(sp, ty, irrefutable_ctx);
                    }
                    self.pats.insert(p.id, PatRes::Wildcard);
                }
                None => {
                    let msg = format!("tuple pattern cannot match `{}`", self.show(ty));
                    self.err("E0362", msg, p.span);
                }
            },
            PatternKind::Typed { name, ty: te } => {
                let target = self.resolve_type(te, &mut self.type_scope());
                let (narrowed, _) = self.split_by_type(ty, target);
                let l = self.declare_local(&name.name, narrowed, name.span);
                let rt = self.rt_of(target);
                let idx = self.rt_type(rt);
                self.pats.insert(p.id, PatRes::Type { rt: idx, bind: Some(l) });
            }
            PatternKind::Or(alts) => {
                for a in alts {
                    let before = self.ctx().scopes.last().unwrap().len();
                    self.check_pattern(a, ty, irrefutable_ctx);
                    if self.ctx().scopes.last().unwrap().len() != before {
                        self.err("E0363", "alternatives in an `|` pattern cannot bind variables", a.span);
                    }
                }
                self.pats.insert(p.id, PatRes::Wildcard);
            }
            PatternKind::Ctor { path, args, has_parens, shorthand } => {
                self.check_ctor_pattern(p, path, args, *has_parens, *shorthand, ty, irrefutable_ctx)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_ctor_pattern(
        &mut self,
        p: &Pattern,
        path: &[ast::Ident],
        args: &[Pattern],
        has_parens: bool,
        shorthand: bool,
        ty: TyId,
        irr: bool,
    ) {
        // Find the enum and variant.
        let name = &path.last().unwrap().name;
        let variant: Option<(EnumId, u32)> = if shorthand {
            self.enum_in(ty).and_then(|(en, _)| {
                self.enums[en as usize].variants.iter().position(|v| &v.name == name).map(|t| (en, t as u32))
            })
        } else if path.len() == 1 {
            match self.lookup_name(self.cur_module, name) {
                Some(ScopeItem::Variant(en, t)) => Some((en, t)),
                _ => {
                    // A bare variant name of the subject's enum.
                    self.enum_in(ty).and_then(|(en, _)| {
                        self.enums[en as usize].variants.iter().position(|v| &v.name == name).map(|t| (en, t as u32))
                    })
                }
            }
        } else {
            let prefix = ast::Expr { id: u32::MAX - 1, kind: path_expr(&path[..path.len() - 1]), span: path[0].span };
            match self.resolve_path(&prefix) {
                Some(expr::PathRes::Enum(en, _)) => {
                    self.enums[en as usize].variants.iter().position(|v| &v.name == name).map(|t| (en, t as u32))
                }
                _ => None,
            }
        };
        if let Some((en, tag)) = variant {
            let info = self.enums[en as usize].clone();
            // Payload types: from the subject when it is (or contains) this enum.
            let targs: Vec<TyId> = match self.enum_in(ty) {
                Some((e2, a)) if e2 == en => a,
                _ => info.params.iter().map(|_| self.types.fresh_var()).collect(),
            };
            let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(targs).collect();
            let v = &info.variants[tag as usize];
            if has_parens && args.len() != v.fields.len() {
                let msg = format!("`{}` has {} field(s), but the pattern has {}", v.name, v.fields.len(), args.len());
                self.err("E0364", msg, p.span);
            }
            if !has_parens && !v.fields.is_empty() && !irr {
                // `.Circle` without bindings matches any payload.
            }
            for (i, a) in args.iter().enumerate() {
                let ft = v.fields.get(i).map(|&(_, t)| self.types.subst(t, &subst, None)).unwrap_or(self.types.error);
                self.check_pattern(a, ft, irr);
            }
            if !self.subject_has_enum(ty, en) {
                let msg = format!("pattern `{}` can never match `{}`", v.name, self.show(ty));
                self.err("E0362", msg, p.span);
            }
            self.pats.insert(p.id, PatRes::Variant { class: v.class_id, tag });
            return;
        }
        // A type test: `Int`, `Point`, `List[Int]`.
        if !shorthand {
            let te =
                ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.to_vec(), args: vec![] }, span: p.span };
            let before = self.diags.len();
            self.suppress_diags += 1;
            let t = self.resolve_type(&te, &mut self.type_scope());
            self.suppress_diags -= 1;
            self.diags.truncate(before);
            if !matches!(self.types.get(t), Ty::Error) {
                if !args.is_empty() {
                    self.err("E0365", "type patterns do not take arguments; bind with `name: Type`", p.span);
                }
                if !self.trial(|c| c.assignable(t, ty) || c.assignable(ty, t)) && !self.overlaps(ty, t) {
                    let msg = format!("type `{}` can never match `{}`", self.show(t), self.show(ty));
                    self.err("E0362", msg, p.span);
                }
                let rt = self.rt_of(t);
                let idx = self.rt_type(rt);
                self.pats.insert(p.id, PatRes::Type { rt: idx, bind: None });
                return;
            }
        }
        let msg = format!("unknown pattern `{}`", path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join("."));
        let mut d = Diag::error("E0366", msg, p.span);
        if let Some((en, _)) = self.enum_in(ty) {
            let names: Vec<String> = self.enums[en as usize].variants.iter().map(|v| v.name.clone()).collect();
            if let Some(s) = crate::util::suggest(name, names.iter().map(|s| s.as_str())) {
                d = d.fix(format!("did you mean `.{s}`?"), path.last().unwrap().span, s);
            }
        }
        self.emit(d);
    }

    /// The element types of a tuple type of arity `n` inside `ty` (directly or as a union member).
    fn tuple_in(&self, ty: TyId, n: usize) -> Option<Vec<TyId>> {
        let ty = self.types.shallow_resolve(ty);
        let members = match self.types.get(ty) {
            Ty::Union(p) => p.clone(),
            _ => vec![ty],
        };
        members.into_iter().find_map(|m| match self.types.get(self.types.shallow_resolve(m)) {
            Ty::Tuple(ts) if ts.len() == n => Some(ts.clone()),
            _ => None,
        })
    }

    fn overlaps(&mut self, a: TyId, b: TyId) -> bool {
        let members: Vec<TyId> = match self.types.get(self.types.shallow_resolve(a)) {
            Ty::Union(p) => p.clone(),
            _ => vec![a],
        };
        members.into_iter().any(|m| self.trial(|c| c.assignable(m, b) || c.assignable(b, m)))
    }

    /// The enum type inside `ty` (directly, or as a union member).
    fn enum_in(&self, ty: TyId) -> Option<(EnumId, Vec<TyId>)> {
        let ty = self.types.shallow_resolve(ty);
        match self.types.get(ty) {
            Ty::Enum(e, a) => Some((*e, a.clone())),
            Ty::Union(parts) => parts.iter().find_map(|&p| match self.types.get(self.types.shallow_resolve(p)) {
                Ty::Enum(e, a) => Some((*e, a.clone())),
                _ => None,
            }),
            Ty::Frozen(i) => self.enum_in(*i),
            _ => None,
        }
    }

    fn subject_has_enum(&self, ty: TyId, en: EnumId) -> bool {
        matches!(self.types.get(self.types.shallow_resolve(ty)), Ty::Dyn | Ty::Error | Ty::Param(_) | Ty::Iface(..))
            || self.enum_in(ty).is_some_and(|(e, _)| e == en)
    }

    pub fn check_match(&mut self, e: &Expr, subject: &Expr, arms: &[MatchArm], expected: Option<TyId>) -> TyId {
        let st = self.check_expr(subject, None);
        let subject_local = match (&subject.kind, self.names.get(&subject.id)) {
            (ExprKind::Ident(_), Some(NameRes::Local(l))) => Some(*l),
            _ => None,
        };
        let mut tys = Vec::new();
        // What the subject can still be after the arms so far.
        let mut remaining = st;
        for arm in arms {
            self.push_scope();
            self.check_pattern(&arm.pattern, remaining, false);
            // Narrow the subject variable inside the arm.
            if let Some(l) = subject_local {
                if let Some(nt) = self.pattern_narrowing(&arm.pattern, st) {
                    self.narrow(l, nt);
                }
            }
            if let Some(g) = &arm.guard {
                let (tn, _) = self.check_cond(g);
                for (l, t) in tn {
                    self.narrow(l, t);
                }
            }
            let t = self.check_stmt(&arm.body, expected);
            self.record_ty(arm.body.id, t);
            tys.push(t);
            self.pop_scope();
            if arm.guard.is_none() {
                remaining = self.subtract_pattern(remaining, &arm.pattern);
            }
        }
        self.check_exhaustive(e, st, arms);
        if arms.is_empty() {
            return self.types.never;
        }
        if let Some(exp) = expected {
            let all = tys.clone();
            if self.trial(|c| all.iter().all(|&t| c.assignable(t, exp))) {
                for (arm, &t) in arms.iter().zip(tys.iter()) {
                    if self.try_int_to_float(t, exp) {
                        self.coerce.insert(arm.body.id, Coercion::IntToFloat);
                    }
                }
                return exp;
            }
        }
        self.join(&tys)
    }

    /// The subject type minus the values an unguarded arm with pattern `p` certainly matched.
    fn subtract_pattern(&mut self, ty: TyId, p: &Pattern) -> TyId {
        let removed: Option<TyId> = match (&p.kind, self.pats.get(&p.id).cloned()) {
            (PatternKind::Literal(e), _) if matches!(e.kind, ExprKind::Nil) => Some(self.types.nil),
            (PatternKind::Typed { ty: te, .. }, _) => Some(self.resolve_type(te, &mut self.type_scope())),
            (PatternKind::Ctor { path, .. }, Some(PatRes::Type { .. })) => {
                let te =
                    ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.clone(), args: vec![] }, span: p.span };
                self.suppress_diags += 1;
                let before = self.diags.len();
                let t = self.resolve_type(&te, &mut self.type_scope());
                self.diags.truncate(before);
                self.suppress_diags -= 1;
                Some(t)
            }
            _ => None,
        };
        let Some(r) = removed else { return ty };
        match self.split_by_type(ty, r) {
            (_, Some(rest)) => rest,
            (_, None) => ty,
        }
    }

    /// The type a variable has when `p` matches.
    fn pattern_narrowing(&mut self, p: &Pattern, st: TyId) -> Option<TyId> {
        match (&p.kind, self.pats.get(&p.id).cloned()) {
            (_, Some(PatRes::Variant { class, .. })) => {
                let (en, _) = self.variant_of_class.get(&class).copied()?;
                let (e2, a) = self.enum_in(st)?;
                if e2 == en {
                    Some(self.types.intern(Ty::Enum(en, a)))
                } else {
                    None
                }
            }
            (PatternKind::Ctor { path, .. }, Some(PatRes::Type { .. })) => {
                let te =
                    ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.clone(), args: vec![] }, span: p.span };
                self.suppress_diags += 1;
                let t = self.resolve_type(&te, &mut self.type_scope());
                self.suppress_diags -= 1;
                Some(self.split_by_type(st, t).0)
            }
            (PatternKind::Literal(e), _) if matches!(e.kind, ExprKind::Nil) => Some(self.types.nil),
            _ => None,
        }
    }

    fn check_exhaustive(&mut self, e: &Expr, st: TyId, arms: &[MatchArm]) {
        let pats: Vec<&Pattern> = arms.iter().filter(|a| a.guard.is_none()).map(|a| &a.pattern).collect();
        let missing = self.missing_cases(st, &pats);
        if !missing.is_empty() {
            let msg = format!("`match` does not cover {}", missing.join(", "));
            let insert_at = Span::new(e.span.file, e.span.end - 1, e.span.end - 1);
            let arms_text: String = missing.iter().map(|m| format!("  {m} => panic(\"unhandled\")\n")).collect();
            self.emit(Diag::error("E0360", msg, e.span).fix("add the missing arms", insert_at, arms_text).fix(
                "add a catch-all arm",
                insert_at,
                "  _ => panic(\"unhandled\")\n",
            ));
        }
    }

    /// Cases of `ty` not covered by `pats`, rendered as patterns.
    fn missing_cases(&mut self, ty: TyId, pats: &[&Pattern]) -> Vec<String> {
        let rows: Vec<Vec<AP>> = pats.iter().map(|p| vec![self.abstract_pat(p)]).collect();
        let mut out: Vec<String> = self.uncovered(&[ty], rows, 0).into_iter().map(|mut w| w.remove(0)).collect();
        out.dedup();
        out.truncate(8);
        out
    }

    fn abstract_pat(&mut self, p: &Pattern) -> AP {
        match (&p.kind, self.pats.get(&p.id).cloned()) {
            (PatternKind::Wildcard | PatternKind::Bind(_), _) => AP::Wild,
            (PatternKind::Literal(e), _) => match e.kind {
                ExprKind::Bool(b) => AP::Ctor(Key::Bool(b), vec![]),
                ExprKind::Nil => AP::Ctor(Key::Nil, vec![]),
                _ => AP::Lit,
            },
            (PatternKind::Range { .. }, _) => AP::Lit,
            (PatternKind::Tuple(ps), _) => {
                AP::Ctor(Key::Tuple(ps.len()), ps.iter().map(|x| self.abstract_pat(x)).collect())
            }
            (PatternKind::Or(alts), _) => AP::Or(alts.iter().map(|x| self.abstract_pat(x)).collect()),
            (PatternKind::Typed { ty, .. }, _) => {
                let t = self.resolve_type(ty, &mut self.type_scope());
                AP::Type(t)
            }
            (PatternKind::Ctor { args, has_parens, .. }, Some(PatRes::Variant { class, .. })) => {
                let arity = self
                    .variant_of_class
                    .get(&class)
                    .map(|&(e, t)| self.enums[e as usize].variants[t as usize].fields.len())
                    .unwrap_or(0);
                let sub = if *has_parens && args.len() == arity {
                    args.iter().map(|x| self.abstract_pat(x)).collect()
                } else {
                    vec![AP::Wild; arity]
                };
                AP::Ctor(Key::Variant(class), sub)
            }
            (PatternKind::Ctor { path, .. }, Some(PatRes::Type { .. })) => {
                let te =
                    ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.clone(), args: vec![] }, span: p.span };
                self.suppress_diags += 1;
                let before = self.diags.len();
                let t = self.resolve_type(&te, &mut self.type_scope());
                self.diags.truncate(before);
                self.suppress_diags -= 1;
                AP::Type(t)
            }
            _ => AP::Lit,
        }
    }

    /// The constructors of a type for exhaustiveness: (key, field types, rendering), or None if
    /// the type has too many values to enumerate (numbers, strings...).
    fn constructors(&mut self, t: TyId) -> Option<Vec<(Key, Vec<TyId>, String)>> {
        let t = self.types.shallow_resolve(t);
        match self.types.get(t).clone() {
            Ty::Enum(e, args) => {
                let info = self.enums[e as usize].clone();
                let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args).collect();
                let is_result = Some(e) == self.well_known.result;
                Some(
                    info.variants
                        .iter()
                        .map(|v| {
                            let fts = v.fields.iter().map(|&(_, ft)| self.types.subst(ft, &subst, None)).collect();
                            let name = if is_result { v.name.clone() } else { format!(".{}", v.name) };
                            (Key::Variant(v.class_id), fts, name)
                        })
                        .collect(),
                )
            }
            Ty::Class(builtin::BOOL, _) => {
                Some(vec![(Key::Bool(true), vec![], "true".into()), (Key::Bool(false), vec![], "false".into())])
            }
            Ty::Class(builtin::NIL, _) => Some(vec![(Key::Nil, vec![], "nil".into())]),
            Ty::Tuple(ts) => Some(vec![(Key::Tuple(ts.len()), ts, String::new())]),
            Ty::Union(members) => {
                let mut out = Vec::new();
                for m in members {
                    match self.constructors(m) {
                        Some(cs) => out.extend(cs),
                        _ => {
                            let name = self.show(m);
                            out.push((Key::Member(m), vec![], name));
                        }
                    }
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// The type a constructor's values have (for matching type patterns against it).
    fn key_type(&mut self, k: &Key, scrutinee: TyId) -> TyId {
        match k {
            Key::Member(m) => *m,
            Key::Nil => self.types.nil,
            Key::Bool(_) => self.types.bool,
            Key::Variant(c) => {
                let (e, _) = self.variant_of_class[c];
                // Use the enum as it appears in the scrutinee (with its arguments).
                let sc = self.types.shallow_resolve(scrutinee);
                let members = match self.types.get(sc) {
                    Ty::Union(p) => p.clone(),
                    _ => vec![sc],
                };
                members
                    .into_iter()
                    .find(|&m| matches!(self.types.get(m), Ty::Enum(x, _) if *x == e))
                    .unwrap_or(self.types.error)
            }
            Key::Tuple(_) => scrutinee,
        }
    }

    /// Pattern-matrix usefulness: witnesses (one string per column) of value vectors that no row
    /// matches.
    fn uncovered(&mut self, tys: &[TyId], rows: Vec<Vec<AP>>, depth: usize) -> Vec<Vec<String>> {
        if tys.is_empty() {
            return if rows.is_empty() { vec![vec![]] } else { vec![] };
        }
        if depth > 24 {
            return vec![];
        }
        // Expand alternatives in the first column.
        let mut expanded = Vec::new();
        for r in rows {
            match &r[0] {
                AP::Or(alts) => {
                    for a in alts {
                        let mut nr = r.clone();
                        nr[0] = a.clone();
                        expanded.push(nr);
                    }
                }
                _ => expanded.push(r),
            }
        }
        let rows = expanded;
        let t = tys[0];
        let rest = &tys[1..];
        // Only split on constructors when some row actually inspects this column; otherwise the
        // default matrix suffices (and recursive types would expand forever).
        let inspected = rows.iter().any(|r| matches!(r[0], AP::Ctor(..) | AP::Type(_)));
        let ctors = if inspected || rows.is_empty() { self.constructors(t) } else { None };
        match ctors {
            Some(ctors) => {
                let mut out = Vec::new();
                for (key, fts, name) in ctors {
                    let kt = self.key_type(&key, t);
                    let mut spec = Vec::new();
                    for r in &rows {
                        let head = match &r[0] {
                            AP::Wild => Some(vec![AP::Wild; fts.len()]),
                            AP::Ctor(k, sub) if *k == key => {
                                Some(if sub.len() == fts.len() { sub.clone() } else { vec![AP::Wild; fts.len()] })
                            }
                            AP::Type(pt) => {
                                let pt = *pt;
                                if self.trial(|c| c.assignable(kt, pt)) {
                                    Some(vec![AP::Wild; fts.len()])
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(mut h) = head {
                            h.extend(r[1..].iter().cloned());
                            spec.push(h);
                        }
                    }
                    let mut sub_tys = fts.clone();
                    sub_tys.extend_from_slice(rest);
                    for w in self.uncovered(&sub_tys, spec, depth + 1) {
                        let (fields, tail) = w.split_at(fts.len());
                        let shown = match &key {
                            Key::Tuple(_) => format!("({})", fields.join(", ")),
                            _ if fields.is_empty() => name.clone(),
                            _ => format!("{}({})", name, fields.join(", ")),
                        };
                        let mut v = vec![shown];
                        v.extend(tail.iter().cloned());
                        out.push(v);
                        if out.len() >= 8 {
                            return out;
                        }
                    }
                }
                out
            }
            None => {
                let defaults: Vec<Vec<AP>> = rows
                    .iter()
                    .filter(|r| match &r[0] {
                        AP::Wild => true,
                        AP::Type(pt) => {
                            let pt = *pt;
                            self.trial(|c| c.assignable(t, pt))
                        }
                        _ => false,
                    })
                    .map(|r| r[1..].to_vec())
                    .collect();
                self.uncovered(rest, defaults, depth + 1)
                    .into_iter()
                    .map(|w| {
                        let mut v = vec!["_".to_string()];
                        v.extend(w);
                        v
                    })
                    .collect()
            }
        }
    }
}

fn path_expr(path: &[ast::Ident]) -> ExprKind {
    let mut e = ExprKind::Ident(path[0].name.clone());
    for seg in &path[1..] {
        e = ExprKind::Member {
            recv: Box::new(ast::Expr { id: u32::MAX - 2, kind: e, span: seg.span }),
            name: seg.clone(),
            args: vec![],
            block: None,
            parens: false,
            safe: false,
        };
    }
    e
}

/// Abstract pattern for exhaustiveness checking.
#[derive(Clone, Debug)]
enum AP {
    Wild,
    Ctor(Key, Vec<AP>),
    Type(TyId),
    Or(Vec<AP>),
    /// A literal or range: never covers a whole constructor.
    Lit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Variant(ClassId),
    Bool(bool),
    Nil,
    Tuple(usize),
    /// A union member type with no enumerable constructors.
    Member(TyId),
}
