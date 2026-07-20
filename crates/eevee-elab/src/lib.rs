//! Elaboration: turn an [`eevee_ast::SourceFile`] into a runnable [`Sim`].
//!
//! For each module this:
//! 1. creates a 4-state [`net`](eevee_sched::Net) per variable (init value
//!    constant-folded and resized to the declared width), recording a
//!    name → (`NetId`, width) scope, then
//! 2. compiles every `always`/`initial` block to an [`eevee_ir::Program`] via
//!    the [`CodeGen`] mini-compiler and instantiates it through the chosen
//!    [`ExecBackend`] (interpreter today, JIT later).
//!
//! Names are resolved to `NetId`s / registers *here*, once — the running IR
//! never does name lookup. This is the P2 vertical slice (RTL subset); classes,
//! ports/instances, generate, and full type inference build on this spine.

#![forbid(unsafe_code)]

mod mono;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use eevee_ast::*;
use eevee_core::{LogicVec, Timescale};
use eevee_ir::{
    ArgMode, ClassDef, CollOp, DpiRegistry, ExecBackend, Inst, Label, Linkage, Program,
    ProgramBuilder, Reg, Value,
};
use eevee_sched::{EdgeKind, ForkJoin, NetId, Sim};

/// Statistics from a global elaboration pass (how much UVM we ingested).
#[derive(Debug, Default, Clone)]
pub struct ElabStats {
    /// Classes laid out (fields + vtables).
    pub classes: usize,
    /// Callables (free functions + methods + constructors) seen.
    pub callables: usize,
    /// Callables whose body could not be compiled yet (compiled to a stub).
    pub callables_stubbed: usize,
    /// Why callables were stubbed: (reason, count), highest first.
    pub stub_reasons: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollInfo {
    kind: CollKind,
    elem_class: Option<u32>,
    key_class: Option<u32>,
}

/// The global program shared by every module: class layout, the name→FuncId
/// map, and the compiled [`Linkage`].
struct Global {
    class_ids: HashMap<String, u32>,
    func_ids: HashMap<String, u32>,
    class_infos: Vec<ClassInfo>,
    consts: HashMap<String, LogicVec>,
    /// Package-level global variables: name -> static-storage id.
    globals: HashMap<String, u32>,
    /// Collection-typed globals and their element/key class types.
    global_coll: HashMap<String, CollInfo>,
    /// Class-handle globals: name -> class id.
    global_class: HashMap<String, u32>,
    /// Package/module-scope type aliases: alias name -> class id.
    global_type_aliases: HashMap<String, u32>,
    /// Unambiguous class-scoped typedefs (same mapping across all classes).
    class_typedefs: HashMap<String, u32>,
    /// Unambiguous class-scoped collection typedefs.
    class_collection_typedefs: HashMap<String, CollInfo>,
    /// Enum type name -> value-name table id (into `linkage.enum_tables`).
    enum_types: HashMap<String, u32>,
    linkage: Rc<Linkage>,
    stats: ElabStats,
}

/// Elaborate a parsed source file into a runnable simulation.
pub fn elaborate(file: &SourceFile, backend: &dyn ExecBackend) -> Sim {
    elaborate_with_stats(file, backend).0
}

/// Elaborate with caller-provided DPI-C host bindings.
pub fn elaborate_with_dpi(file: &SourceFile, backend: &dyn ExecBackend, dpi: DpiRegistry) -> Sim {
    elaborate_with_stats_and_dpi(file, backend, dpi).0
}

/// Like [`elaborate`], but also returns how much of the design was ingested
/// (useful for measuring UVM coverage during the port).
pub fn elaborate_with_stats(file: &SourceFile, backend: &dyn ExecBackend) -> (Sim, ElabStats) {
    elaborate_with_stats_and_dpi(file, backend, DpiRegistry::simulator_defaults())
}

/// Elaborate with statistics and caller-provided DPI-C host bindings.
pub fn elaborate_with_stats_and_dpi(
    file: &SourceFile,
    backend: &dyn ExecBackend,
    dpi: DpiRegistry,
) -> (Sim, ElabStats) {
    let mut sim = Sim::with_default_timescale();
    let ts = sim.kernel().timescale();

    // Gather global declarations from every package and module. UVM lives in a
    // single package; we flatten all classes/functions into one global scope
    // (import scoping is ignored for now).
    let mut class_decls: Vec<&ClassDecl> = Vec::new();
    let mut free_funcs: Vec<&FuncDecl> = Vec::new();
    let mut global_vars: Vec<&VarDecl> = Vec::new();
    let mut global_aliases: Vec<&TypeAlias> = Vec::new();
    let mut consts: HashMap<String, LogicVec> = HashMap::new();
    // Enum value->name tables (for `.name()`), and the type-name -> table-id map.
    let mut enum_types: HashMap<String, u32> = HashMap::new();
    let mut enum_tables: Vec<HashMap<i64, Rc<str>>> = Vec::new();
    let mut gather_enum = |name: &str, members: &[(String, LogicVec)]| {
        if enum_types.contains_key(name) {
            return;
        }
        let id = enum_tables.len() as u32;
        let mut table: HashMap<i64, Rc<str>> = HashMap::new();
        for (m, v) in members {
            table
                .entry(v.to_i64())
                .or_insert_with(|| Rc::from(m.as_str()));
        }
        enum_types.insert(name.to_string(), id);
        enum_tables.push(table);
    };
    for item in &file.items {
        match item {
            // Package items: classes, functions, enum consts, and package-level
            // global variables (e.g. UVM's `m_uvm_core_state` queue).
            Item::Package(p) => {
                for mi in &p.items {
                    match mi {
                        ModuleItem::Class(c) => class_decls.push(c),
                        ModuleItem::Func(f) => free_funcs.push(f),
                        ModuleItem::Var(v) => global_vars.push(v),
                        ModuleItem::TypeAlias(a) => global_aliases.push(a),
                        ModuleItem::EnumConst { name, value } => {
                            consts.entry(name.clone()).or_insert_with(|| value.clone());
                        }
                        ModuleItem::EnumType { name, members } => gather_enum(name, members),
                        _ => {}
                    }
                }
            }
            // Module items: classes/functions are global; vars are module nets
            // (handled in elaborate_module_processes).
            Item::Module(m) => {
                for mi in &m.items {
                    match mi {
                        ModuleItem::Class(c) => class_decls.push(c),
                        ModuleItem::Func(f) => free_funcs.push(f),
                        ModuleItem::TypeAlias(a) => global_aliases.push(a),
                        ModuleItem::EnumConst { name, value } => {
                            consts.entry(name.clone()).or_insert_with(|| value.clone());
                        }
                        ModuleItem::EnumType { name, members } => gather_enum(name, members),
                        _ => {}
                    }
                }
            }
            Item::Class(c) => class_decls.push(c),
            Item::Func(f) => free_funcs.push(f),
        }
    }

    // Inject IEEE-1800 builtin classes that have no SV source (e.g. `process`).
    // They are real classes with real (minimal) method bodies, so the SV code
    // that references them compiles and runs unmodified.
    let builtins = builtin_classes();
    for c in &builtins {
        if !class_decls.iter().any(|d| d.name == c.name) {
            class_decls.push(c);
        }
    }

    if let Ok(filt) = std::env::var("EEVEE_DUMP_GLOBALS") {
        eprintln!("-- global_vars ({}) --", global_vars.len());
        for v in &global_vars {
            if filt == "1" || v.name.contains(filt.as_str()) {
                eprintln!(
                    "  {} coll={:?} class={:?} is_string={}",
                    v.name, v.coll, v.class_name, v.is_string
                );
            }
        }
    }

    // Merge extern bodies + monomorphize parameterized classes, then build the
    // global tables over the expanded concrete class set.
    let (specialized, true_free, alias_map) =
        expand_classes(&class_decls, &free_funcs, &global_aliases);
    // Class-scoped `localparam`/`parameter`/enum constants join the global
    // constant table (first definition wins; names are effectively unique).
    for c in &specialized {
        for (name, value) in &c.consts {
            consts.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    let spec_refs: Vec<&ClassDecl> = specialized.iter().collect();
    let global = build_global(
        &spec_refs,
        &true_free,
        &global_vars,
        &alias_map,
        consts,
        enum_types,
        enum_tables,
        dpi,
        ts,
        backend,
    );
    let stats = global.stats.clone();

    // Each module contributes nets + processes, compiled against the global
    // class/function tables.
    for item in &file.items {
        if let Item::Module(m) = item {
            elaborate_module_processes(m, &global, &mut sim, backend);
        }
    }
    (sim, stats)
}

/// IEEE-1800 builtin classes that have no SV source. They are provided as real
/// (minimal) classes so SV code that references them compiles and runs.
/// Currently `process` (its handle is always null until fork/join lands; UVM
/// guards every use with `if (p != null)`, so the bodies are dead at runtime
/// but must still type-check).
fn builtin_classes() -> Vec<ClassDecl> {
    fn method(
        name: &str,
        is_void: bool,
        ret_class: Option<&str>,
        params: Vec<Param>,
        body: Stmt,
    ) -> FuncDecl {
        FuncDecl {
            name: name.to_string(),
            ret_width: 32,
            ret_class: ret_class.map(str::to_string),
            class_scope: None,
            dpi_name: None,
            is_void,
            is_virtual: false,
            params,
            body,
        }
    }
    fn str_param(name: &str) -> Param {
        Param {
            name: name.to_string(),
            dir: PortDir::Input,
            width: 32,
            class_name: None,
            type_scope: None,
            type_args: Vec::new(),
            coll: None,
            key_class_name: None,
            default: None,
        }
    }
    let ret0 = Stmt::Return(Some(Expr::Literal(LogicVec::zero(32))));
    let process = ClassDecl {
        name: "process".to_string(),
        base: None,
        fields: Vec::new(),
        methods: vec![
            // `process::self()` -> null (no current-process handle yet).
            method(
                "self",
                false,
                Some("process"),
                vec![],
                Stmt::Return(Some(Expr::Null)),
            ),
            method("status", false, None, vec![], ret0.clone()),
            method("kill", true, None, vec![], Stmt::Null),
            method("await", true, None, vec![], Stmt::Null),
            method("suspend", true, None, vec![], Stmt::Null),
            method("resume", true, None, vec![], Stmt::Null),
            // randstate save/restore: return "" / accept and ignore.
            method(
                "get_randstate",
                false,
                None,
                vec![],
                Stmt::Return(Some(Expr::Str(String::new()))),
            ),
            method(
                "set_randstate",
                true,
                None,
                vec![str_param("state")],
                Stmt::Null,
            ),
            // `process.srandom(seed)` — reseed the process's random generator.
            method("srandom", true, None, vec![str_param("seed")], Stmt::Null),
        ],
        constructor: None,
        type_aliases: Vec::new(),
        collection_aliases: Vec::new(),
        params: Vec::new(),
        base_args: Vec::new(),
        consts: Vec::new(),
        is_struct: false,
    };
    // IEEE-1800 built-in `mailbox #(T)`: unbounded/bounded queue of handles.
    // Parameterized, but since we don't have the source, inject a generic stub
    // that compiles. The T parameter is ignored here — any `mailbox #(Foo)` call
    // resolves to this class after mono fails to specialize it (no template).
    let ret0_32 = Stmt::Return(Some(Expr::Literal(LogicVec::zero(32))));
    let mailbox = ClassDecl {
        name: "mailbox".to_string(),
        base: None,
        fields: Vec::new(),
        methods: vec![
            // `m.num()` → int (number of items)
            method("num", false, None, vec![], ret0_32.clone()),
            // Blocking put/get (stubs: no blocking support yet)
            method("put", true, None, vec![str_param("item")], Stmt::Null),
            method("get", true, None, vec![str_param("item")], Stmt::Null),
            method("peek", true, None, vec![str_param("item")], Stmt::Null),
            // Non-blocking variants: return 0 (fail gracefully)
            method(
                "try_put",
                false,
                None,
                vec![str_param("item")],
                ret0_32.clone(),
            ),
            method(
                "try_get",
                false,
                None,
                vec![str_param("item")],
                ret0_32.clone(),
            ),
            method(
                "try_peek",
                false,
                None,
                vec![str_param("item")],
                ret0_32.clone(),
            ),
        ],
        constructor: None,
        type_aliases: Vec::new(),
        collection_aliases: Vec::new(),
        params: Vec::new(),
        base_args: Vec::new(),
        consts: Vec::new(),
        is_struct: false,
    };
    vec![process, mailbox]
}

/// Merge out-of-body (`extern`) method definitions into their class
/// declarations, then monomorphize parameterized classes. Returns the expanded
/// concrete class set (each `#(args)` instantiation specialized), the true
/// free functions (those that are not class methods), and the resolved
/// package-level type aliases (`alias name -> concrete/mangled class name`).
fn expand_classes<'a>(
    class_decls: &[&ClassDecl],
    free_funcs: &[&'a FuncDecl],
    global_aliases: &[&TypeAlias],
) -> (Vec<ClassDecl>, Vec<&'a FuncDecl>, HashMap<String, String>) {
    // Group extern definitions (free funcs with a class scope) by class.
    let mut extern_by_class: HashMap<&str, Vec<&FuncDecl>> = HashMap::new();
    for f in free_funcs {
        if let Some(scope) = &f.class_scope {
            extern_by_class.entry(scope.as_str()).or_default().push(f);
        }
    }
    // Fold the extern bodies into each class's method list (extern body wins;
    // the prototype keeps its virtualness / return type).
    let merged: Vec<ClassDecl> = class_decls
        .iter()
        .map(|c| {
            let mut decl = (*c).clone();
            if let Some(externs) = extern_by_class.get(c.name.as_str()) {
                let mut idx: HashMap<String, usize> = decl
                    .methods
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.name.clone(), i))
                    .collect();
                for e in externs {
                    // An out-of-body `function C::new(...)` is the constructor,
                    // not a regular method (the in-class `extern function new`
                    // is only a prototype).
                    if e.name == "new" {
                        decl.constructor = Some((*e).clone());
                        continue;
                    }
                    let mut merged_m = (*e).clone();
                    if let Some(&i) = idx.get(&e.name) {
                        let proto = &decl.methods[i];
                        merged_m.is_virtual = merged_m.is_virtual || proto.is_virtual;
                        merged_m.ret_class = merged_m.ret_class.clone().or(proto.ret_class.clone());
                        decl.methods[i] = merged_m;
                    } else {
                        idx.insert(e.name.clone(), decl.methods.len());
                        decl.methods.push(merged_m);
                    }
                }
            }
            decl
        })
        .collect();

    let owned_aliases: Vec<TypeAlias> = global_aliases.iter().map(|a| (*a).clone()).collect();
    let (specialized, alias_map) = mono::monomorphize(&merged, &owned_aliases);
    let true_free: Vec<&FuncDecl> = free_funcs
        .iter()
        .filter(|f| f.class_scope.is_none())
        .copied()
        .collect();
    (specialized, true_free, alias_map)
}

/// FuncIds for one class's methods and constructor. Each method entry is
/// (name, FuncId, is_virtual, return-class id).
struct ClassFids {
    method_fids: Vec<(String, u32, bool, Option<u32>)>,
    ctor_fid: Option<u32>,
}

/// Build the global class layout + compiled linkage from the flattened
/// declaration lists. Callable bodies that use not-yet-supported constructs
/// are compiled to stubs (so one unsupported class never aborts the rest).
#[allow(clippy::too_many_arguments)]
fn build_global(
    class_decls: &[&ClassDecl],
    free_funcs: &[&FuncDecl],
    global_vars: &[&VarDecl],
    alias_map: &HashMap<String, String>,
    consts: HashMap<String, LogicVec>,
    enum_types: HashMap<String, u32>,
    enum_tables: Vec<HashMap<i64, Rc<str>>>,
    dpi: DpiRegistry,
    ts: Timescale,
    _backend: &dyn ExecBackend,
) -> Global {
    // Class ids = declaration order. First declaration of a name wins.
    let mut class_ids: HashMap<String, u32> = HashMap::new();
    for (cid, c) in class_decls.iter().enumerate() {
        class_ids.entry(c.name.clone()).or_insert(cid as u32);
    }

    // Package/module typedefs resolved to concrete class ids (`alias -> cid`).
    // The alias target was resolved to a concrete/mangled class name by the
    // monomorphizer; map it to that class's id.
    let mut global_type_aliases: HashMap<String, u32> = HashMap::new();
    for (alias, concrete) in alias_map {
        if let Some(&cid) = class_ids.get(concrete) {
            global_type_aliases.insert(alias.clone(), cid);
        }
    }

    // Out-of-body (`extern`) method definitions, grouped by their class.
    let mut extern_by_class: HashMap<&str, Vec<&FuncDecl>> = HashMap::new();
    for f in free_funcs {
        if let Some(scope) = &f.class_scope {
            extern_by_class.entry(scope.as_str()).or_default().push(f);
        }
    }

    // Assign FuncIds in a fixed order: true free functions, then for each class
    // its methods then its constructor. FuncIds are stable regardless of the
    // topological class-layout order computed below.
    let mut jobs: Vec<(&FuncDecl, Option<u32>)> = Vec::new();
    let mut func_ids: HashMap<String, u32> = HashMap::new();
    for f in free_funcs {
        if f.class_scope.is_some() {
            continue; // extern definitions become methods of their class
        }
        func_ids.entry(f.name.clone()).or_insert(jobs.len() as u32);
        jobs.push((f, None));
    }
    let mut class_fids: Vec<ClassFids> = Vec::with_capacity(class_decls.len());
    for (cid, c) in class_decls.iter().enumerate() {
        // Effective method set: in-body methods, with any extern definition of
        // the same name supplying the body (virtualness/return type from the
        // in-body prototype are preserved); extern-only methods are appended.
        let mut eff: Vec<(&str, &FuncDecl, bool, Option<&str>)> = Vec::new();
        let mut idx: HashMap<&str, usize> = HashMap::new();
        for m in &c.methods {
            idx.insert(m.name.as_str(), eff.len());
            eff.push((m.name.as_str(), m, m.is_virtual, m.ret_class.as_deref()));
        }
        if let Some(externs) = extern_by_class.get(c.name.as_str()) {
            for e in externs {
                if let Some(&i) = idx.get(e.name.as_str()) {
                    let (nm, _, vis, rc) = eff[i];
                    eff[i] = (nm, e, vis || e.is_virtual, rc.or(e.ret_class.as_deref()));
                } else {
                    idx.insert(e.name.as_str(), eff.len());
                    eff.push((e.name.as_str(), e, e.is_virtual, e.ret_class.as_deref()));
                }
            }
        }
        let mut method_fids = Vec::new();
        for (name, m, is_virtual, ret_class_name) in &eff {
            let fid = jobs.len() as u32;
            jobs.push((m, Some(cid as u32)));
            let ret_class = ret_class_name.and_then(|rc| class_ids.get(rc).copied());
            method_fids.push(((*name).to_string(), fid, *is_virtual, ret_class));
        }
        let ctor_fid = c
            .constructor
            .as_ref()
            .map(|ct| {
                let fid = jobs.len() as u32;
                jobs.push((ct, Some(cid as u32)));
                fid
            })
            .or_else(|| {
                // A constructor declared as a method `new` (e.g. an `extern
                // function new(...)` whose body is defined out of body) is used
                // as the class's constructor.
                method_fids
                    .iter()
                    .find(|(n, ..)| n == "new")
                    .map(|&(_, fid, ..)| fid)
            });
        class_fids.push(ClassFids {
            method_fids,
            ctor_fid,
        });
    }

    // Every class has a constructor. Classes without an explicit `new` get a
    // synthesized default constructor (which implicitly calls `super.new()`),
    // matching SystemVerilog semantics. Their FuncIds follow all real jobs.
    let n_real = jobs.len() as u32;
    let mut synth_ctor_classes: Vec<u32> = Vec::new();
    for (cid, cf) in class_fids.iter_mut().enumerate() {
        if cf.ctor_fid.is_none() {
            cf.ctor_fid = Some(n_real + synth_ctor_classes.len() as u32);
            synth_ctor_classes.push(cid as u32);
        }
    }

    // Build class layouts in topological order (base before derived) so an
    // inherited field/method keeps its slot. A fixpoint avoids needing the
    // declarations to be perfectly ordered; classes whose base is missing or
    // cyclic are built with no inheritance.
    let n = class_decls.len();
    let mut built: Vec<Option<ClassInfo>> = (0..n).map(|_| None).collect();
    // Global storage defaults for every `static` class field and package
    // global variable. Package globals are allocated first (stable ids).
    let mut statics_defaults: Vec<Value> = Vec::new();
    let mut globals: HashMap<String, u32> = HashMap::new();
    let mut global_coll: HashMap<String, CollInfo> = HashMap::new();
    let mut global_class: HashMap<String, u32> = HashMap::new();
    for v in global_vars {
        if globals.contains_key(&v.name) {
            continue;
        }
        let id = statics_defaults.len() as u32;
        let default = match v.coll {
            Some(CollKind::Assoc) => Value::new_assoc(),
            Some(CollKind::Queue) => Value::new_queue(),
            None => default_value(v, &consts),
        };
        statics_defaults.push(default);
        globals.insert(v.name.clone(), id);
        let elem = v
            .class_name
            .as_deref()
            .and_then(|cn| class_ids.get(cn).copied());
        let key_class = v
            .key_class_name
            .as_deref()
            .and_then(|cn| class_ids.get(cn).copied());
        match v.coll {
            Some(kind) => {
                global_coll.insert(
                    v.name.clone(),
                    CollInfo {
                        kind,
                        elem_class: elem,
                        key_class,
                    },
                );
            }
            None => {
                if let Some(c) = elem {
                    global_class.insert(v.name.clone(), c);
                }
            }
        }
    }
    loop {
        let mut progress = false;
        for cid in 0..n {
            if built[cid].is_some() {
                continue;
            }
            let c = class_decls[cid];
            let base_id = c
                .base
                .as_ref()
                .and_then(|bn| {
                    // A class may `extends` its own type parameter (e.g.
                    // `uvm_port_base #(type IF=uvm_void) extends IF`); resolve
                    // such a base to the parameter's default class.
                    class_ids.get(bn).copied().or_else(|| {
                        c.params
                            .iter()
                            .find(|p| p.is_type && &p.name == bn)
                            .and_then(|p| p.default.as_ref())
                            .and_then(|def| class_ids.get(def).copied())
                    })
                })
                .filter(|bid| *bid as usize != cid);
            if let Some(bid) = base_id {
                if built[bid as usize].is_none() {
                    continue; // wait until the base is built
                }
            }
            built[cid] = Some(build_class_info(
                c,
                base_id,
                &built,
                &class_fids[cid],
                &class_ids,
                &consts,
                &mut statics_defaults,
            ));
            progress = true;
        }
        if !progress {
            break;
        }
    }
    for cid in 0..n {
        if built[cid].is_none() {
            // Missing or cyclic base: lay out with no inheritance.
            built[cid] = Some(build_class_info(
                class_decls[cid],
                None,
                &built,
                &class_fids[cid],
                &class_ids,
                &consts,
                &mut statics_defaults,
            ));
        }
    }
    let class_infos: Vec<ClassInfo> = built.into_iter().map(|o| o.unwrap()).collect();

    // Collect unambiguous cross-class typedef aliases (e.g. `rsrc_q_t` in
    // `uvm_resource_types`). A name is included only if every class that defines
    // it maps to the same class id (i.e. it's globally unambiguous).
    let mut class_typedefs: HashMap<String, u32> = HashMap::new();
    {
        let mut ambiguous: HashSet<String> = HashSet::new();
        for ci in &class_infos {
            for (alias, &cid) in &ci.type_aliases {
                if ambiguous.contains(alias) {
                    continue;
                }
                match class_typedefs.get(alias) {
                    Some(&existing) if existing == cid => {}
                    Some(_) => {
                        class_typedefs.remove(alias);
                        ambiguous.insert(alias.clone());
                    }
                    None => {
                        class_typedefs.insert(alias.clone(), cid);
                    }
                }
            }
        }
    }

    let mut class_collection_typedefs: HashMap<String, CollInfo> = HashMap::new();
    {
        let mut ambiguous: HashSet<String> = HashSet::new();
        for ci in &class_infos {
            for (alias, &info) in &ci.collection_aliases {
                if ambiguous.contains(alias) {
                    continue;
                }
                match class_collection_typedefs.get(alias) {
                    Some(&existing) if existing == info => {}
                    Some(_) => {
                        class_collection_typedefs.remove(alias);
                        ambiguous.insert(alias.clone());
                    }
                    None => {
                        class_collection_typedefs.insert(alias.clone(), info);
                    }
                }
            }
        }
    }

    // Compile every callable. Package callables have no module nets in scope.
    // A body that hits an unsupported construct panics inside the codegen; we
    // catch it and substitute a stub so the rest of the library still loads.
    let empty_scope: HashMap<String, (NetId, u32)> = HashMap::new();
    let gv = GlobalVars {
        vars: &globals,
        coll: &global_coll,
        class: &global_class,
        type_aliases: &global_type_aliases,
        enums: &enum_types,
        class_typedefs: &class_typedefs,
        class_collection_typedefs: &class_collection_typedefs,
    };
    let mut funcs: Vec<Rc<Program>> = Vec::with_capacity(jobs.len());
    let mut stubbed = 0usize;
    let mut reasons: HashMap<String, usize> = HashMap::new();
    // Optional per-function stub diagnostics. Set EEVEE_DUMP_STUBS=1 to list
    // every stubbed callable and its panic category; set it to a substring to
    // filter (e.g. EEVEE_DUMP_STUBS=uvm_init).
    let dump_stubs = std::env::var("EEVEE_DUMP_STUBS").ok();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the resilient-compile noise
    for (f, class_ctx) in &jobs {
        let cc = *class_ctx;
        if f.dpi_name.is_some() {
            funcs.push(Rc::new(dpi_program(f)));
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            CodeGen::new(
                &empty_scope,
                &func_ids,
                &class_ids,
                &class_infos,
                &consts,
                &gv,
                ts,
            )
            .gen_callable(f, cc)
        }));
        match result {
            Ok(prog) => funcs.push(Rc::new(prog)),
            Err(payload) => {
                stubbed += 1;
                let reason = panic_category(payload.as_ref());
                if let Some(filt) = &dump_stubs {
                    let qual = match cc {
                        Some(cid) => format!("{}::{}", class_infos[cid as usize].name, f.name),
                        None => f.name.clone(),
                    };
                    if filt == "1" || qual.contains(filt.as_str()) {
                        eprintln!("STUB {qual}  <-  {reason}");
                    }
                }
                *reasons.entry(reason).or_insert(0) += 1;
                funcs.push(Rc::new(stub_program(f, cc.is_some())));
            }
        }
    }
    std::panic::set_hook(prev_hook);

    // Append the synthesized default constructors (FuncIds n_real..). Each just
    // chains to its base constructor; never fails to compile.
    for &cid in &synth_ctor_classes {
        let base = class_infos[cid as usize].base;
        funcs.push(Rc::new(synth_default_ctor(base, &class_infos)));
    }

    let mut stub_reasons: Vec<(String, usize)> = reasons.into_iter().collect();
    stub_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let classes: Vec<ClassDef> = class_infos
        .iter()
        .map(|ci| {
            // Collection fields (slot, is_assoc) get fresh storage per `New`.
            let coll_fields: Vec<(u32, bool)> = ci
                .field_coll
                .iter()
                .filter_map(|(name, info)| {
                    let slot = ci.field_slot.get(name)?.0;
                    Some((slot, matches!(info.kind, CollKind::Assoc)))
                })
                .collect();
            let event_fields: Vec<u32> = ci
                .field_events
                .iter()
                .filter_map(|name| ci.field_slot.get(name).map(|(slot, _)| *slot))
                .collect();
            // Struct-typed fields (an unpacked `typedef struct {...}` member,
            // modeled as a no-method class — see `ClassDecl::is_struct`) also
            // get a fresh auto-constructed sub-object per `New`: SV structs
            // are always fully default-initialized, never left as a null
            // handle awaiting an explicit `new()` the way a real class-typed
            // field is.
            let struct_fields: Vec<(u32, u32)> = ci
                .field_class
                .iter()
                .filter_map(|(name, &cid)| {
                    if !class_infos[cid as usize].is_struct {
                        return None;
                    }
                    let slot = ci.field_slot.get(name)?.0;
                    Some((slot, cid))
                })
                .collect();
            ClassDef {
                name: ci.name.clone(),
                base: ci.base,
                field_defaults: ci.field_defaults.clone().into_boxed_slice(),
                vtable: ci.vtable.clone().into_boxed_slice(),
                coll_fields: coll_fields.into_boxed_slice(),
                event_fields: event_fields.into_boxed_slice(),
                struct_fields: struct_fields.into_boxed_slice(),
            }
        })
        .collect();
    let stats = ElabStats {
        classes: class_infos.len(),
        callables: jobs.len(),
        callables_stubbed: stubbed,
        stub_reasons,
    };
    let linkage = Rc::new(Linkage {
        funcs,
        classes,
        statics: statics_defaults.into_iter().map(RefCell::new).collect(),
        enum_tables,
        dpi,
    });
    Global {
        class_ids,
        func_ids,
        class_infos,
        consts,
        globals,
        global_coll,
        global_class,
        global_type_aliases,
        class_typedefs,
        class_collection_typedefs,
        enum_types,
        linkage,
        stats,
    }
}

/// Lay out one class: inherit the base's fields/methods/vtable (if built), then
/// append own fields and (virtual) methods using their pre-assigned FuncIds.
fn build_class_info(
    c: &ClassDecl,
    base_id: Option<u32>,
    built: &[Option<ClassInfo>],
    fids: &ClassFids,
    class_ids: &HashMap<String, u32>,
    consts: &HashMap<String, LogicVec>,
    statics_defaults: &mut Vec<Value>,
) -> ClassInfo {
    let (
        mut field_slot,
        mut field_class,
        mut field_coll,
        mut field_events,
        mut static_fields,
        mut static_field_class,
        mut static_field_coll,
        mut type_aliases,
        mut collection_aliases,
        mut method_ret_class,
        mut field_defaults,
        mut methods,
        mut vtable,
        mut vslot_of,
    ) = match base_id {
        Some(bid) => {
            let b = built[bid as usize]
                .as_ref()
                .expect("base laid out before derived");
            (
                b.field_slot.clone(),
                b.field_class.clone(),
                b.field_coll.clone(),
                b.field_events.clone(),
                b.static_fields.clone(),
                b.static_field_class.clone(),
                b.static_field_coll.clone(),
                b.type_aliases.clone(),
                b.collection_aliases.clone(),
                b.method_ret_class.clone(),
                b.field_defaults.clone(),
                b.methods.clone(),
                b.vtable.clone(),
                b.vslot_of.clone(),
            )
        }
        None => (
            HashMap::new(), // field_slot
            HashMap::new(), // field_class
            HashMap::new(), // field_coll
            HashSet::new(), // field_events
            HashMap::new(), // static_fields
            HashMap::new(), // static_field_class
            HashMap::new(), // static_field_coll
            HashMap::new(), // type_aliases
            HashMap::new(), // collection_aliases
            HashMap::new(), // method_ret_class
            Vec::new(),     // field_defaults
            HashMap::new(), // methods
            Vec::new(),     // vtable
            HashMap::new(), // vslot_of
        ),
    };
    // Type parameters with a class default (own to this class, not inherited).
    let mut type_param_default: HashMap<String, u32> = HashMap::new();
    for p in &c.params {
        if p.is_type {
            if let Some(def) = &p.default {
                if let Some(&cid) = class_ids.get(def) {
                    type_param_default.insert(p.name.clone(), cid);
                }
            }
        }
    }
    // Class-scoped typedef aliases first, so field/local types declared via a
    // `typedef <Class> alias;` (notably the ubiquitous `this_type`) resolve.
    for a in &c.type_aliases {
        // For parameterised targets like `uvm_queue#(uvm_resource_base)` the
        // monomorphizer produces a mangled name `uvm_queue__uvm_resource_base`.
        // Build it here so the lookup succeeds.
        let lookup_name: String = if a.target.args.is_empty() {
            a.target.name.clone()
        } else {
            let mut parts = vec![a.target.name.clone()];
            for arg in &a.target.args {
                // Sanitize just like mono::sanitize(): keep alnum/_; map rest to '_'.
                let s: String = arg
                    .name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                parts.push(if s.is_empty() { "_".to_string() } else { s });
            }
            parts.join("__")
        };
        if let Some(&tid) = class_ids.get(&lookup_name) {
            type_aliases.insert(a.alias.clone(), tid);
        }
    }
    // Resolve a type name to a class id: a global class, one of this class's
    // own typedef aliases, or a type parameter's class default.
    let resolve_ty = |name: &str| -> Option<u32> {
        class_ids
            .get(name)
            .copied()
            .or_else(|| type_aliases.get(name).copied())
            .or_else(|| type_param_default.get(name).copied())
    };
    for alias in &c.collection_aliases {
        collection_aliases.insert(
            alias.alias.clone(),
            CollInfo {
                kind: alias.kind,
                elem_class: alias.element.as_ref().and_then(|ty| resolve_ty(&ty.name)),
                key_class: alias.key.as_ref().and_then(|ty| resolve_ty(&ty.name)),
            },
        );
    }
    for fld in &c.fields {
        let elem_class = fld.class_name.as_deref().and_then(resolve_ty);
        let key_class = fld.key_class_name.as_deref().and_then(resolve_ty);
        if fld.is_static {
            // Static fields live in global storage, not per-instance slots.
            // A static collection field is initialized to fresh empty storage.
            let id = statics_defaults.len() as u32;
            let default = match fld.coll {
                Some(CollKind::Assoc) => Value::new_assoc(),
                Some(CollKind::Queue) => Value::new_queue(),
                None => default_value(fld, consts),
            };
            statics_defaults.push(default);
            static_fields.insert(fld.name.clone(), id);
            if let Some(fc) = elem_class {
                static_field_class.insert(fld.name.clone(), fc);
            }
            if let Some(kind) = fld.coll {
                static_field_coll.insert(
                    fld.name.clone(),
                    CollInfo {
                        kind,
                        elem_class,
                        key_class,
                    },
                );
            }
            continue;
        }
        let slot = field_defaults.len() as u32;
        field_slot.insert(fld.name.clone(), (slot, fld.width));
        if fld.is_event {
            field_events.insert(fld.name.clone());
        }
        match fld.coll {
            Some(kind) => {
                field_coll.insert(
                    fld.name.clone(),
                    CollInfo {
                        kind,
                        elem_class,
                        key_class,
                    },
                );
            }
            None => {
                if let Some(fc) = elem_class {
                    field_class.insert(fld.name.clone(), fc);
                }
            }
        }
        field_defaults.push(default_value(fld, consts));
    }
    for (name, fid, is_virtual, ret_class) in &fids.method_fids {
        methods.insert(name.clone(), *fid);
        if let Some(rid) = ret_class {
            method_ret_class.insert(name.clone(), *rid);
        }
        match vslot_of.get(name).copied() {
            Some(vslot) => vtable[vslot as usize] = *fid,
            None if *is_virtual => {
                vslot_of.insert(name.clone(), vtable.len() as u32);
                vtable.push(*fid);
            }
            None => {}
        }
    }
    ClassInfo {
        name: c.name.clone(),
        base: base_id,
        field_slot,
        field_class,
        field_coll,
        field_events,
        static_fields,
        static_field_class,
        static_field_coll,
        type_param_default,
        type_aliases,
        collection_aliases,
        method_ret_class,
        field_defaults,
        methods,
        vtable,
        vslot_of,
        ctor: fids.ctor_fid,
        is_struct: c.is_struct,
    }
}

/// Extract a coarse category from a panic payload, so similar failures group
/// together in the stub-reason histogram (the text before the first `:`).
fn panic_category(payload: &(dyn std::any::Any + Send)) -> String {
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic>".to_string()
    };
    let head = msg.split(':').next().unwrap_or(&msg).trim();
    // Keep the full message for the "unknown signal 'X'" bucket so the specific
    // names stay visible in the diagnostic histogram.
    let mut cat = if head == "elaboration" {
        msg.chars().take(70).collect::<String>()
    } else {
        head.chars().take(60).collect::<String>()
    };
    if cat.is_empty() {
        cat = "<empty>".to_string();
    }
    cat
}

/// A synthesized default constructor: `function new(); super.new(); endfunction`.
/// Reg 0 is `this`; it chains to the base constructor (if any) then returns.
fn synth_default_ctor(base: Option<u32>, class_infos: &[ClassInfo]) -> Program {
    let mut pb = ProgramBuilder::new("default new");
    let this = pb.new_reg(); // reg 0 = this
    pb.set_arg_modes(&[ArgMode::Input]);
    if let Some(b) = base {
        if let Some(base_ctor) = class_infos[b as usize].ctor {
            let arglist = pb.arglist(&[this]);
            let ret = pb.new_reg();
            pb.emit(Inst::Call {
                func: base_ctor,
                args: arglist,
                ret,
            });
        }
    }
    pb.emit(Inst::ReturnVoid);
    pb.build()
}

/// A placeholder body for a callable we cannot compile yet: returns the default
/// value (so the program is well-formed and the rest of the design links).
fn stub_program(f: &FuncDecl, has_this: bool) -> Program {
    let mut pb = ProgramBuilder::new(format!("stub {}", f.name));
    let mut modes = Vec::with_capacity(f.params.len() + usize::from(has_this));
    if has_this {
        pb.new_reg();
        modes.push(ArgMode::Input);
    }
    for param in &f.params {
        pb.new_reg();
        modes.push(arg_mode(param.dir));
    }
    pb.set_arg_modes(&modes);
    if f.is_void {
        pb.emit(Inst::ReturnVoid);
    } else {
        let r = pb.new_reg();
        let k = pb.konst_logic(LogicVec::zero(f.ret_width.max(1)));
        pb.emit(Inst::LoadConst { dst: r, k });
        pb.emit(Inst::Return { value: r });
    }
    pb.build()
}

fn dpi_program(f: &FuncDecl) -> Program {
    let name = f.dpi_name.as_deref().expect("DPI program without a symbol");
    let mut pb = ProgramBuilder::new(format!("dpi {name}"));
    let mut args = Vec::with_capacity(f.params.len());
    let mut modes = Vec::with_capacity(f.params.len());
    for param in &f.params {
        args.push(pb.new_reg());
        modes.push(arg_mode(param.dir));
    }
    pb.set_arg_modes(&modes);
    let args = pb.arglist(&args);
    let name = pb.konst(Value::Str(Rc::from(name)));
    let dst = pb.new_reg();
    pb.emit(Inst::DpiCall { dst, name, args });
    if f.is_void {
        pb.emit(Inst::ReturnVoid);
    } else {
        pb.emit(Inst::Return { value: dst });
    }
    pb.build()
}

fn arg_mode(dir: PortDir) -> ArgMode {
    match dir {
        PortDir::Input => ArgMode::Input,
        PortDir::Output => ArgMode::Output,
        PortDir::Inout => ArgMode::Inout,
        PortDir::Ref => ArgMode::Ref,
    }
}

/// Build a module's nets and compile its `always`/`initial` processes against
/// the global class/function tables, instantiating them with the shared linkage.
fn elaborate_module_processes(m: &Module, g: &Global, sim: &mut Sim, backend: &dyn ExecBackend) {
    let mut scope: HashMap<String, (NetId, u32)> = HashMap::new();
    for it in &m.items {
        if let ModuleItem::Var(v) = it {
            let init = match &v.init {
                Some(e) => const_eval(e).resize(v.width, v.signed),
                None => LogicVec::zero(v.width),
            };
            let net = sim.kernel().new_net(v.name.clone(), init);
            scope.insert(v.name.clone(), (net, v.width));
        }
    }

    let ts = sim.kernel().timescale();
    let gv = GlobalVars {
        vars: &g.globals,
        coll: &g.global_coll,
        class: &g.global_class,
        type_aliases: &g.global_type_aliases,
        enums: &g.enum_types,
        class_typedefs: &g.class_typedefs,
        class_collection_typedefs: &g.class_collection_typedefs,
    };
    for it in &m.items {
        match it {
            ModuleItem::Always(a) => {
                let prog = CodeGen::new(
                    &scope,
                    &g.func_ids,
                    &g.class_ids,
                    &g.class_infos,
                    &g.consts,
                    &gv,
                    ts,
                )
                .gen_always(a);
                sim.add_process(backend.instantiate(Rc::new(prog), g.linkage.clone()));
            }
            ModuleItem::Initial(body) => {
                let prog = CodeGen::new(
                    &scope,
                    &g.func_ids,
                    &g.class_ids,
                    &g.class_infos,
                    &g.consts,
                    &gv,
                    ts,
                )
                .gen_initial(body);
                sim.add_process(backend.instantiate(Rc::new(prog), g.linkage.clone()));
            }
            ModuleItem::Var(_)
            | ModuleItem::Func(_)
            | ModuleItem::Class(_)
            | ModuleItem::TypeAlias(_)
            | ModuleItem::EnumType { .. }
            | ModuleItem::EnumConst { .. } => {}
        }
    }
}

/// Per-class layout: base, field slots, method/constructor FuncIds, the
/// virtual-method table (vslot -> FuncId), the name->vslot map, and field
/// default values.
struct ClassInfo {
    name: String,
    base: Option<u32>,
    field_slot: HashMap<String, (u32, u32)>,
    /// Fields whose declared type is a class: name -> class id (for resolving
    /// `obj.field.method(...)` receiver chains).
    field_class: HashMap<String, u32>,
    /// Collection fields and their element/key class types.
    field_coll: HashMap<String, CollInfo>,
    /// Event-typed field names, initialized to a fresh identity per instance.
    field_events: HashSet<String>,
    /// Static fields: name -> global static id (shared storage).
    static_fields: HashMap<String, u32>,
    /// Static fields whose type is a class: name -> class id.
    static_field_class: HashMap<String, u32>,
    /// Static collection fields and their element/key class types.
    static_field_coll: HashMap<String, CollInfo>,
    /// Type parameters with a class default: param name -> default class id
    /// (used to resolve `type_param`-typed fields and `extends type_param`).
    type_param_default: HashMap<String, u32>,
    /// Class-scoped typedef aliases (e.g. `type_id`) -> target class id.
    type_aliases: HashMap<String, u32>,
    /// Class-scoped unpacked collection typedef aliases.
    collection_aliases: HashMap<String, CollInfo>,
    /// Methods that return a class handle: method name -> return class id
    /// (for resolving chained `a.method().next()` receivers).
    method_ret_class: HashMap<String, u32>,
    field_defaults: Vec<Value>,
    methods: HashMap<String, u32>,
    vtable: Vec<u32>,
    vslot_of: HashMap<String, u32>,
    ctor: Option<u32>,
    /// True for a synthetic class generated from `typedef struct {...}`: its
    /// struct-typed fields get an auto-constructed sub-object at `new` time
    /// instead of staying null until an explicit `= new()` (see
    /// `ClassDef::struct_fields`).
    is_struct: bool,
}

/// The default value of a class field (a collection placeholder, a null handle,
/// an empty string, or a zero bit-vector). Collection fields are given fresh
/// storage at [`Inst::New`] time, so their stored default is just `Null`.
fn default_value(fld: &VarDecl, consts: &HashMap<String, LogicVec>) -> Value {
    if fld.coll.is_some() || fld.class_name.is_some() {
        Value::Null
    } else if fld.is_event {
        Value::new_event()
    } else if fld.is_string {
        match &fld.init {
            Some(Expr::Str(value)) => Value::Str(Rc::from(value.as_str())),
            _ => Value::Str(Rc::from("")),
        }
    } else if let Some(init) = &fld.init {
        Value::Logic(const_eval_with(init, Some(consts)).resize(fld.width.max(1), fld.signed))
    } else {
        Value::Logic(LogicVec::zero(fld.width.max(1)))
    }
}

/// Compiles AST to IR. Resolves names to module nets, process-local registers,
/// or (inside a method) class fields.
struct CodeGen<'a> {
    nets: &'a HashMap<String, (NetId, u32)>,
    funcs: &'a HashMap<String, u32>,
    class_ids: &'a HashMap<String, u32>,
    classes: &'a [ClassInfo],
    /// Named compile-time constants (enum members, params).
    consts: &'a HashMap<String, LogicVec>,
    /// Package-level global variables.
    globals: &'a GlobalVars<'a>,
    /// Class id of the method being compiled (None for free functions / blocks).
    class_ctx: Option<u32>,
    this_reg: Reg,
    /// Local class-handle variables: name -> class id.
    local_classes: HashMap<String, u32>,
    /// Local collection variables and their element/key class types.
    local_colls: HashMap<String, CollInfo>,
    /// Local enum-typed variables: name -> enum table id (for `.name()`).
    local_enums: HashMap<String, u32>,
    /// Local variables whose declared type is the IEEE named-event type.
    local_events: HashSet<String>,
    locals: HashMap<String, (Reg, u32)>,
    /// `(break target, continue target)` for nested procedural loops.
    loop_targets: Vec<(Label, Label)>,
    ts: Timescale,
}

/// Borrowed package-global tables passed to the codegen.
struct GlobalVars<'a> {
    vars: &'a HashMap<String, u32>,
    coll: &'a HashMap<String, CollInfo>,
    class: &'a HashMap<String, u32>,
    /// Package/module-scope type aliases: alias name -> class id.
    type_aliases: &'a HashMap<String, u32>,
    /// Enum type name -> value-name table id.
    enums: &'a HashMap<String, u32>,
    /// Unambiguous class-scoped typedef aliases (e.g. `rsrc_q_t` inside
    /// `uvm_resource_types`): alias name -> class id. Only contains names
    /// that map to the *same* class across all classes that define them.
    class_typedefs: &'a HashMap<String, u32>,
    /// Unambiguous class-scoped unpacked collection typedef aliases.
    class_collection_typedefs: &'a HashMap<String, CollInfo>,
}

impl<'a> CodeGen<'a> {
    fn new(
        nets: &'a HashMap<String, (NetId, u32)>,
        funcs: &'a HashMap<String, u32>,
        class_ids: &'a HashMap<String, u32>,
        classes: &'a [ClassInfo],
        consts: &'a HashMap<String, LogicVec>,
        globals: &'a GlobalVars<'a>,
        ts: Timescale,
    ) -> CodeGen<'a> {
        CodeGen {
            nets,
            funcs,
            class_ids,
            classes,
            consts,
            globals,
            class_ctx: None,
            this_reg: 0,
            local_classes: HashMap::new(),
            local_colls: HashMap::new(),
            local_enums: HashMap::new(),
            local_events: HashSet::new(),
            locals: HashMap::new(),
            loop_targets: Vec::new(),
            ts,
        }
    }

    /// `always ...` — body wrapped in an infinite loop (re-arms each pass).
    fn gen_always(&mut self, a: &AlwaysBlock) -> Program {
        let mut pb = ProgramBuilder::new("always");
        let top = pb.new_label();
        pb.bind(top);
        self.gen_stmt(&a.body, &mut pb);
        pb.jump(top);
        pb.build()
    }

    /// `initial ...` — runs the body once then finishes.
    fn gen_initial(&mut self, body: &Stmt) -> Program {
        let mut pb = ProgramBuilder::new("initial");
        self.gen_stmt(body, &mut pb);
        pb.emit(Inst::Finish);
        pb.build()
    }

    /// Compile a function/task/method body. For a method (`class_ctx` set),
    /// register 0 is the implicit `this`; remaining formals follow. An extra
    /// register holds the return value (bound to the function name for the
    /// `funcname = expr` form). An implicit return is appended.
    fn gen_callable(&mut self, f: &FuncDecl, class_ctx: Option<u32>) -> Program {
        let label = match class_ctx {
            Some(cid) => format!("{}::{}", self.classes[cid as usize].name, f.name),
            None => format!("func {}", f.name),
        };
        let mut pb = ProgramBuilder::new(label);
        self.locals.clear();
        self.local_classes.clear();
        self.local_colls.clear();
        self.local_enums.clear();
        self.local_events.clear();
        self.class_ctx = class_ctx;
        let mut arg_modes = Vec::with_capacity(f.params.len() + usize::from(class_ctx.is_some()));
        if class_ctx.is_some() {
            self.this_reg = pb.new_reg(); // register 0 = this
            arg_modes.push(ArgMode::Input);
        }
        for p in &f.params {
            let reg = pb.new_reg();
            arg_modes.push(arg_mode(p.dir));
            self.locals.insert(p.name.clone(), (reg, p.width));
            let collection = p.coll.map(|kind| CollInfo {
                kind,
                elem_class: p
                    .class_name
                    .as_deref()
                    .and_then(|name| self.resolve_class(name)),
                key_class: p
                    .key_class_name
                    .as_deref()
                    .and_then(|name| self.resolve_class(name)),
            });
            let collection = collection.or_else(|| {
                p.class_name
                    .as_deref()
                    .and_then(|name| self.resolve_collection_alias(p.type_scope.as_deref(), name))
            });
            if let Some(info) = collection {
                self.local_colls.insert(p.name.clone(), info);
                continue;
            }
            if let Some(cn) = &p.class_name {
                if let Some(cid) = self.resolve_class(cn) {
                    self.local_classes.insert(p.name.clone(), cid);
                } else if let Some(&eid) = self.globals.enums.get(cn) {
                    self.local_enums.insert(p.name.clone(), eid);
                }
            }
        }
        pb.set_arg_modes(&arg_modes);
        let first_formal = u32::from(class_ctx.is_some());
        for (offset, param) in f.params.iter().enumerate() {
            let Some(default) = &param.default else {
                continue;
            };
            let formal = first_formal + offset as u32;
            let supplied = pb.new_label();
            pb.branch_arg_provided(formal, supplied);
            let value = self.gen_expr(default, &mut pb);
            pb.emit(Inst::Assign {
                dst: formal,
                src: value,
            });
            pb.bind(supplied);
        }
        let ret_width = f.ret_width.max(1);
        let ret_reg = pb.new_reg();
        self.locals.insert(f.name.clone(), (ret_reg, ret_width));
        // A class-returning function may assign its result via the function-name
        // variable (`funcname = new(...)`); register its return class so the
        // `new` / member accesses resolve.
        if let Some(rc) = &f.ret_class {
            if let Some(cid) = self.resolve_class(rc) {
                self.local_classes.insert(f.name.clone(), cid);
            }
        }
        let k = pb.konst_logic(LogicVec::zero(ret_width));
        pb.emit(Inst::LoadConst { dst: ret_reg, k });
        self.gen_stmt(&f.body, &mut pb);
        if f.is_void {
            pb.emit(Inst::ReturnVoid);
        } else {
            pb.emit(Inst::Return { value: ret_reg });
        }
        pb.build()
    }

    /// Compile one `fork` branch as its own standalone program (an
    /// independent concurrent process, not a callee frame — it ends with
    /// `Inst::Finish`, not `Return`). It gets a *fresh* register file and
    /// local-variable scope: SystemVerilog runs each branch as a genuinely
    /// concurrent process, so it cannot use the parent's register file
    /// directly. Enclosing locals and `this` receive fresh child registers
    /// plus capture mappings sampled when `Inst::Fork` executes. Object and
    /// collection handles preserve shared identity across the processes.
    fn gen_fork_branch(&mut self, body: &Stmt) -> (Rc<Program>, Vec<(Reg, Reg)>) {
        let label = match self.class_ctx {
            Some(cid) => format!("{}::<fork>", self.classes[cid as usize].name),
            None => "<fork>".to_string(),
        };
        let mut pb = ProgramBuilder::new(label);
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_classes = std::mem::take(&mut self.local_classes);
        let saved_colls = std::mem::take(&mut self.local_colls);
        let saved_enums = std::mem::take(&mut self.local_enums);
        let saved_events = std::mem::take(&mut self.local_events);
        let saved_loop_targets = std::mem::take(&mut self.loop_targets);
        let saved_this_reg = self.this_reg;
        let mut captures = Vec::with_capacity(saved_locals.len() + 1);
        if self.class_ctx.is_some() {
            self.this_reg = pb.new_reg(); // reg 0 = this, seeded by the scheduler at spawn
            captures.push((self.this_reg, saved_this_reg));
        }
        let mut locals: Vec<_> = saved_locals.iter().collect();
        locals.sort_by_key(|(_, value)| value.0);
        for (name, &(parent_reg, width)) in locals {
            let child_reg = pb.new_reg();
            self.locals.insert(name.clone(), (child_reg, width));
            captures.push((child_reg, parent_reg));
        }
        self.local_classes = saved_classes.clone();
        self.local_colls = saved_colls.clone();
        self.local_enums = saved_enums.clone();
        self.local_events = saved_events.clone();
        self.gen_stmt(body, &mut pb);
        pb.emit(Inst::Finish);
        let prog = pb.build();
        self.locals = saved_locals;
        self.local_classes = saved_classes;
        self.local_colls = saved_colls;
        self.local_enums = saved_enums;
        self.local_events = saved_events;
        self.loop_targets = saved_loop_targets;
        self.this_reg = saved_this_reg;
        (Rc::new(prog), captures)
    }

    fn gen_stmt(&mut self, s: &Stmt, pb: &mut ProgramBuilder) {
        match s {
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.gen_stmt(s, pb);
                }
            }
            Stmt::VarDecl(v) => {
                let reg = pb.new_reg();
                self.locals.insert(v.name.clone(), (reg, v.width));
                if v.is_event {
                    self.local_events.insert(v.name.clone());
                    match &v.init {
                        Some(init) => {
                            let src = self.gen_expr(init, pb);
                            pb.emit(Inst::Assign { dst: reg, src });
                        }
                        None => pb.emit(Inst::NewEvent { dst: reg }),
                    }
                } else if let Some(info) = v
                    .coll
                    .map(|kind| CollInfo {
                        kind,
                        elem_class: v
                            .class_name
                            .as_deref()
                            .and_then(|name| self.resolve_class(name)),
                        key_class: v
                            .key_class_name
                            .as_deref()
                            .and_then(|name| self.resolve_class(name)),
                    })
                    .or_else(|| {
                        v.class_name.as_deref().and_then(|name| {
                            self.resolve_collection_alias(v.type_scope.as_deref(), name)
                        })
                    })
                {
                    self.local_events.remove(&v.name);
                    // Queue / dynamic array / associative array: initialize to
                    // fresh empty storage and record its kind + element class.
                    self.local_colls.insert(v.name.clone(), info);
                    match info.kind {
                        CollKind::Assoc => pb.emit(Inst::NewAssoc { dst: reg }),
                        CollKind::Queue => pb.emit(Inst::NewQueue { dst: reg }),
                    }
                } else if let Some(cn) = &v.class_name {
                    // Class handle: record its declared class so later member
                    // accesses resolve. A fresh frame register is already null.
                    if let Some(cid) = self.resolve_class(cn) {
                        self.local_classes.insert(v.name.clone(), cid);
                    } else if let Some(&eid) = self.globals.enums.get(cn) {
                        // An enum-typed local (e.g. `uvm_severity l_severity;`):
                        // track its enum table for `.name()`.
                        self.local_enums.insert(v.name.clone(), eid);
                    }
                    // Honor an initializer: `T h = new(...)`, `T h = T::get()`,
                    // `T h = other`, `T h = null`.
                    match &v.init {
                        Some(Expr::New { args }) => self.gen_new(&v.name, args, pb),
                        Some(e) => {
                            let init = self.gen_expr(e, pb);
                            pb.emit(Inst::Assign {
                                dst: reg,
                                src: init,
                            });
                        }
                        None => {}
                    }
                } else if v.is_string {
                    let init = match &v.init {
                        Some(e) => self.gen_expr(e, pb),
                        None => {
                            let dst = pb.new_reg();
                            let k = pb.konst(Value::Str(Rc::from("")));
                            pb.emit(Inst::LoadConst { dst, k });
                            dst
                        }
                    };
                    pb.emit(Inst::Assign {
                        dst: reg,
                        src: init,
                    });
                } else {
                    let init = match &v.init {
                        Some(e) => self.gen_expr(e, pb),
                        None => {
                            // 2-state default is 0.
                            let dst = pb.new_reg();
                            let k = pb.konst_logic(LogicVec::zero(v.width));
                            pb.emit(Inst::LoadConst { dst, k });
                            dst
                        }
                    };
                    pb.emit(Inst::Assign {
                        dst: reg,
                        src: init,
                    });
                }
            }
            Stmt::Timed { control, body } => {
                self.gen_timing(control, pb);
                self.gen_stmt(body, pb);
            }
            Stmt::Blocking { lhs, rhs } => {
                if let Expr::New { args } = rhs {
                    if let Some(index) = &lhs.index {
                        // `collection[key] = new(...)` — create a new element
                        // and store it at the given index.
                        let base_expr = Expr::Ref(lhs.name.clone());
                        if let Some(cid) = self.coll_elem_class(&base_expr) {
                            let obj = pb.new_reg();
                            pb.emit(Inst::New {
                                dst: obj,
                                class: cid,
                            });
                            let ctor = self.classes[cid as usize].ctor;
                            if let Some(ctor_fid) = ctor {
                                let mut arg_regs = vec![obj];
                                for a in args {
                                    arg_regs.push(self.gen_expr(a, pb));
                                }
                                let arglist = pb.arglist(&arg_regs);
                                let ret = pb.new_reg();
                                pb.emit(Inst::Call {
                                    func: ctor_fid,
                                    args: arglist,
                                    ret,
                                });
                            }
                            let base = self.gen_expr(&base_expr, pb);
                            let idx = self.gen_expr(index, pb);
                            pb.emit(Inst::IndexSet {
                                base,
                                idx,
                                src: obj,
                            });
                        } else {
                            panic!("`new` assigned to indexed non-class-handle '{}'", lhs.name)
                        }
                    } else {
                        self.gen_new(&lhs.name, args, pb);
                    }
                } else {
                    let r = self.gen_expr(rhs, pb);
                    self.gen_assign_lvalue(lhs, r, false, pb);
                }
            }
            Stmt::Nonblocking { lhs, rhs } => {
                let r = self.gen_expr(rhs, pb);
                self.gen_assign_lvalue(lhs, r, true, pb);
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.gen_expr(cond, pb);
                let else_lbl = pb.new_label();
                pb.branch_false(c, else_lbl);
                self.gen_stmt(then_branch, pb);
                match else_branch {
                    Some(els) => {
                        let end_lbl = pb.new_label();
                        pb.jump(end_lbl);
                        pb.bind(else_lbl);
                        self.gen_stmt(els, pb);
                        pb.bind(end_lbl);
                    }
                    None => pb.bind(else_lbl),
                }
            }
            Stmt::While { cond, body } => {
                let top = pb.new_label();
                let done = pb.new_label();
                pb.bind(top);
                let value = self.gen_expr(cond, pb);
                pb.branch_false(value, done);
                self.loop_targets.push((done, top));
                self.gen_stmt(body, pb);
                self.loop_targets.pop();
                pb.jump(top);
                pb.bind(done);
            }
            Stmt::DoWhile { cond, body } => {
                let top = pb.new_label();
                let check = pb.new_label();
                let done = pb.new_label();
                pb.bind(top);
                self.loop_targets.push((done, check));
                self.gen_stmt(body, pb);
                self.loop_targets.pop();
                pb.bind(check);
                let value = self.gen_expr(cond, pb);
                pb.branch_true(value, top);
                pb.bind(done);
            }
            Stmt::Case {
                expr,
                items,
                default,
            } => {
                let selector = self.gen_expr(expr, pb);
                let done = pb.new_label();
                let labels: Vec<_> = items.iter().map(|_| pb.new_label()).collect();
                let default_label = default.as_ref().map(|_| pb.new_label());
                for ((values, _), &label) in items.iter().zip(&labels) {
                    for value in values {
                        let candidate = self.gen_expr(value, pb);
                        let equal = pb.new_reg();
                        pb.emit(Inst::Eq {
                            dst: equal,
                            a: selector,
                            b: candidate,
                        });
                        pb.branch_true(equal, label);
                    }
                }
                pb.jump(default_label.unwrap_or(done));
                for ((_, body), &label) in items.iter().zip(&labels) {
                    pb.bind(label);
                    self.gen_stmt(body, pb);
                    pb.jump(done);
                }
                if let (Some(body), Some(label)) = (default, default_label) {
                    pb.bind(label);
                    self.gen_stmt(body, pb);
                }
                pb.bind(done);
            }
            Stmt::Foreach {
                collection,
                index,
                body,
            } => {
                let info = self
                    .coll_of_receiver(collection)
                    .unwrap_or_else(|| panic!("foreach receiver is not a collection"));
                let base = self.gen_expr(collection, pb);
                let index_reg = pb.new_reg();
                let previous_local = self.locals.insert(index.clone(), (index_reg, 32));
                let previous_class = match info.key_class {
                    Some(class) => self.local_classes.insert(index.clone(), class),
                    None => self.local_classes.remove(index),
                };

                let args = pb.arglist(&[index_reg]);
                let has_item = pb.new_reg();
                pb.emit(Inst::CollMethod {
                    dst: has_item,
                    base,
                    op: CollOp::First,
                    args,
                });
                let check = pb.new_label();
                let advance = pb.new_label();
                let done = pb.new_label();
                pb.bind(check);
                pb.branch_false(has_item, done);
                self.loop_targets.push((done, advance));
                self.gen_stmt(body, pb);
                self.loop_targets.pop();
                pb.bind(advance);
                pb.emit(Inst::CollMethod {
                    dst: has_item,
                    base,
                    op: CollOp::Next,
                    args,
                });
                pb.jump(check);
                pb.bind(done);

                match previous_local {
                    Some(local) => {
                        self.locals.insert(index.clone(), local);
                    }
                    None => {
                        self.locals.remove(index);
                    }
                }
                match previous_class {
                    Some(class) => {
                        self.local_classes.insert(index.clone(), class);
                    }
                    None => {
                        self.local_classes.remove(index);
                    }
                }
            }
            Stmt::SysCall { name, args } => self.gen_sys_call(name, args, pb),
            Stmt::Return(val) => match val {
                Some(e) => {
                    let r = self.gen_expr(e, pb);
                    pb.emit(Inst::Return { value: r });
                }
                None => pb.emit(Inst::ReturnVoid),
            },
            Stmt::Break => {
                let (target, _) = self
                    .loop_targets
                    .last()
                    .copied()
                    .expect("break used outside a loop");
                pb.jump(target);
            }
            Stmt::Continue => {
                let (_, target) = self
                    .loop_targets
                    .last()
                    .copied()
                    .expect("continue used outside a loop");
                pb.jump(target);
            }
            Stmt::Expr(e) => {
                self.gen_expr(e, pb);
            }
            Stmt::Trigger(event) => {
                let event = self.gen_expr(event, pb);
                pb.emit(Inst::TriggerEvent { event });
            }
            Stmt::Fork { branches, join } => {
                let ids: Vec<u32> = branches
                    .iter()
                    .map(|b| {
                        let (prog, captures) = self.gen_fork_branch(b);
                        pb.add_fork_branch(prog, &captures)
                    })
                    .collect();
                let group = pb.fork_group(&ids);
                let join = match join {
                    eevee_ast::ForkJoin::All => ForkJoin::All,
                    eevee_ast::ForkJoin::Any => ForkJoin::Any,
                    eevee_ast::ForkJoin::None => ForkJoin::None,
                };
                pb.emit(Inst::Fork { group, join });
            }
            Stmt::Null => {}
        }
    }

    fn gen_sys_call(&mut self, name: &str, args: &[Expr], pb: &mut ProgramBuilder) {
        match name {
            "$display" | "$write" => {
                // The format string is the first arg when it is a string
                // literal; otherwise synthesize a default decimal format.
                let (fmt, val_exprs): (String, &[Expr]) = match args.split_first() {
                    Some((Expr::Str(s), rest)) => (s.clone(), rest),
                    _ => (default_fmt(args.len()), args),
                };
                let regs: Vec<Reg> = val_exprs.iter().map(|e| self.gen_expr(e, pb)).collect();
                let fmt_const = pb.konst(Value::Str(Rc::from(fmt.as_str())));
                let arglist = pb.arglist(&regs);
                pb.emit(Inst::Display {
                    fmt: fmt_const,
                    args: arglist,
                });
            }
            // `$swrite(var, fmt, args...)` / `$sformat(var, fmt, args...)`:
            // format into a string and assign to the first (l-value) argument.
            "$swrite" | "$sformat" => {
                if let Some((Expr::Ref(dst_name), rest)) = args.split_first() {
                    let (fmt, val_exprs): (String, &[Expr]) = match rest.split_first() {
                        Some((Expr::Str(s), vs)) => (s.clone(), vs),
                        _ => (default_fmt(rest.len()), rest),
                    };
                    let regs: Vec<Reg> = val_exprs.iter().map(|e| self.gen_expr(e, pb)).collect();
                    let fmt_const = pb.konst(Value::Str(Rc::from(fmt.as_str())));
                    let arglist = pb.arglist(&regs);
                    let tmp = pb.new_reg();
                    pb.emit(Inst::Format {
                        dst: tmp,
                        fmt: fmt_const,
                        args: arglist,
                    });
                    self.gen_assign(dst_name, tmp, false, pb);
                }
            }
            // `$cast(dst, src)` as a statement uses the same dynamic class
            // compatibility check as its function form.
            "$cast" if args.len() == 2 => {
                let _ = self.gen_cast(args, pb);
            }
            // Other system tasks are ignored for now (graceful no-op).
            _ => {}
        }
    }

    fn gen_timing(&mut self, c: &TimingControl, pb: &mut ProgramBuilder) {
        match c {
            TimingControl::Delay(e) => {
                let fs = self.ts.delay_to_fs(const_eval(e).to_u64() as f64);
                pb.emit(Inst::Delay { fs });
            }
            TimingControl::Event(evs) => {
                assert_eq!(
                    evs.len(),
                    1,
                    "multi-signal sensitivity not yet supported (needs a WaitAny opcode)"
                );
                let ev = &evs[0];
                if let Expr::Ref(name) = &ev.expr {
                    if let Some(&(net, _)) = self.nets.get(name) {
                        let edge = match ev.edge {
                            Edge::Posedge => EdgeKind::Posedge,
                            Edge::Negedge => EdgeKind::Negedge,
                            Edge::AnyChange => EdgeKind::AnyChange,
                        };
                        pb.emit(Inst::WaitEdge { net, edge });
                        return;
                    }
                }
                let event = self.gen_expr(&ev.expr, pb);
                if self.is_named_event_expr(&ev.expr) {
                    pb.emit(Inst::WaitEvent { event });
                } else {
                    pb.emit(Inst::WaitChange { value: event });
                }
            }
            TimingControl::Wait(cond) => {
                let check = pb.new_label();
                let ready = pb.new_label();
                pb.bind(check);
                let value = self.gen_expr(cond, pb);
                pb.branch_true(value, ready);
                pb.emit(Inst::WaitRuntime);
                pb.jump(check);
                pb.bind(ready);
            }
        }
    }

    /// Generate code for `e`, returning the register holding its value.
    fn gen_expr(&mut self, e: &Expr, pb: &mut ProgramBuilder) -> Reg {
        match e {
            Expr::Literal(lv) => {
                let dst = pb.new_reg();
                let k = pb.konst_logic(lv.clone());
                pb.emit(Inst::LoadConst { dst, k });
                dst
            }
            Expr::Str(s) => {
                let dst = pb.new_reg();
                let k = pb.konst(Value::Str(Rc::from(s.as_str())));
                pb.emit(Inst::LoadConst { dst, k });
                dst
            }
            Expr::Ref(name) => {
                if name == "this" {
                    self.this_reg
                } else if let Some((reg, _)) = self.locals.get(name).copied() {
                    // Local: the register *is* the value (read in place).
                    reg
                } else if let Some((slot, _)) = self.class_field(name) {
                    // Unqualified field inside a method -> this.field.
                    let this = self.this_reg;
                    let dst = pb.new_reg();
                    pb.emit(Inst::GetField {
                        dst,
                        obj: this,
                        slot,
                    });
                    dst
                } else if let Some(id) = self.static_field(name) {
                    // Static class field -> shared global storage.
                    let dst = pb.new_reg();
                    pb.emit(Inst::StaticGet { dst, id });
                    dst
                } else if let Some(&id) = self.globals.vars.get(name) {
                    // Package-level global variable -> shared global storage.
                    let dst = pb.new_reg();
                    pb.emit(Inst::StaticGet { dst, id });
                    dst
                } else if let Some(cv) = self.consts.get(name) {
                    // Enum member / named constant.
                    let dst = pb.new_reg();
                    let k = pb.konst_logic(cv.clone());
                    pb.emit(Inst::LoadConst { dst, k });
                    dst
                } else if self.method_of_ctx(name) {
                    // Unqualified zero-arg method of the current class -> this.m().
                    self.gen_method_on_this(name, &[], pb)
                } else {
                    let net = self.net(name);
                    let dst = pb.new_reg();
                    pb.emit(Inst::NetRead { dst, net });
                    dst
                }
            }
            Expr::Unary { op, operand } => {
                let a = self.gen_expr(operand, pb);
                let dst = pb.new_reg();
                match op {
                    UnaryOp::BitNot => pb.emit(Inst::Not { dst, a }),
                    UnaryOp::LogNot => pb.emit(Inst::LogNot { dst, a }),
                    UnaryOp::Neg => pb.emit(Inst::Neg { dst, a }),
                    UnaryOp::Plus => pb.emit(Inst::Mov { dst, src: a }),
                    UnaryOp::ReduceAnd => pb.emit(Inst::ReduceAnd { dst, a }),
                    UnaryOp::ReduceOr => pb.emit(Inst::ReduceOr { dst, a }),
                    UnaryOp::ReduceXor => pb.emit(Inst::ReduceXor { dst, a }),
                }
                dst
            }
            Expr::Binary { op, lhs, rhs } => {
                let a = self.gen_expr(lhs, pb);
                let b = self.gen_expr(rhs, pb);
                let dst = pb.new_reg();
                let inst = match op {
                    BinOp::Add => Inst::Add { dst, a, b },
                    BinOp::Sub => Inst::Sub { dst, a, b },
                    BinOp::Mul => Inst::Mul { dst, a, b },
                    BinOp::And => Inst::And { dst, a, b },
                    BinOp::Or => Inst::Or { dst, a, b },
                    BinOp::Xor => Inst::Xor { dst, a, b },
                    BinOp::Eq => Inst::Eq { dst, a, b },
                    BinOp::Neq => Inst::Neq { dst, a, b },
                    BinOp::Lt => Inst::Lt { dst, a, b },
                    BinOp::Gt => Inst::Gt { dst, a, b },
                    BinOp::Le => Inst::Le { dst, a, b },
                    BinOp::Ge => Inst::Ge { dst, a, b },
                    BinOp::Shl => Inst::Shl { dst, a, b },
                    BinOp::Shr => Inst::Shr { dst, a, b },
                    BinOp::LogAnd => Inst::LogAnd { dst, a, b },
                    BinOp::LogOr => Inst::LogOr { dst, a, b },
                };
                pb.emit(inst);
                dst
            }
            Expr::Call { name, args } => {
                if self.method_of_ctx(name) {
                    // Class-scope lookup precedes compilation-unit functions:
                    // an unqualified `f()` in a method means `this.f()` when
                    // the class has a member named `f`.
                    self.gen_method_on_this(name, args, pb)
                } else if let Some(&fid) = self.funcs.get(name) {
                    let arg_regs: Vec<Reg> = args.iter().map(|a| self.gen_expr(a, pb)).collect();
                    let arglist = pb.arglist(&arg_regs);
                    let ret = pb.new_reg();
                    pb.emit(Inst::Call {
                        func: fid,
                        args: arglist,
                        ret,
                    });
                    ret
                } else {
                    panic!("call to unknown function '{name}'")
                }
            }
            Expr::MethodCall { obj, method, args } => {
                // `super.m(...)` / `super.new(...)`: static dispatch to the base
                // class's method, on the current `this`.
                if matches!(&**obj, Expr::Ref(n) if n == "super") {
                    let base = self
                        .class_ctx
                        .and_then(|cid| self.classes[cid as usize].base)
                        .expect("`super` used without a base class");
                    let fid = if method == "new" {
                        self.classes[base as usize]
                            .ctor
                            .expect("base class has no constructor")
                    } else {
                        self.classes[base as usize]
                            .methods
                            .get(method)
                            .copied()
                            .unwrap_or_else(|| panic!("base class has no method '{method}'"))
                    };
                    let this = self.this_reg;
                    let mut arg_regs = vec![this];
                    for a in args {
                        arg_regs.push(self.gen_expr(a, pb));
                    }
                    let arglist = pb.arglist(&arg_regs);
                    let ret = pb.new_reg();
                    pb.emit(Inst::Call {
                        func: fid,
                        args: arglist,
                        ret,
                    });
                    return ret;
                }

                // Built-in queue/array/assoc method on a collection receiver.
                if self.coll_of_receiver(obj).is_some() {
                    if let Some(op) = coll_op(method) {
                        let base_reg = self.gen_expr(obj, pb);
                        let arg_regs: Vec<Reg> =
                            args.iter().map(|a| self.gen_expr(a, pb)).collect();
                        let arglist = pb.arglist(&arg_regs);
                        let dst = pb.new_reg();
                        pb.emit(Inst::CollMethod {
                            dst,
                            base: base_reg,
                            op,
                            args: arglist,
                        });
                        return dst;
                    }
                }

                // Built-in enum / string methods on a non-class receiver.
                if let Some(r) = self.gen_builtin_method(obj, method, args, pb) {
                    return r;
                }

                let obj_reg = self.gen_expr(obj, pb);

                // If the receiver is a local/param that isn't a class handle
                // (e.g. a dynamic-array parameter like `ref bit value[]`),
                // try a collection dispatch before falling through.
                let is_bare_ref = matches!(obj.as_ref(), Expr::Ref(_));
                if is_bare_ref && self.try_class_of(obj).is_none() {
                    if let Some(op) = coll_op(method) {
                        let arg_regs: Vec<Reg> =
                            args.iter().map(|a| self.gen_expr(a, pb)).collect();
                        let coll_args = pb.arglist(&arg_regs);
                        let dst = pb.new_reg();
                        pb.emit(Inst::CollMethod {
                            dst,
                            base: obj_reg,
                            op,
                            args: coll_args,
                        });
                        return dst;
                    }
                    panic!(
                        "'{name}' is not a class handle",
                        name = if let Expr::Ref(n) = obj.as_ref() {
                            n.as_str()
                        } else {
                            "<expr>"
                        }
                    );
                }

                let cid = self.receiver_class(obj);
                let mut arg_regs = vec![obj_reg];
                for a in args {
                    arg_regs.push(self.gen_expr(a, pb));
                }
                let arglist = pb.arglist(&arg_regs);
                let ret = pb.new_reg();
                // Virtual dispatch when the method is virtual in the receiver's
                // static class; otherwise a direct call.
                if let Some(&vslot) = self.classes[cid as usize].vslot_of.get(method) {
                    pb.emit(Inst::CallVirtual {
                        obj: obj_reg,
                        vslot,
                        args: arglist,
                        ret,
                    });
                } else if let Some(fid) = self.classes[cid as usize].methods.get(method).copied() {
                    pb.emit(Inst::Call {
                        func: fid,
                        args: arglist,
                        ret,
                    });
                } else if let Some(op) = coll_op(method) {
                    // The receiver resolved to a class that lacks this method,
                    // but the method is a collection builtin — e.g. a nested
                    // associative array `m[a][b]` whose element is modeled as
                    // its leaf class. Dispatch as a collection method on the
                    // receiver value (a non-collection value yields null/0).
                    let coll_args = pb.arglist(&arg_regs[1..]);
                    pb.emit(Inst::CollMethod {
                        dst: ret,
                        base: obj_reg,
                        op,
                        args: coll_args,
                    });
                } else {
                    panic!(
                        "no method '{method}' on class '{}'",
                        self.classes[cid as usize].name
                    );
                }
                ret
            }
            Expr::Field { obj, field } => {
                let obj_reg = self.gen_expr(obj, pb);
                let cid = self.receiver_class(obj);
                // Instance field (most common).
                if let Some(&(slot, _)) = self.classes[cid as usize].field_slot.get(field) {
                    let dst = pb.new_reg();
                    pb.emit(Inst::GetField {
                        dst,
                        obj: obj_reg,
                        slot,
                    });
                    return dst;
                }
                // Static field accessed via a class handle (`obj.static_field`).
                if let Some(&id) = self.classes[cid as usize].static_fields.get(field) {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StaticGet { dst, id });
                    return dst;
                }
                panic!("no field '{field}' on the receiver class")
            }
            Expr::Index { base, index } => {
                // String indexing: str[i] -> byte value at position i.
                if let Expr::Ref(name) = base.as_ref() {
                    if self.is_string_local(name) {
                        let src = self.gen_expr(base, pb);
                        let idx = self.gen_expr(index, pb);
                        let dst = pb.new_reg();
                        pb.emit(Inst::StringIndex { dst, src, idx });
                        return dst;
                    }
                }
                let base_reg = self.gen_expr(base, pb);
                let idx = self.gen_expr(index, pb);
                let dst = pb.new_reg();
                pb.emit(Inst::IndexGet {
                    dst,
                    base: base_reg,
                    idx,
                });
                dst
            }
            Expr::PartSelect { base, left, right } => {
                let base = self.gen_expr(base, pb);
                let left = self.gen_expr(left, pb);
                let right = self.gen_expr(right, pb);
                let dst = pb.new_reg();
                pb.emit(Inst::PartSelect {
                    dst,
                    base,
                    left,
                    right,
                });
                dst
            }
            Expr::StaticCall {
                class_name,
                method,
                args,
                ..
            } => {
                // `pkg::func(...)` resolves to a free function when the scope is
                // not a class (e.g. `uvm_pkg::...`).
                if self.resolve_class(class_name).is_none() {
                    if let Some(&fid) = self.funcs.get(method) {
                        let arg_regs: Vec<Reg> =
                            args.iter().map(|a| self.gen_expr(a, pb)).collect();
                        let arglist = pb.arglist(&arg_regs);
                        let ret = pb.new_reg();
                        pb.emit(Inst::Call {
                            func: fid,
                            args: arglist,
                            ret,
                        });
                        return ret;
                    }
                    // Package-scope global variable (e.g. `uvm_pkg::uvm_deferred_init`).
                    if let Some(&id) = self.globals.vars.get(method.as_str()) {
                        let dst = pb.new_reg();
                        pb.emit(Inst::StaticGet { dst, id });
                        return dst;
                    }
                    // IEEE-1800 builtin classes (`process`, ...) have no SV source.
                    if let Some(r) = self.gen_builtin_static(class_name, method, pb) {
                        return r;
                    }
                }
                let cid = self
                    .resolve_class(class_name)
                    .unwrap_or_else(|| panic!("static call on unknown class '{class_name}'"));
                let fid = self.classes[cid as usize]
                    .methods
                    .get(method)
                    .copied()
                    .unwrap_or_else(|| {
                        panic!("no static method '{method}' on class '{class_name}'")
                    });
                // A static method has no real receiver; pass a null `this`.
                let this = pb.new_reg();
                let k = pb.konst(Value::Null);
                pb.emit(Inst::LoadConst { dst: this, k });
                let mut arg_regs = vec![this];
                for a in args {
                    arg_regs.push(self.gen_expr(a, pb));
                }
                let arglist = pb.arglist(&arg_regs);
                let ret = pb.new_reg();
                pb.emit(Inst::Call {
                    func: fid,
                    args: arglist,
                    ret,
                });
                ret
            }
            Expr::StaticRef { class_name, field } => {
                // First try: package-scope global var (for `pkg::var` references).
                if let Some(&id) = self.globals.vars.get(field.as_str()) {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StaticGet { dst, id });
                    return dst;
                }
                // Second try: enum/named constant in the const table.
                if let Some(cv) = self.consts.get(field.as_str()) {
                    let dst = pb.new_reg();
                    let k = pb.konst_logic(cv.clone());
                    pb.emit(Inst::LoadConst { dst, k });
                    return dst;
                }
                // Third: static field of the named class.
                if let Some(cid) = self.resolve_class(class_name) {
                    if let Some(&id) = self.classes[cid as usize].static_fields.get(field.as_str())
                    {
                        let dst = pb.new_reg();
                        pb.emit(Inst::StaticGet { dst, id });
                        return dst;
                    }
                }
                // Fallback: treat as a bare Ref (resolves package enum constants
                // like `uvm_pkg::UVM_LOW` whose class lookup would fail).
                let dst = pb.new_reg();
                if let Some(cv) = self.consts.get(class_name.as_str()) {
                    // The "class" segment was actually an enum value name.
                    let k = pb.konst_logic(cv.clone());
                    pb.emit(Inst::LoadConst { dst, k });
                } else {
                    // Last resort — treat the field name as a bare signal/const.
                    return self.gen_expr(&Expr::Ref(field.clone()), pb);
                }
                dst
            }
            Expr::New { .. } => {
                panic!("`new` is only supported as `handle = new(...)` for now")
            }
            Expr::Null => {
                let dst = pb.new_reg();
                let k = pb.konst(Value::Null);
                pb.emit(Inst::LoadConst { dst, k });
                dst
            }
            Expr::Concat(parts) => {
                let regs: Vec<Reg> = parts.iter().map(|e| self.gen_expr(e, pb)).collect();
                let arglist = pb.arglist(&regs);
                let dst = pb.new_reg();
                pb.emit(Inst::Concat { dst, args: arglist });
                dst
            }
            Expr::SysCall { name, args } => self.gen_sys_func(name, args, pb),
        }
    }

    /// A system function in expression position: `$sformatf`, `$realtime`,
    /// `$time`, `$cast`, `$bits`, ... Returns the register with its result.
    fn gen_sys_func(&mut self, name: &str, args: &[Expr], pb: &mut ProgramBuilder) -> Reg {
        match name {
            "$sformatf" | "$psprintf" => {
                let (fmt, rest): (String, &[Expr]) = match args.split_first() {
                    Some((Expr::Str(s), rest)) => (s.clone(), rest),
                    _ => (default_fmt(args.len()), args),
                };
                let regs: Vec<Reg> = rest.iter().map(|e| self.gen_expr(e, pb)).collect();
                let fmt_const = pb.konst(Value::Str(Rc::from(fmt.as_str())));
                let arglist = pb.arglist(&regs);
                let dst = pb.new_reg();
                pb.emit(Inst::Format {
                    dst,
                    fmt: fmt_const,
                    args: arglist,
                });
                dst
            }
            "$realtime" | "$time" | "$stime" => {
                let dst = pb.new_reg();
                pb.emit(Inst::SimTime { dst });
                dst
            }
            // `$typename(T)` — after mono the type param T is already a Str
            // constant; just return the first arg as a string value.
            "$typename" => match args.first() {
                Some(e) => self.gen_expr(e, pb),
                None => {
                    let dst = pb.new_reg();
                    let k = pb.konst(Value::Str(Rc::from("")));
                    pb.emit(Inst::LoadConst { dst, k });
                    dst
                }
            },
            "$cast" if args.len() == 2 => self.gen_cast(args, pb),
            // Unknown system function: yield 0 (graceful).
            _ => {
                for a in args {
                    let _ = self.gen_expr(a, pb);
                }
                let dst = pb.new_reg();
                let k = pb.konst_logic(LogicVec::zero(32));
                pb.emit(Inst::LoadConst { dst, k });
                dst
            }
        }
    }

    fn gen_cast(&mut self, args: &[Expr], pb: &mut ProgramBuilder) -> Reg {
        let src = self.gen_expr(&args[1], pb);
        let success = pb.new_reg();
        if let Expr::Ref(name) = &args[0] {
            if let Some(class) = self.try_class_of(&args[0]) {
                pb.emit(Inst::ClassCast {
                    dst: success,
                    src,
                    class,
                });
                let done = pb.new_label();
                pb.branch_false(success, done);
                self.gen_assign(name, src, false, pb);
                pb.bind(done);
                return success;
            }

            self.gen_assign(name, src, false, pb);
            let one = pb.konst_logic(LogicVec::from_u64(1, 1));
            pb.emit(Inst::LoadConst {
                dst: success,
                k: one,
            });
            return success;
        }

        let zero = pb.konst_logic(LogicVec::zero(1));
        pb.emit(Inst::LoadConst {
            dst: success,
            k: zero,
        });
        success
    }

    /// `handle = new(args)`: allocate the object and run its constructor.
    fn gen_new(&mut self, lhs: &str, args: &[Expr], pb: &mut ProgramBuilder) {
        let cid = self
            .local_classes
            .get(lhs)
            .copied()
            .or_else(|| self.field_class(lhs))
            .or_else(|| self.static_field_class(lhs))
            .or_else(|| self.globals.class.get(lhs).copied())
            .unwrap_or_else(|| panic!("`new` assigned to non-class-handle '{lhs}'"));
        let obj = pb.new_reg();
        pb.emit(Inst::New {
            dst: obj,
            class: cid,
        });
        let ctor = self.classes[cid as usize].ctor;
        if let Some(ctor_fid) = ctor {
            let mut arg_regs = vec![obj];
            for a in args {
                arg_regs.push(self.gen_expr(a, pb));
            }
            let arglist = pb.arglist(&arg_regs);
            let ret = pb.new_reg();
            pb.emit(Inst::Call {
                func: ctor_fid,
                args: arglist,
                ret,
            });
        }
        // Store the constructed handle into the l-value (local / field / static).
        self.gen_assign(lhs, obj, false, pb);
    }

    /// A field of the class currently being compiled (when inside a method).
    fn class_field(&self, name: &str) -> Option<(u32, u32)> {
        let cid = self.class_ctx?;
        self.classes[cid as usize].field_slot.get(name).copied()
    }

    /// The class id of a method-call / field receiver expression.
    fn receiver_class(&self, obj: &Expr) -> u32 {
        match obj {
            Expr::Ref(name) if name == "this" => {
                self.class_ctx.expect("`this` used outside a method")
            }
            Expr::Ref(name) => {
                // A local class handle, or an unqualified field of `this`.
                if let Some(cid) = self.local_classes.get(name).copied() {
                    cid
                } else if let Some(cid) = self.field_class(name) {
                    cid
                } else if let Some(cid) = self.static_field_class(name) {
                    cid
                } else if let Some(&cid) = self.globals.class.get(name) {
                    cid
                } else {
                    panic!("'{name}' is not a class handle")
                }
            }
            // Chained access `recv.field.method(...)`: resolve the receiver's
            // class, then look up the field's class (instance or static).
            Expr::Field { obj, field } => {
                let oc = self.receiver_class(obj);
                self.classes[oc as usize]
                    .field_class
                    .get(field)
                    .copied()
                    .or_else(|| {
                        self.classes[oc as usize]
                            .static_field_class
                            .get(field)
                            .copied()
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "field '{field}' is not a class handle on '{}'",
                            self.classes[oc as usize].name
                        )
                    })
            }
            // Chained call `recv.method(...).next()`: the receiver's class is
            // the (class) return type of `method`.
            Expr::MethodCall { obj, method, .. } => {
                let oc = self.receiver_class(obj);
                self.classes[oc as usize]
                    .method_ret_class
                    .get(method)
                    .copied()
                    .unwrap_or_else(|| {
                        panic!(
                            "method '{method}' does not return a class handle on '{}'",
                            self.classes[oc as usize].name
                        )
                    })
            }
            // `Class::method(...).next()` — static method's class return type.
            Expr::StaticCall {
                class_name, method, ..
            } => {
                let cid = self
                    .resolve_class(class_name)
                    .unwrap_or_else(|| panic!("static call on unknown class '{class_name}'"));
                self.classes[cid as usize]
                    .method_ret_class
                    .get(method)
                    .copied()
                    .unwrap_or_else(|| {
                        panic!("static method '{method}' does not return a class handle")
                    })
            }
            // Unqualified `method(...).next()` — method of the current class.
            Expr::Call { name, .. } if self.method_of_ctx(name) => {
                let cid = self.class_ctx.expect("method call outside a method");
                self.classes[cid as usize]
                    .method_ret_class
                    .get(name)
                    .copied()
                    .unwrap_or_else(|| panic!("method '{name}' does not return a class handle"))
            }
            // `coll[i].method(...)` — receiver is the collection's element class.
            Expr::Index { base, .. } => self
                .coll_elem_class(base)
                .unwrap_or_else(|| panic!("indexed receiver is not a class-handle collection")),
            // Complex receiver (cast, call-chain result) that the front-end
            // lowered to a placeholder — not yet supported.
            _ => panic!("unsupported method/field receiver expression"),
        }
    }

    /// The `(kind, element class)` of the collection that `base` refers to, if
    /// `base` is a queue/array/assoc local or field.
    fn coll_of_receiver(&self, base: &Expr) -> Option<CollInfo> {
        match base {
            Expr::Ref(name) if name == "this" => None,
            Expr::Ref(name) => self
                .local_colls
                .get(name)
                .copied()
                .or_else(|| {
                    let cid = self.class_ctx?;
                    self.classes[cid as usize].field_coll.get(name).copied()
                })
                .or_else(|| {
                    let cid = self.class_ctx?;
                    self.classes[cid as usize]
                        .static_field_coll
                        .get(name)
                        .copied()
                })
                .or_else(|| self.globals.coll.get(name).copied()),
            Expr::Field { obj, field } => {
                let oc = self.try_class_of(obj)?;
                self.classes[oc as usize]
                    .field_coll
                    .get(field)
                    .copied()
                    .or_else(|| {
                        self.classes[oc as usize]
                            .static_field_coll
                            .get(field)
                            .copied()
                    })
            }
            // Package-qualified global collection: `pkg::varname` with 0 args.
            Expr::StaticCall {
                class_name,
                method: var_name,
                args,
                ..
            } if args.is_empty() && self.resolve_class(class_name).is_none() => {
                self.globals.coll.get(var_name.as_str()).copied()
            }
            _ => None,
        }
    }

    /// The element class id of a collection `base` (if its elements are handles).
    fn coll_elem_class(&self, base: &Expr) -> Option<u32> {
        self.coll_of_receiver(base).and_then(|info| info.elem_class)
    }

    /// Non-panicking class resolution of a receiver (for collection lookups).
    fn try_class_of(&self, obj: &Expr) -> Option<u32> {
        match obj {
            Expr::Ref(name) if name == "this" => self.class_ctx,
            Expr::Ref(name) => self
                .local_classes
                .get(name)
                .copied()
                .or_else(|| self.field_class(name))
                .or_else(|| self.static_field_class(name))
                .or_else(|| self.globals.class.get(name).copied()),
            Expr::Field { obj, field } => {
                let oc = self.try_class_of(obj)?;
                self.classes[oc as usize].field_class.get(field).copied()
            }
            _ => None,
        }
    }

    /// A class-typed field of the current method's class.
    fn field_class(&self, name: &str) -> Option<u32> {
        let cid = self.class_ctx?;
        self.classes[cid as usize].field_class.get(name).copied()
    }

    /// The global static-field id of `name` in the current class (if any).
    fn static_field(&self, name: &str) -> Option<u32> {
        let cid = self.class_ctx?;
        self.classes[cid as usize].static_fields.get(name).copied()
    }

    /// The class id of a class-typed static field `name` of the current class.
    fn static_field_class(&self, name: &str) -> Option<u32> {
        let cid = self.class_ctx?;
        self.classes[cid as usize]
            .static_field_class
            .get(name)
            .copied()
    }

    /// Resolve a class name in a static reference: a global class, a
    /// class-scoped typedef alias (e.g. `type_id`) of the current class, or a
    /// type parameter's class default.
    /// IEEE-1800 builtin static methods that have no UVM/SV source. Currently
    /// `process::self()` (returns null until fork/join provides real process
    /// handles; every UVM call site guards the result with `if (p != null)`).
    /// Returns `None` for anything not handled here.
    fn gen_builtin_static(
        &mut self,
        class_name: &str,
        method: &str,
        pb: &mut ProgramBuilder,
    ) -> Option<Reg> {
        match (class_name, method) {
            ("process", "self") => {
                let dst = pb.new_reg();
                let k = pb.konst(Value::Null);
                pb.emit(Inst::LoadConst { dst, k });
                Some(dst)
            }
            _ => None,
        }
    }

    /// IEEE-1800 builtin methods on enum / string values (`.name()`, `.itoa()`).
    /// Returns `None` if `obj.method(...)` is not a recognized builtin (the
    /// caller then falls through to class-method dispatch).
    fn gen_builtin_method(
        &mut self,
        obj: &Expr,
        method: &str,
        args: &[Expr],
        pb: &mut ProgramBuilder,
    ) -> Option<Reg> {
        // `enum_var.name()` -> the enum member name string.
        if method == "name" && args.is_empty() {
            if let Expr::Ref(name) = obj {
                if let Some(&table) = self.local_enums.get(name) {
                    let src = self.gen_expr(obj, pb);
                    let dst = pb.new_reg();
                    pb.emit(Inst::EnumName { dst, src, table });
                    return Some(dst);
                }
            }
        }
        // `str.itoa(value)` -> formats `value` as decimal into `str` (mutates
        // the receiver l-value). Only on a string-typed local receiver.
        if method == "itoa" && args.len() == 1 {
            if let Expr::Ref(name) = obj {
                if self.is_string_local(name) {
                    let val = self.gen_expr(&args[0], pb);
                    let fmt = pb.konst(Value::Str(Rc::from("%0d")));
                    let arglist = pb.arglist(&[val]);
                    let tmp = pb.new_reg();
                    pb.emit(Inst::Format {
                        dst: tmp,
                        fmt,
                        args: arglist,
                    });
                    self.gen_assign(name, tmp, false, pb);
                    return Some(tmp);
                }
            }
        }
        // String built-in methods: .len(), .substr(), .toupper(), .tolower(), .atoi()
        if let Some(src_reg) = self.try_string_receiver(obj, pb) {
            match (method, args.len()) {
                ("len", 0) => {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StringLen { dst, src: src_reg });
                    return Some(dst);
                }
                ("substr", 2) => {
                    let lo = self.gen_expr(&args[0], pb);
                    let hi = self.gen_expr(&args[1], pb);
                    let dst = pb.new_reg();
                    pb.emit(Inst::StringSub {
                        dst,
                        src: src_reg,
                        lo,
                        hi,
                    });
                    return Some(dst);
                }
                ("toupper", 0) => {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StringToUpper { dst, src: src_reg });
                    return Some(dst);
                }
                ("tolower", 0) => {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StringToLower { dst, src: src_reg });
                    return Some(dst);
                }
                ("atoi" | "atoreal", 0) => {
                    let dst = pb.new_reg();
                    pb.emit(Inst::StringAtoi { dst, src: src_reg });
                    return Some(dst);
                }
                _ => {}
            }
        }
        None
    }

    /// If `obj` is a string-typed expression, emit code to load it and return
    /// the register holding its value. Returns `None` if `obj` is not a string.
    fn try_string_receiver(&mut self, obj: &Expr, pb: &mut ProgramBuilder) -> Option<Reg> {
        if let Expr::Ref(name) = obj {
            // Local string variable.
            if self.is_string_local(name) {
                return Some(self.gen_expr(obj, pb));
            }
            // Class field of string type (not a class-handle field and not a coll).
            if let Some((slot, _)) = self.class_field(name) {
                let cid = self.class_ctx?;
                let ci = &self.classes[cid as usize];
                if !ci.field_class.contains_key(name) && !ci.field_coll.contains_key(name) {
                    let this = self.this_reg;
                    let dst = pb.new_reg();
                    pb.emit(Inst::GetField {
                        dst,
                        obj: this,
                        slot,
                    });
                    return Some(dst);
                }
            }
            // String-typed parameter (non-class, non-enum local).
            if self.locals.contains_key(name) {
                return Some(self.gen_expr(obj, pb));
            }
        }
        None
    }

    /// Whether `name` is a string-typed local (best-effort: a local that is not
    /// a class handle, enum, or collection).
    fn is_string_local(&self, name: &str) -> bool {
        self.locals.contains_key(name)
            && !self.local_classes.contains_key(name)
            && !self.local_enums.contains_key(name)
            && !self.local_colls.contains_key(name)
    }

    fn resolve_class(&self, name: &str) -> Option<u32> {
        if let Some(&cid) = self.class_ids.get(name) {
            return Some(cid);
        }
        if let Some((owner, aliases)) = name.split_once("::") {
            let mut cid = self.resolve_class(owner)?;
            for alias in aliases.split("::") {
                cid = self.classes[cid as usize]
                    .type_aliases
                    .get(alias)
                    .copied()?;
            }
            return Some(cid);
        }
        // Package/module-scope typedef (`typedef uvm_pool#(string,int) foo;`).
        if let Some(&cid) = self.globals.type_aliases.get(name) {
            return Some(cid);
        }
        // Cross-class typedef (e.g. `rsrc_q_t` inside `uvm_resource_types`).
        if let Some(&cid) = self.globals.class_typedefs.get(name) {
            return Some(cid);
        }
        let cur = self.class_ctx?;
        let ci = &self.classes[cur as usize];
        ci.type_aliases
            .get(name)
            .copied()
            .or_else(|| ci.type_param_default.get(name).copied())
    }

    fn resolve_collection_alias(&self, scope: Option<&str>, name: &str) -> Option<CollInfo> {
        if let Some(owner) = scope {
            if let Some(cid) = self.resolve_class(owner) {
                if let Some(&info) = self.classes[cid as usize].collection_aliases.get(name) {
                    return Some(info);
                }
            }
        }
        if let Some(cid) = self.class_ctx {
            if let Some(&info) = self.classes[cid as usize].collection_aliases.get(name) {
                return Some(info);
            }
        }
        self.globals.class_collection_typedefs.get(name).copied()
    }

    /// True if `name` is a method of the class currently being compiled.
    fn method_of_ctx(&self, name: &str) -> bool {
        self.class_ctx
            .is_some_and(|cid| self.classes[cid as usize].methods.contains_key(name))
    }

    /// Emit an unqualified method call `this.method(args)` (virtual if the
    /// method is virtual in the current class, else a direct call).
    fn gen_method_on_this(&mut self, method: &str, args: &[Expr], pb: &mut ProgramBuilder) -> Reg {
        let cid = self.class_ctx.expect("method call outside a method");
        let this = self.this_reg;
        let mut arg_regs = vec![this];
        for a in args {
            arg_regs.push(self.gen_expr(a, pb));
        }
        let arglist = pb.arglist(&arg_regs);
        let ret = pb.new_reg();
        if let Some(&vslot) = self.classes[cid as usize].vslot_of.get(method) {
            pb.emit(Inst::CallVirtual {
                obj: this,
                vslot,
                args: arglist,
                ret,
            });
        } else {
            let fid = self.classes[cid as usize].methods[method];
            pb.emit(Inst::Call {
                func: fid,
                args: arglist,
                ret,
            });
        }
        ret
    }

    /// Assign to an l-value: an element write `name[i] = src` for a collection,
    /// or a plain assignment to `name`.
    fn gen_assign_lvalue(
        &mut self,
        lhs: &Lvalue,
        src: Reg,
        nonblocking: bool,
        pb: &mut ProgramBuilder,
    ) {
        if let Some(index) = &lhs.index {
            // `base[index] = src` — element write through the shared collection
            // handle (mutates the underlying storage).
            let base_expr = match &lhs.scope {
                Some(scope) => Expr::StaticRef {
                    class_name: scope.clone(),
                    field: lhs.name.clone(),
                },
                None => match &lhs.receiver {
                    Some(receiver) => Expr::Field {
                        obj: receiver.clone(),
                        field: lhs.name.clone(),
                    },
                    None => Expr::Ref(lhs.name.clone()),
                },
            };
            let base = self.gen_expr(&base_expr, pb);
            let idx = self.gen_expr(index, pb);
            pb.emit(Inst::IndexSet { base, idx, src });
        } else if let Some(scope) = &lhs.scope {
            // `Class::field = src` — a scoped static-field write.
            let cid = self
                .resolve_class(scope)
                .unwrap_or_else(|| panic!("scoped assign on unknown class '{scope}'"));
            let id = *self.classes[cid as usize]
                .static_fields
                .get(&lhs.name)
                .unwrap_or_else(|| panic!("no static field '{}' on class '{scope}'", lhs.name));
            pb.emit(Inst::StaticSet { id, src });
        } else if let Some(receiver) = &lhs.receiver {
            let obj = self.gen_expr(receiver, pb);
            let cid = self.receiver_class(receiver);
            let slot = self.classes[cid as usize]
                .field_slot
                .get(&lhs.name)
                .map(|(slot, _)| *slot)
                .unwrap_or_else(|| {
                    panic!(
                        "no field '{}' on class '{}'",
                        lhs.name, self.classes[cid as usize].name
                    )
                });
            pb.emit(Inst::SetField { obj, slot, src });
        } else {
            self.gen_assign(&lhs.name, src, nonblocking, pb);
        }
    }

    /// Assign `src` to `name` (a local move, a class-field write, or a net write).
    fn gen_assign(&mut self, name: &str, src: Reg, nonblocking: bool, pb: &mut ProgramBuilder) {
        if let Some((reg, _)) = self.locals.get(name).copied() {
            if nonblocking {
                pb.emit(Inst::NbaAssign { dst: reg, src });
            } else {
                pb.emit(Inst::Assign { dst: reg, src });
            }
        } else if let Some((slot, _)) = self.class_field(name) {
            let this = self.this_reg;
            pb.emit(Inst::SetField {
                obj: this,
                slot,
                src,
            });
        } else if let Some(id) = self.static_field(name) {
            pb.emit(Inst::StaticSet { id, src });
        } else if let Some(&id) = self.globals.vars.get(name) {
            pb.emit(Inst::StaticSet { id, src });
        } else {
            let net = self.net(name);
            if nonblocking {
                pb.emit(Inst::NbaWrite { net, src });
            } else {
                pb.emit(Inst::BlockingWrite { net, src });
            }
        }
    }

    fn net(&self, name: &str) -> NetId {
        self.nets
            .get(name)
            .unwrap_or_else(|| panic!("elaboration: unknown signal '{name}'"))
            .0
    }

    fn is_named_event_expr(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Ref(name) => {
                self.local_events.contains(name)
                    || self.class_ctx.is_some_and(|class| {
                        self.classes[class as usize].field_events.contains(name)
                    })
            }
            Expr::Field { obj, field } => {
                let class = self.receiver_class(obj);
                self.classes[class as usize].field_events.contains(field)
            }
            _ => false,
        }
    }
}

/// Build a default `$display` format (`%0d` per value arg) when no format
/// string is supplied.
fn default_fmt(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        if i > 0 {
            s.push(' ');
        }
        s.push_str("%0d");
    }
    s
}

/// Map a built-in collection method name to its [`CollOp`], if it is one.
fn coll_op(method: &str) -> Option<CollOp> {
    Some(match method {
        "push_back" => CollOp::PushBack,
        "push_front" => CollOp::PushFront,
        "pop_back" => CollOp::PopBack,
        "pop_front" => CollOp::PopFront,
        "size" => CollOp::Size,
        "insert" => CollOp::Insert,
        "delete" => CollOp::Delete,
        "exists" => CollOp::Exists,
        "num" => CollOp::Num,
        "first" => CollOp::First,
        "last" => CollOp::Last,
        "next" => CollOp::Next,
        "prev" => CollOp::Prev,
        _ => return None,
    })
}

/// Constant-fold an expression (for init values and `#delay` amounts).
fn const_eval(e: &Expr) -> LogicVec {
    const_eval_with(e, None)
}

fn const_eval_with(e: &Expr, consts: Option<&HashMap<String, LogicVec>>) -> LogicVec {
    match e {
        Expr::Literal(lv) => lv.clone(),
        Expr::Str(_) => LogicVec::zero(32),
        Expr::Ref(name) => consts
            .and_then(|values| values.get(name))
            .cloned()
            .unwrap_or_else(|| LogicVec::zero(32)),
        Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Field { .. } | Expr::New { .. } => {
            LogicVec::zero(32)
        }
        Expr::StaticCall { .. } => LogicVec::zero(32),
        Expr::StaticRef { .. } => LogicVec::zero(32),
        Expr::Index { .. } => LogicVec::zero(32),
        Expr::PartSelect { base, left, right } => {
            let value = const_eval_with(base, consts);
            let left = const_eval_with(left, consts).to_u64() as u32;
            let right = const_eval_with(right, consts).to_u64() as u32;
            if left >= right {
                value.slice(left, right)
            } else {
                let width = right - left + 1;
                let mut selected = LogicVec::zero(width);
                for offset in 0..width {
                    selected.set_bit(offset, value.get_bit(right - offset));
                }
                selected
            }
        }
        Expr::Null => LogicVec::zero(32),
        Expr::Concat(parts) => {
            let mut parts = parts.iter().map(|part| const_eval_with(part, consts));
            let Some(first) = parts.next() else {
                return LogicVec::zero(1);
            };
            parts.fold(first, |value, part| value.concat(&part))
        }
        Expr::SysCall { .. } => LogicVec::zero(32),
        Expr::Unary { op, operand } => {
            let v = const_eval_with(operand, consts);
            match op {
                UnaryOp::BitNot => v.bitnot(),
                UnaryOp::Neg => LogicVec::zero(v.width()).sub(&v),
                _ => v,
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let l = const_eval_with(lhs, consts);
            let r = const_eval_with(rhs, consts);
            match op {
                BinOp::Add => l.add(&r),
                BinOp::Sub => l.sub(&r),
                BinOp::Mul => l.mul(&r),
                BinOp::And => l.bitand(&r),
                BinOp::Or => l.bitor(&r),
                BinOp::Xor => l.bitxor(&r),
                _ => l,
            }
        }
    }
}
