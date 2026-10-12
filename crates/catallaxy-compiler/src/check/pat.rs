//! Patterns, `match`, and exhaustiveness.

use rustc_hash::FxHashMap;

use super::*;
use crate::ast::{Expr, ExprKind, MatchArm, Pattern, PatternKind};

impl Checker {
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
            PatternKind::Tuple(ps) => match self.types.get(ty).clone() {
                Ty::Tuple(ts) if ts.len() == ps.len() => {
                    for (sp, t) in ps.iter().zip(ts) {
                        self.check_pattern(sp, t, irrefutable_ctx);
                    }
                    self.pats.insert(p.id, PatRes::Wildcard);
                }
                Ty::Dyn | Ty::Error => {
                    for sp in ps {
                        self.check_pattern(sp, ty, irrefutable_ctx);
                    }
                    self.pats.insert(p.id, PatRes::Wildcard);
                }
                _ => {
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
            let prefix = ast::Expr {
                id: u32::MAX - 1,
                kind: path_expr(&path[..path.len() - 1]),
                span: path[0].span,
            };
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
            let te = ast::TypeExpr {
                kind: ast::TypeExprKind::Named { path: path.to_vec(), args: vec![] },
                span: p.span,
            };
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
        for arm in arms {
            self.push_scope();
            self.check_pattern(&arm.pattern, st, false);
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
                let te = ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.clone(), args: vec![] }, span: p.span };
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
            self.emit(
                Diag::error("E0360", msg, e.span)
                    .fix("add the missing arms", insert_at, arms_text)
                    .fix("add a catch-all arm", insert_at, "  _ => panic(\"unhandled\")\n"),
            );
        }
    }

    /// Cases of `ty` not covered by `pats` (rendered as patterns).
    fn missing_cases(&mut self, ty: TyId, pats: &[&Pattern]) -> Vec<String> {
        let ty = self.types.shallow_resolve(ty);
        let flat: Vec<&Pattern> = pats
            .iter()
            .flat_map(|p| match &p.kind {
                PatternKind::Or(alts) => alts.iter().collect::<Vec<_>>(),
                _ => vec![*p],
            })
            .collect();
        if flat.iter().any(|p| self.pattern_covers_all(p, ty)) {
            return Vec::new();
        }
        match self.types.get(ty).clone() {
            Ty::Enum(en, args) => {
                let info = self.enums[en as usize].clone();
                let subst: FxHashMap<ParamId, TyId> = info.params.iter().copied().zip(args).collect();
                let mut missing = Vec::new();
                for v in &info.variants {
                    let for_v: Vec<&Pattern> = flat
                        .iter()
                        .copied()
                        .filter(|p| matches!(self.pats.get(&p.id), Some(PatRes::Variant { class, .. }) if *class == v.class_id))
                        .collect();
                    let covered = for_v.iter().any(|p| match &p.kind {
                        PatternKind::Ctor { args, .. } => args.is_empty() || args.iter().enumerate().all(|(i, a)| {
                            let ft = v.fields.get(i).map(|&(_, t)| t).unwrap_or(self.types.error);
                            let ft = self.types.subst(ft, &subst, None);
                            self.pattern_covers_all(a, ft)
                        }),
                        _ => false,
                    }) || (v.fields.len() == 1 && !for_v.is_empty() && {
                        let ft = self.types.subst(v.fields[0].1, &subst, None);
                        let subs: Vec<&Pattern> = for_v
                            .iter()
                            .filter_map(|p| match &p.kind {
                                PatternKind::Ctor { args, .. } => args.first(),
                                _ => None,
                            })
                            .collect();
                        self.missing_cases(ft, &subs).is_empty()
                    });
                    if !covered {
                        let shown = if v.fields.is_empty() {
                            format!(".{}", v.name)
                        } else {
                            format!(".{}({})", v.name, vec!["_"; v.fields.len()].join(", "))
                        };
                        missing.push(if Some(en) == self.well_known.result { shown.trim_start_matches('.').to_string() } else { shown });
                    }
                }
                missing
            }
            Ty::Class(builtin::BOOL, _) => {
                let mut has = (false, false);
                for p in &flat {
                    if let PatternKind::Literal(e) = &p.kind {
                        match e.kind {
                            ExprKind::Bool(true) => has.0 = true,
                            ExprKind::Bool(false) => has.1 = true,
                            _ => {}
                        }
                    }
                }
                let mut m = Vec::new();
                if !has.0 {
                    m.push("true".to_string());
                }
                if !has.1 {
                    m.push("false".to_string());
                }
                m
            }
            Ty::Union(members) => {
                let mut missing = Vec::new();
                for m in members {
                    if m == self.types.nil {
                        let has_nil = flat.iter().any(|p| matches!(&p.kind, PatternKind::Literal(e) if matches!(e.kind, ExprKind::Nil)))
                            || flat.iter().any(|p| self.type_pattern_covers(p, m));
                        if !has_nil {
                            missing.push("nil".to_string());
                        }
                        continue;
                    }
                    if flat.iter().any(|p| self.type_pattern_covers(p, m)) {
                        continue;
                    }
                    if matches!(self.types.get(m), Ty::Enum(..) | Ty::Class(builtin::BOOL, _)) {
                        missing.extend(self.missing_cases(m, &flat));
                        continue;
                    }
                    missing.push(self.show(m));
                }
                missing
            }
            Ty::Tuple(ts) => {
                // Covered if some tuple pattern is irrefutable element-wise.
                let ok = flat.iter().any(|p| match &p.kind {
                    PatternKind::Tuple(ps) if ps.len() == ts.len() => {
                        ps.iter().zip(ts.iter()).all(|(sp, &t)| self.pattern_covers_all(sp, t))
                    }
                    _ => false,
                });
                if ok {
                    Vec::new()
                } else {
                    vec!["_".into()]
                }
            }
            Ty::Error | Ty::Never => Vec::new(),
            _ => vec!["_".into()],
        }
    }

    fn type_pattern_covers(&mut self, p: &Pattern, member: TyId) -> bool {
        match (&p.kind, self.pats.get(&p.id).cloned()) {
            (PatternKind::Typed { ty: te, .. }, _) => {
                let t = self.resolve_type(te, &mut self.type_scope());
                self.trial(|c| c.assignable(member, t))
            }
            (PatternKind::Ctor { path, .. }, Some(PatRes::Type { .. })) => {
                let te = ast::TypeExpr { kind: ast::TypeExprKind::Named { path: path.clone(), args: vec![] }, span: p.span };
                self.suppress_diags += 1;
                let t = self.resolve_type(&te, &mut self.type_scope());
                self.suppress_diags -= 1;
                // `List` (without args) in a pattern covers any list.
                let t = match (self.types.get(t).clone(), self.types.get(member).clone()) {
                    (Ty::Error, _) => return false,
                    (_, Ty::Class(c2, _)) if matches!(self.types.get(t), Ty::Class(c, _) if *c == c2) => return true,
                    _ => t,
                };
                self.trial(|c| c.assignable(member, t))
            }
            _ => false,
        }
    }

    fn pattern_covers_all(&mut self, p: &Pattern, ty: TyId) -> bool {
        match &p.kind {
            PatternKind::Wildcard | PatternKind::Bind(_) => true,
            PatternKind::Typed { .. } => self.type_pattern_covers(p, ty),
            PatternKind::Tuple(ps) => match self.types.get(self.types.shallow_resolve(ty)).clone() {
                Ty::Tuple(ts) if ts.len() == ps.len() => ps.iter().zip(ts).all(|(sp, t)| self.pattern_covers_all(sp, t)),
                _ => false,
            },
            PatternKind::Or(alts) => alts.iter().any(|a| self.pattern_covers_all(a, ty)),
            PatternKind::Ctor { .. } => {
                matches!(self.pats.get(&p.id), Some(PatRes::Type { .. })) && self.type_pattern_covers(p, ty)
            }
            _ => false,
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
