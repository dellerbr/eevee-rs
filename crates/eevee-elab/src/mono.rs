//! Monomorphization of parameterized classes.
//!
//! Ports the Python reference's `specialize_class` (eevee/exec/elaborator.py):
//! each distinct `C#(args)` instantiation becomes a distinct concrete class
//! with a mangled name (`C__arg1__arg2`), produced by deep-copying the template
//! and substituting its type parameters with the actual arguments. This gives
//! every specialization its own static fields / `type_id` (so the factory can
//! tell `uvm_component_registry#(A)` from `#(B)`), and lets a class that
//! `extends` its own type parameter (`uvm_port_base #(type IF) extends IF`)
//! resolve to the right base.
//!
//! Strategy (demand-driven): concrete (non-template) classes are walked; every
//! `#(args)` reference triggers `specialize`, which recurses into nested args.
//! Specializations are memoized by mangled name; a recursion guard handles
//! UVM's self-referential hierarchies.

use std::collections::{HashMap, HashSet};

use eevee_ast::*;
use eevee_core::LogicVec;

/// Expand all parameterized-class instantiations in `classes` into concrete
/// specialized classes. `global_aliases` are package/module-scope typedefs
/// (`typedef uvm_pool#(string,int) foo;`) made visible to every class body.
/// Returns the concrete classes (with `#(args)` references rewritten to mangled
/// names) plus every specialization reached, and a map from each global alias
/// name to its resolved concrete (possibly mangled) class name.
pub fn monomorphize(
    classes: &[ClassDecl],
    global_aliases: &[TypeAlias],
) -> (Vec<ClassDecl>, HashMap<String, String>) {
    let mut m = Mono::new(classes);
    for a in global_aliases {
        m.global_aliases
            .entry(a.alias.clone())
            .or_insert_with(|| a.target.clone());
    }
    // Process concrete classes; their bodies pull in the specializations.
    for c in classes {
        if c.params.is_empty() {
            let mut ctx = SubstCtx::default();
            for a in &c.type_aliases {
                ctx.aliases.insert(a.alias.clone(), a.target.clone());
            }
            let spec = m.build_spec(c, &c.name, ctx);
            m.record(c.name.clone(), spec);
        }
    }
    // Resolve each global alias to a concrete class name (specializing its
    // target on demand). Done after the concrete pass so templates are known.
    let mut alias_map = HashMap::new();
    let targets: Vec<(String, TypeRef)> = global_aliases
        .iter()
        .map(|a| (a.alias.clone(), a.target.clone()))
        .collect();
    for (alias, target) in targets {
        m.cur = SubstCtx::default();
        let concrete = m.resolve(&target);
        alias_map.insert(alias, concrete);
    }
    (m.out, alias_map)
}

/// param name -> concrete (possibly mangled) actual type name.
type Bindings = HashMap<String, String>;

/// The substitution context for the specialization currently being built.
#[derive(Default, Clone)]
struct SubstCtx {
    /// All formal params -> actual value (type and value params).
    bindings: Bindings,
    /// Value params -> value text (for substituting value references in exprs).
    value_bindings: Bindings,
    /// Class-scoped typedef aliases (`this_type`, `common_type`, `type_id`).
    aliases: HashMap<String, TypeRef>,
}

struct Mono {
    templates: HashMap<String, ClassDecl>,
    out: Vec<ClassDecl>,
    index: HashMap<String, usize>,
    in_progress: HashSet<String>,
    cur: SubstCtx,
    /// Package/module-scope typedefs visible to every class body.
    global_aliases: HashMap<String, TypeRef>,
}

impl Mono {
    fn new(classes: &[ClassDecl]) -> Mono {
        let mut templates = HashMap::new();
        for c in classes {
            if !c.params.is_empty() {
                templates.entry(c.name.clone()).or_insert_with(|| c.clone());
            }
        }
        Mono {
            templates,
            out: Vec::new(),
            index: HashMap::new(),
            in_progress: HashSet::new(),
            cur: SubstCtx::default(),
            global_aliases: HashMap::new(),
        }
    }

    fn record(&mut self, name: String, decl: ClassDecl) {
        match self.index.get(&name) {
            Some(&i) => self.out[i] = decl, // replace stub
            None => {
                self.index.insert(name, self.out.len());
                self.out.push(decl);
            }
        }
    }

    /// Resolve a type reference to a concrete class name, specializing any
    /// `#(args)` and substituting type parameters / class-local aliases.
    fn resolve(&mut self, tr: &TypeRef) -> String {
        // A formal type parameter -> its bound actual type name.
        if let Some(bound) = self.cur.bindings.get(&tr.name) {
            return bound.clone();
        }
        // A class-scoped typedef alias (e.g. `this_type`) -> its target.
        if let Some(target) = self.cur.aliases.get(&tr.name).cloned() {
            return self.resolve(&target);
        }
        // A package/module-scope typedef alias -> its target. Only consulted
        // when `tr` has no `#(args)` of its own (an alias names a full type).
        if tr.args.is_empty() {
            if let Some(target) = self.global_aliases.get(&tr.name).cloned() {
                return self.resolve(&target);
            }
        }
        let arg_names: Vec<String> = tr.args.clone().iter().map(|a| self.resolve(a)).collect();
        if arg_names.is_empty() {
            return tr.name.clone();
        }
        self.specialize(&tr.name, &arg_names)
    }

    /// Create (or return the cached) specialization `base#(arg_names)`.
    fn specialize(&mut self, base: &str, arg_names: &[String]) -> String {
        let Some(tmpl) = self.templates.get(base).cloned() else {
            // Not a known parameterized class: keep the base name (params drop).
            return base.to_string();
        };
        let mangled = mangle(base, &tmpl.params, arg_names);
        if self.index.contains_key(&mangled) || self.in_progress.contains(&mangled) {
            return mangled;
        }
        // bindings: formal param -> actual arg name (or its declared default).
        let mut ctx = SubstCtx::default();
        for (i, p) in tmpl.params.iter().enumerate() {
            let val = arg_names
                .get(i)
                .cloned()
                .or_else(|| p.default.clone())
                .unwrap_or_default();
            if val.is_empty() {
                continue;
            }
            ctx.bindings.insert(p.name.clone(), val.clone());
            if !p.is_type {
                ctx.value_bindings.insert(p.name.clone(), val);
            }
        }
        for a in &tmpl.type_aliases {
            ctx.aliases.insert(a.alias.clone(), a.target.clone());
        }

        self.in_progress.insert(mangled.clone());
        // Reserve the slot first (stable identity for recursive references).
        self.index.insert(mangled.clone(), self.out.len());
        self.out.push(stub(&mangled));
        let spec = self.build_spec(&tmpl, &mangled, ctx);
        let i = self.index[&mangled];
        self.out[i] = spec;
        self.in_progress.remove(&mangled);
        mangled
    }

    /// Deep-copy `tmpl` under `name`, substituting type params via `ctx`.
    fn build_spec(&mut self, tmpl: &ClassDecl, name: &str, ctx: SubstCtx) -> ClassDecl {
        let saved = std::mem::replace(&mut self.cur, ctx);

        let base = tmpl.base.as_ref().map(|b| {
            self.resolve(&TypeRef {
                name: b.clone(),
                args: tmpl.base_args.clone(),
            })
        });

        let mut fields = tmpl.fields.clone();
        for f in &mut fields {
            self.subst_vardecl_type(f);
        }

        let type_aliases = tmpl
            .type_aliases
            .iter()
            .map(|a| TypeAlias {
                alias: a.alias.clone(),
                target: TypeRef::simple(self.resolve(&a.target)),
            })
            .collect();

        let mut methods = tmpl.methods.clone();
        for mth in &mut methods {
            self.subst_func(mth);
        }
        let constructor = tmpl.constructor.clone().map(|mut ct| {
            self.subst_func(&mut ct);
            ct
        });

        self.cur = saved;
        ClassDecl {
            name: name.to_string(),
            base,
            fields,
            methods,
            constructor,
            type_aliases,
            params: Vec::new(),
            base_args: Vec::new(),
            consts: tmpl.consts.clone(),
        }
    }

    fn subst_vardecl_type(&mut self, v: &mut VarDecl) {
        if let Some(cn) = &v.class_name {
            let resolved = self.resolve(&TypeRef {
                name: cn.clone(),
                args: v.type_args.clone(),
            });
            v.class_name = Some(resolved);
            v.type_args = Vec::new();
        }
    }

    fn subst_func(&mut self, f: &mut FuncDecl) {
        if let Some(rc) = &f.ret_class {
            f.ret_class = Some(self.resolve(&TypeRef::simple(rc.clone())));
        }
        for p in &mut f.params {
            if let Some(cn) = &p.class_name {
                p.class_name = Some(self.resolve(&TypeRef::simple(cn.clone())));
            }
        }
        let mut body = std::mem::replace(&mut f.body, Stmt::Null);
        self.subst_stmt(&mut body);
        f.body = body;
    }

    fn subst_stmt(&mut self, s: &mut Stmt) {
        match s {
            Stmt::Block(ss) => {
                for st in ss {
                    self.subst_stmt(st);
                }
            }
            Stmt::VarDecl(v) => self.subst_vardecl_type(v),
            Stmt::Timed { body, .. } => self.subst_stmt(body),
            Stmt::Blocking { rhs, .. } | Stmt::Nonblocking { rhs, .. } => self.subst_expr(rhs),
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.subst_expr(cond);
                self.subst_stmt(then_branch);
                if let Some(e) = else_branch {
                    self.subst_stmt(e);
                }
            }
            Stmt::Expr(e) => self.subst_expr(e),
            Stmt::Return(Some(e)) => self.subst_expr(e),
            Stmt::SysCall { args, .. } => {
                for a in args {
                    self.subst_expr(a);
                }
            }
            Stmt::Return(None) | Stmt::Null => {}
        }
    }

    fn subst_expr(&mut self, e: &mut Expr) {
        match e {
            // A bare reference to a value parameter -> its constant value.
            Expr::Ref(name) => {
                if let Some(val) = self.cur.value_bindings.get(name) {
                    *e = value_to_expr(val);
                }
            }
            Expr::StaticCall {
                class_name,
                class_args,
                args,
                ..
            } => {
                let resolved = self.resolve(&TypeRef {
                    name: class_name.clone(),
                    args: std::mem::take(class_args),
                });
                *class_name = resolved;
                for a in args {
                    self.subst_expr(a);
                }
            }
            Expr::Unary { operand, .. } => self.subst_expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.subst_expr(lhs);
                self.subst_expr(rhs);
            }
            Expr::Call { args, .. } => {
                for a in args {
                    self.subst_expr(a);
                }
            }
            Expr::Field { obj, .. } => self.subst_expr(obj),
            Expr::MethodCall { obj, args, .. } => {
                self.subst_expr(obj);
                for a in args {
                    self.subst_expr(a);
                }
            }
            Expr::Index { base, index } => {
                self.subst_expr(base);
                self.subst_expr(index);
            }
            Expr::New { args } => {
                for a in args {
                    self.subst_expr(a);
                }
            }
            Expr::Concat(parts) => {
                for p in parts {
                    self.subst_expr(p);
                }
            }
            Expr::SysCall { args, .. } => {
                for a in args {
                    self.subst_expr(a);
                }
            }
            Expr::Literal(_) | Expr::Str(_) | Expr::Null => {}
        }
    }
}

/// A value-parameter binding -> a literal expression (number or string).
fn value_to_expr(val: &str) -> Expr {
    match val.parse::<i64>() {
        Ok(n) => Expr::Literal(LogicVec::from_i64(n, 32)),
        Err(_) => Expr::Str(val.to_string()),
    }
}

/// `C#(int, 4)` -> `C__int__4`. Uses defaults for omitted args; sanitizes the
/// already-concrete arg names (which may themselves contain `__`).
fn mangle(base: &str, params: &[ParamDecl], arg_names: &[String]) -> String {
    let mut parts = vec![base.to_string()];
    for (i, p) in params.iter().enumerate() {
        let v = arg_names
            .get(i)
            .cloned()
            .or_else(|| p.default.clone())
            .unwrap_or_else(|| "_".to_string());
        parts.push(sanitize(&v));
    }
    parts.join("__")
}

fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn stub(name: &str) -> ClassDecl {
    ClassDecl {
        name: name.to_string(),
        base: None,
        fields: Vec::new(),
        methods: Vec::new(),
        constructor: None,
        type_aliases: Vec::new(),
        params: Vec::new(),
        base_args: Vec::new(),
        consts: Vec::new(),
    }
}
