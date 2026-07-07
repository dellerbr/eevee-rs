//! Lowering the Verible CST to the eevee AST.
//!
//! This is the Rust port of the most valuable part of the Python reference
//! (`lang/verible_fe.py`): the map from Verible's concrete syntax tree to a
//! clean AST. It currently handles the synthesizable-RTL subset; each construct
//! is a small, independently-testable function so the map can grow the way the
//! Python one did.
//!
//! Verible quirks already encoded here (see `docs/frontend-decision.md`):
//! * a bare variable read parses as `kFunctionCall(kReference(kLocalRoot(
//!   kUnqualifiedId(SymbolIdentifier))))` — a zero-arg "call" — so references
//!   fall back to the identifier;
//! * operator leaves carry the symbol in `tag` with empty `text` (`leaf_op`);
//! * `children` arrays contain `null` holes (`kids` skips them).

use eevee_ast::*;
use eevee_core::LogicVec;

use crate::cst::{const_int, find, find_deep, kids, leaf_op, tag, text};
use serde_json::Value;

/// Lower a whole file (root `kDescriptionList`) to a [`SourceFile`].
pub fn lower_file(tree: &Value) -> SourceFile {
    let mut items = Vec::new();
    collect_descriptions(tree, &mut items);
    SourceFile { items }
}

fn collect_descriptions(n: &Value, out: &mut Vec<Item>) {
    match tag(n) {
        "kModuleDeclaration" => out.push(Item::Module(lower_module(n))),
        "kPackageDeclaration" => out.push(Item::Package(lower_package(n))),
        // Compilation-unit-scope class / function (real testbenches use these).
        "kClassDeclaration" => out.push(Item::Class(lower_class(n))),
        "kFunctionDeclaration" | "kTaskDeclaration" => out.push(Item::Func(lower_function(n))),
        // Descend through wrappers (kDescriptionList, etc.).
        _ => {
            for c in kids(n) {
                collect_descriptions(c, out);
            }
        }
    }
}

/// Lower one module/package item, pushing any results onto `out`.
fn lower_module_item(item: &Value, out: &mut Vec<ModuleItem>) {
    match tag(item) {
        "kDataDeclaration" => {
            lower_enum_consts(item, out);
            for v in lower_data_decl(item) {
                out.push(ModuleItem::Var(v));
            }
        }
        "kAlwaysStatement" => out.push(ModuleItem::Always(lower_always(item))),
        "kInitialStatement" => out.push(ModuleItem::Initial(lower_initial(item))),
        "kFunctionDeclaration" | "kTaskDeclaration" => {
            out.push(ModuleItem::Func(lower_function(item)))
        }
        "kClassDeclaration" => out.push(ModuleItem::Class(lower_class(item))),
        // `localparam`/`parameter NAME = value;` -> a named constant.
        "kParamDeclaration" => {
            if let Some((name, value)) = param_const_pair(item) {
                out.push(ModuleItem::EnumConst { name, value });
            }
        }
        // `typedef enum {...}` contributes named compile-time constants;
        // `typedef <Class>#(...) <alias>;` contributes a type alias.
        "kTypeDeclaration" => {
            lower_enum_consts(item, out);
            if let Some(alias) = lower_class_typedef(item) {
                out.push(ModuleItem::TypeAlias(alias));
            }
        }
        // List wrappers — descend.
        "kModuleItemList" | "kPackageItemList" | "kClassItems" => {
            for it in kids(item) {
                lower_module_item(it, out);
            }
        }
        _ => {} // unsupported item — skipped for this subset
    }
}

/// Extract enum members as named constants. Members get sequential integer
/// values starting at 0, unless an explicit `= value` overrides (subsequent
/// members continue from there). Enum base width defaults to 32 (`int`).
fn lower_enum_consts(n: &Value, out: &mut Vec<ModuleItem>) {
    let members = enum_const_pairs(n);
    for (name, value) in &members {
        out.push(ModuleItem::EnumConst {
            name: name.clone(),
            value: value.clone(),
        });
    }
    // A named enum type (`typedef enum {...} NAME;`) records its members for
    // `.name()` resolution. NAME is a direct-child identifier of the decl.
    if !members.is_empty() {
        if let Some(name) = find(n, "SymbolIdentifier").map(text) {
            if !name.is_empty() {
                out.push(ModuleItem::EnumType {
                    name: name.to_string(),
                    members,
                });
            }
        }
    }
}

/// The `(name, value)` constant pairs of a `typedef enum {...}` declaration.
fn enum_const_pairs(n: &Value) -> Vec<(String, LogicVec)> {
    let mut out = Vec::new();
    let Some(enum_ty) = find_deep(n, "kEnumType") else {
        return out;
    };
    let Some(list) = find_deep(enum_ty, "kEnumNameList") else {
        return out;
    };
    let mut next: i64 = 0;
    for en in kids(list) {
        if tag(en) != "kEnumName" {
            continue;
        }
        let name = match find(en, "SymbolIdentifier") {
            Some(id) => text(id).to_string(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        // An explicit `= value` sits under `kEnumName > kTrailingAssign >
        // kExpression`, not as a direct `kExpression` child.
        let value = find(en, "kTrailingAssign")
            .and_then(|ta| find(ta, "kExpression"))
            .and_then(const_int)
            .unwrap_or(next);
        out.push((name, LogicVec::from_i64(value, 32)));
        next = value + 1;
    }
    out
}

/// A class/package `localparam`/`parameter NAME = value;` -> `(name, value)`.
/// The value is taken as the first integer literal in the declaration.
fn param_const_pair(n: &Value) -> Option<(String, LogicVec)> {
    let pt = find(n, "kParamType")?;
    let name = find(pt, "SymbolIdentifier").map(text)?.to_string();
    if name.is_empty() {
        return None;
    }
    let value = const_int(n).unwrap_or(0);
    Some((name, LogicVec::from_i64(value, 32)))
}

fn lower_module(n: &Value) -> Module {
    let name = find(n, "kModuleHeader")
        .and_then(|h| find(h, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();

    let mut items = Vec::new();
    if let Some(list) = find(n, "kModuleItemList") {
        for item in kids(list) {
            lower_module_item(item, &mut items);
        }
    }

    Module {
        name,
        ports: Vec::new(),
        items,
    }
}

fn lower_package(n: &Value) -> Package {
    let name = find(n, "kPackageHeader")
        .and_then(|h| find(h, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    let mut items = Vec::new();
    for item in kids(n) {
        // Skip the header/keywords; lower everything else (items are direct
        // children of the package declaration, possibly under a list wrapper).
        match tag(item) {
            "kPackageHeader" | "package" | "endpackage" | ";" => {}
            _ => lower_module_item(item, &mut items),
        }
    }
    Package { name, items }
}

/// `logic [W] a = i;`, a class handle `Counter c;`, or a class field
/// `int count;` — one [`VarDecl`] per declared variable.
fn lower_data_decl(n: &Value) -> Vec<VarDecl> {
    let base = find(n, "kInstantiationBase");
    let dtype = base
        .and_then(|b| find(b, "kInstantiationType"))
        .and_then(|t| find(t, "kDataType"));
    let class_name = dtype.and_then(class_type_name);
    let type_args = dtype.map(type_args_of).unwrap_or_default();
    let is_string = dtype.map(is_string_type).unwrap_or(false);
    let width = dtype.map(packed_width).unwrap_or(1);
    // A `static` qualifier sits in a kQualifierList directly under the decl.
    let is_static = find(n, "kQualifierList")
        .map(|q| kids(q).any(|c| tag(c) == "static"))
        .unwrap_or(false);

    let mut out = Vec::new();
    // Module/handle vars use kGateInstanceRegisterVariableList>kRegisterVariable;
    // class fields (and some locals) use
    // kVariableDeclarationAssignmentList>kVariableDeclarationAssignment.
    let list = base.and_then(|b| {
        find(b, "kGateInstanceRegisterVariableList")
            .or_else(|| find(b, "kVariableDeclarationAssignmentList"))
    });
    if let Some(list) = list {
        for rv in kids(list) {
            if !matches!(
                tag(rv),
                "kRegisterVariable" | "kVariableDeclarationAssignment"
            ) {
                continue;
            }
            // `kRegisterVariable` holds the name as a direct `SymbolIdentifier`
            // leaf; `kVariableDeclarationAssignment` (user/enum/class-typed, and
            // all package-scope decls) nests it under a `kUnqualifiedId`.
            let name = find(rv, "SymbolIdentifier")
                .or_else(|| find(rv, "kUnqualifiedId").and_then(|u| find(u, "SymbolIdentifier")))
                .map(text)
                .unwrap_or_default()
                .to_string();
            let coll = classify_coll(rv);
            // `Type v = new(...);` (combined decl+init): the trailing assign's
            // rhs is a bare `kClassNew`, not wrapped in `kExpression` (same
            // quirk `lower_rhs` already works around for plain `v = new(...);`
            // assignment statements).
            let init = find(rv, "kTrailingAssign").and_then(|ta| {
                find(ta, "kExpression")
                    .or_else(|| find(ta, "kClassNew"))
                    .map(lower_expr)
            });
            out.push(VarDecl {
                name,
                width,
                signed: false,
                class_name: class_name.clone(),
                type_args: type_args.clone(),
                is_string,
                coll,
                is_static,
                init,
            });
        }
    }
    out
}

/// The actual `#(...)` type arguments of a `kDataType` (the outermost
/// parameter list), each lowered to a [`TypeRef`] (recursively for nested
/// `#(...)`). Empty for a non-parameterized type.
fn type_args_of(dtype: &Value) -> Vec<TypeRef> {
    match find_deep(dtype, "kActualParameterList") {
        Some(apl) => lower_actual_args(apl),
        None => Vec::new(),
    }
}

/// Lower a `kActualParameterList`'s positional arguments to [`TypeRef`]s. Type
/// args appear as `kDataType`; value args (numbers) and type-parameter
/// references appear as `kExpression`/identifiers.
fn lower_actual_args(apl: &Value) -> Vec<TypeRef> {
    let Some(plist) =
        find(apl, "kParenGroup").and_then(|p| find(p, "kActualParameterPositionalList"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for arg in kids(plist) {
        match tag(arg) {
            "kDataType" => out.push(type_ref_of_datatype(arg)),
            "kExpression" | "kParamByName" => {
                if let Some(s) = find_deep(arg, "TK_StringLiteral") {
                    out.push(TypeRef::simple(unquote(text(s))));
                } else if let Some(id) = find_deep(arg, "SymbolIdentifier") {
                    out.push(TypeRef::simple(text(id).to_string()));
                } else if let Some(num) = find_deep(arg, "TK_DecNumber") {
                    out.push(TypeRef::simple(text(num).to_string()));
                }
            }
            _ => {}
        }
    }
    out
}

/// A [`TypeRef`] for a `kDataType` argument: the primitive keyword or class
/// identifier, plus any nested `#(...)` arguments.
fn type_ref_of_datatype(dt: &Value) -> TypeRef {
    let name = if let Some(prim) = find_deep(dt, "kDataTypePrimitive") {
        kids(prim).next().map(tag).unwrap_or_default().to_string()
    } else {
        find_deep(dt, "SymbolIdentifier")
            .map(|id| text(id).to_string())
            .unwrap_or_default()
    };
    TypeRef {
        name,
        args: type_args_of(dt),
    }
}

/// Classify the unpacked dimension of a declared variable: a queue/dynamic/
/// fixed array (`Queue`), an associative array (`Assoc`), or a scalar (`None`).
/// Mirrors the Python front-end's `_classify_unpacked_dim`.
fn classify_coll(rv: &Value) -> Option<CollKind> {
    let unpacked = find(rv, "kUnpackedDimensions")?;
    let dims = find(unpacked, "kDeclarationDimensions")?;
    for dim in kids(dims) {
        match tag(dim) {
            "kDimensionAssociativeType" => return Some(CollKind::Assoc),
            "kDimensionRange" => {
                // `[$:N]` bounded queue vs fixed `[hi:lo]` range (both list-backed).
                return Some(CollKind::Queue);
            }
            "kDimensionScalar" => {
                if let Some(el) = find(dim, "kExpressionList") {
                    // `[$]` queue.
                    if find_deep(el, "$").is_some() {
                        return Some(CollKind::Queue);
                    }
                    // `[user_type]` (a bare identifier, no number) is an assoc key.
                    if find_deep(el, "TK_DecNumber").is_none()
                        && find_deep(el, "SymbolIdentifier").is_some()
                    {
                        return Some(CollKind::Assoc);
                    }
                }
                // `[]` dynamic or `[N]` fixed array.
                return Some(CollKind::Queue);
            }
            _ => {}
        }
    }
    None
}

/// If a `kDataType` names a class / user type (a bare identifier rather than a
/// primitive keyword), return that name. For a scoped type `pkg::Class` the
/// last segment is taken; `#(...)` parameters are stripped (type erasure).
fn class_type_name(dtype: &Value) -> Option<String> {
    if find(dtype, "kDataTypePrimitive").is_some() {
        return None;
    }
    // Scoped type `pkg::Class[::...]` -> the last `::` segment.
    if let Some(qid) = find_deep(dtype, "kQualifiedId") {
        if let Some(last) = kids(qid)
            .filter(|c| tag(c) == "kUnqualifiedId")
            .filter_map(|u| find(u, "SymbolIdentifier"))
            .map(|id| text(id).to_string())
            .last()
        {
            return Some(last);
        }
    }
    find_deep(dtype, "SymbolIdentifier").map(|id| text(id).to_string())
}

/// True if the data type is the SV `string` primitive.
fn is_string_type(dtype: &Value) -> bool {
    find_deep(dtype, "kDataTypePrimitive")
        .map(|p| kids(p).any(|c| tag(c) == "string"))
        .unwrap_or(false)
}

/// Width from a `kDataType`'s packed dimension `[hi:lo]`, or the primitive
/// type's natural width for an unpacked scalar (`int` = 32, `byte` = 8, ...).
fn packed_width(dtype: &Value) -> u32 {
    if let Some(pd) = find(dtype, "kPackedDimensions") {
        if let Some(dr) = find_deep(pd, "kDimensionRange") {
            let nums: Vec<i64> = kids(dr)
                .filter(|c| tag(c) == "kExpression")
                .filter_map(const_int)
                .collect();
            if nums.len() == 2 {
                return (nums[0] - nums[1]).unsigned_abs() as u32 + 1;
            }
        }
    }
    // No packed range: use the primitive type's natural width.
    if let Some(prim) = find_deep(dtype, "kDataTypePrimitive") {
        if let Some(kw) = kids(prim).next().map(tag) {
            return match kw {
                "byte" => 8,
                "shortint" => 16,
                "int" | "integer" => 32,
                "longint" | "time" => 64,
                _ => 1, // logic / bit / reg scalar
            };
        }
    }
    1
}

fn lower_initial(n: &Value) -> Stmt {
    // [initial keyword, body statement]
    kids(n).nth(1).map(lower_stmt).unwrap_or(Stmt::Null)
}

fn lower_function(n: &Value) -> FuncDecl {
    let header = find(n, "kFunctionHeader").or_else(|| find(n, "kTaskHeader"));
    // A `task` never returns a value; a `function void` explicitly doesn't
    // either. Either way an implicit `ReturnVoid` should be emitted (rather
    // than `Return{value: <bogus zero-init register>}`).
    let is_task = find(n, "kTaskHeader").is_some();
    let ret_dtype = header.and_then(|h| find(h, "kDataType"));
    let ret_width = ret_dtype.map(packed_width).unwrap_or(32);
    let ret_class = ret_dtype.and_then(class_type_name);
    let is_void_return = ret_dtype
        .and_then(|d| find(d, "kDataTypePrimitive"))
        .map(|p| kids(p).any(|c| tag(c) == "void"))
        .unwrap_or(false);
    // Out-of-body `Class::method` definition: the header name is a kQualifiedId.
    let (name, class_scope) = match header.and_then(|h| find(h, "kQualifiedId")) {
        Some(qid) => {
            // Segments are `kUnqualifiedId`s, except the constructor `new`,
            // which appears as a raw `new` keyword leaf.
            let segs: Vec<String> = kids(qid)
                .filter_map(|c| match tag(c) {
                    "kUnqualifiedId" => find(c, "SymbolIdentifier").map(|id| text(id).to_string()),
                    "new" => Some("new".to_string()),
                    _ => None,
                })
                .collect();
            let method = segs.last().cloned().unwrap_or_default();
            let scope = if segs.len() >= 2 {
                Some(segs[segs.len() - 2].clone())
            } else {
                None
            };
            (method, scope)
        }
        None => {
            let name = header
                .and_then(|h| find(h, "kUnqualifiedId"))
                .and_then(|u| find(u, "SymbolIdentifier"))
                .map(text)
                .unwrap_or_default()
                .to_string();
            (name, None)
        }
    };
    let params = header
        .and_then(|h| find(h, "kParenGroup"))
        .and_then(|p| find(p, "kPortList"))
        .map(lower_port_list)
        .unwrap_or_default();
    let mut stmts = Vec::new();
    // Function bodies use `kBlockItemStatementList`; task bodies (in-body and
    // out-of-body alike) use `kStatementList`.
    let body_list = find(n, "kBlockItemStatementList").or_else(|| find(n, "kStatementList"));
    if let Some(list) = body_list {
        for s in kids(list) {
            stmts.push(lower_stmt(s));
        }
    }
    let is_virtual = header
        .and_then(|h| find(h, "kQualifierList"))
        .map(|q| kids(q).any(|c| tag(c) == "virtual"))
        .unwrap_or(false);
    FuncDecl {
        name,
        ret_width,
        ret_class,
        class_scope,
        is_void: is_task || is_void_return,
        is_virtual,
        params,
        body: Stmt::Block(stmts),
    }
}

fn lower_port_list(n: &Value) -> Vec<Param> {
    let mut params = Vec::new();
    for item in kids(n) {
        if tag(item) == "kPortItem" {
            params.push(lower_port_item(item));
        }
    }
    params
}

fn lower_port_item(n: &Value) -> Param {
    let inner = find(n, "kDataTypeImplicitBasicIdDimensions").unwrap_or(n);
    let dtype = find(inner, "kDataType");
    let width = dtype.map(packed_width).unwrap_or(32);
    let class_name = dtype.and_then(class_type_name);
    let type_args = dtype.map(type_args_of).unwrap_or_default();
    let name = find(inner, "kUnqualifiedId")
        .and_then(|u| find(u, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    Param {
        name,
        dir: PortDir::Input,
        width,
        class_name,
        type_args,
    }
}

fn lower_class(n: &Value) -> ClassDecl {
    let header = find(n, "kClassHeader");
    let name = header
        .and_then(|h| find(h, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    let base = header
        .and_then(|h| find(h, "kExtendsList"))
        .and_then(|e| find_deep(e, "SymbolIdentifier"))
        .map(|id| text(id).to_string());
    let base_args = header
        .and_then(|h| find(h, "kExtendsList"))
        .and_then(|e| find_deep(e, "kActualParameterList"))
        .map(lower_actual_args)
        .unwrap_or_default();
    let params = header.map(lower_formal_params).unwrap_or_default();
    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut constructor = None;
    let mut type_aliases = Vec::new();
    let mut consts: Vec<(String, LogicVec)> = Vec::new();
    if let Some(items) = find(n, "kClassItems") {
        for item in kids(items) {
            match tag(item) {
                "kDataDeclaration" => fields.extend(lower_data_decl(item)),
                "kFunctionDeclaration" | "kTaskDeclaration" => methods.push(lower_function(item)),
                "kClassConstructor" => constructor = Some(lower_constructor(item)),
                // `pure virtual` / `extern` prototypes: capture as (virtual)
                // abstract methods so virtual dispatch + the method table see
                // them (an out-of-body definition, if any, supplies the body).
                "kForwardDeclaration" => {
                    if let Some(m) = lower_prototype(item) {
                        methods.push(m);
                    }
                }
                "kTypeDeclaration" => {
                    if let Some(alias) = lower_class_typedef(item) {
                        type_aliases.push(alias);
                    }
                    // `typedef enum {...}` members are class-scoped constants.
                    consts.extend(enum_const_pairs(item));
                }
                // `localparam`/`parameter NAME = value;` -> a named constant.
                "kParamDeclaration" => consts.extend(param_const_pair(item)),
                _ => {}
            }
        }
    }
    ClassDecl {
        name,
        base,
        fields,
        methods,
        constructor,
        type_aliases,
        params,
        base_args,
        consts,
    }
}

/// Lower a class header's `#(...)` formal parameter list. Type params record
/// their default type name; value params record their default value text.
fn lower_formal_params(header: &Value) -> Vec<ParamDecl> {
    let mut out = Vec::new();
    let Some(list) = find_deep(header, "kFormalParameterList") else {
        return out;
    };
    for pd in kids(list) {
        if tag(pd) != "kParamDeclaration" {
            continue;
        }
        if let Some(ta) = find(pd, "kTypeAssignment") {
            // `type NAME = default`
            let name = find(ta, "SymbolIdentifier")
                .map(text)
                .unwrap_or_default()
                .to_string();
            let default = find(ta, "kDataType").and_then(class_type_name);
            if !name.is_empty() {
                out.push(ParamDecl {
                    name,
                    is_type: true,
                    default,
                });
            }
        } else if let Some(pt) = find(pd, "kParamType") {
            // value param `int NAME = default`
            let name = find_deep(pt, "SymbolIdentifier")
                .map(text)
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() {
                out.push(ParamDecl {
                    name,
                    is_type: false,
                    default: None,
                });
            }
        }
    }
    out
}

/// Lower a `kForwardDeclaration` that wraps a `kFunctionPrototype` (a
/// `pure virtual` / `extern` method prototype) into an abstract method: the
/// body is empty (filled by an out-of-body definition, if any). The `virtual`
/// qualifier is carried on the forward-declaration's qualifier list.
fn lower_prototype(n: &Value) -> Option<FuncDecl> {
    let proto = find(n, "kFunctionPrototype").or_else(|| find(n, "kTaskPrototype"))?;
    let mut fd = lower_function(proto);
    let is_virtual = find(n, "kQualifierList")
        .map(|q| kids(q).any(|c| tag(c) == "virtual"))
        .unwrap_or(false);
    fd.is_virtual = fd.is_virtual || is_virtual;
    Some(fd)
}

/// A class-scoped `typedef <Class>[#(...)] <alias>;` -> a [`TypeAlias`].
/// The alias name is the `kTypeDeclaration`'s direct-child identifier; the
/// target is the class named in the `kDataType` with its `#(...)` args.
fn lower_class_typedef(n: &Value) -> Option<TypeAlias> {
    let alias = find(n, "SymbolIdentifier").map(text)?.to_string();
    let dtype = find(n, "kDataType")?;
    let name = class_type_name(dtype)?;
    Some(TypeAlias {
        alias,
        target: TypeRef {
            name,
            args: type_args_of(dtype),
        },
    })
}

fn lower_constructor(n: &Value) -> FuncDecl {
    let params = find(n, "kClassConstructorPrototype")
        .and_then(|p| find(p, "kParenGroup"))
        .and_then(|p| find(p, "kPortList"))
        .map(lower_port_list)
        .unwrap_or_default();
    let mut stmts = Vec::new();
    if let Some(list) = find(n, "kStatementList") {
        for s in kids(list) {
            stmts.push(lower_stmt(s));
        }
    }
    FuncDecl {
        name: "new".to_string(),
        ret_width: 0,
        ret_class: None,
        class_scope: None,
        is_void: true,
        is_virtual: false,
        params,
        body: Stmt::Block(stmts),
    }
}

fn lower_always(n: &Value) -> AlwaysBlock {
    let kind = match kids(n).next().map(tag) {
        Some("always_ff") => AlwaysKind::Ff,
        Some("always_comb") => AlwaysKind::Comb,
        Some("always_latch") => AlwaysKind::Latch,
        _ => AlwaysKind::Plain,
    };
    // The body is the statement following the always* keyword.
    let body = kids(n).nth(1).map(lower_stmt).unwrap_or(Stmt::Null);
    AlwaysBlock { kind, body }
}

fn lower_stmt(n: &Value) -> Stmt {
    match tag(n) {
        "kProceduralTimingControlStatement" => {
            let mut it = kids(n);
            let control = it.next().map(lower_timing_control);
            let body = it.next().map(lower_stmt).unwrap_or(Stmt::Null);
            match control {
                Some(control) => Stmt::Timed {
                    control,
                    body: Box::new(body),
                },
                None => body,
            }
        }
        "kNetVariableAssignment" => Stmt::Blocking {
            lhs: lower_lvalue(n),
            rhs: lower_rhs(n),
        },
        "kBlockingAssignmentStatement" => Stmt::Blocking {
            lhs: lower_lvalue(n),
            rhs: lower_rhs(n),
        },
        "kFunctionCall" => Stmt::Expr(lower_expr(n)),
        "kVoidcast" => {
            // void'(expr) — execute the inner call and discard its return value.
            // The expression lives inside a kParenGroup child, so search deep.
            find_deep(n, "kExpression")
                .map(|e| Stmt::Expr(lower_expr(e)))
                .unwrap_or(Stmt::Null)
        }
        "kNonblockingAssignmentStatement" => Stmt::Nonblocking {
            lhs: lower_lvalue(n),
            rhs: lower_rhs(n),
        },
        // `a++` / `--a` -> `a = a +/- 1`.
        "kIncrementDecrementExpression" => lower_incdec(n),
        // `a += e`, `a -= e`, ... -> `a = a <op> e`.
        "kAssignModifyStatement" => lower_compound_assign(n),
        "kSeqBlock" => {
            let mut stmts = Vec::new();
            let list = find(n, "kBlockItemStatementList").or_else(|| find(n, "kStatementList"));
            if let Some(list) = list {
                for s in kids(list) {
                    stmts.push(lower_stmt(s));
                }
            }
            Stmt::Block(stmts)
        }
        "kDataDeclaration" => {
            let mut decls: Vec<Stmt> = lower_data_decl(n).into_iter().map(Stmt::VarDecl).collect();
            match decls.len() {
                0 => Stmt::Null,
                1 => decls.pop().unwrap(),
                _ => Stmt::Block(decls),
            }
        }
        "kConditionalStatement" => lower_if(n),
        "kSystemTFCall" => lower_sys_call(n),
        "kJumpStatement" => {
            if kids(n).next().map(tag) == Some("return") {
                Stmt::Return(find(n, "kExpression").map(lower_expr))
            } else {
                Stmt::Null // break / continue — later
            }
        }
        "kStatement" | "kStatementItem" => {
            // wrappers — descend to the inner statement
            kids(n).next().map(lower_stmt).unwrap_or(Stmt::Null)
        }
        "kParBlock" => {
            // `fork branches join/join_any/join_none` (LRM 9.3.2). Children:
            // [fork, kBlockItemStatementList (one kStatement/kSeqBlock per
            // branch), join keyword leaf]. The join keyword is a leaf whose
            // *tag* carries the keyword (empty text), like `fork` above.
            let mut branches = Vec::new();
            let mut join = ForkJoin::All;
            for c in kids(n) {
                match tag(c) {
                    "kBlockItemStatementList" => {
                        for s in kids(c) {
                            branches.push(lower_stmt(s));
                        }
                    }
                    "join_none" => join = ForkJoin::None,
                    "join_any" => join = ForkJoin::Any,
                    "join" => join = ForkJoin::All,
                    _ => {}
                }
            }
            Stmt::Fork { branches, join }
        }
        _ => Stmt::Null,
    }
}

fn lower_lvalue(n: &Value) -> Lvalue {
    let lp = find(n, "kLPValue");
    // A scoped target `Class::field = ...` (e.g. a static field write).
    if let Some(segs) = lp.and_then(qualified_segments) {
        return Lvalue {
            scope: Some(segs[0].clone()),
            name: segs[segs.len() - 1].clone(),
            index: None,
        };
    }
    let name = lp
        .and_then(|lp| find_deep(lp, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    // `name[index] = ...` — an element assignment.
    let index = lp
        .and_then(|lp| find_deep(lp, "kDimensionScalar"))
        .and_then(|dim| find(dim, "kExpressionList"))
        .and_then(|el| kids(el).find(|c| tag(c) == "kExpression"))
        .map(lower_expr);
    Lvalue {
        name,
        index,
        scope: None,
    }
}

fn lower_rhs(n: &Value) -> Expr {
    if let Some(e) = find(n, "kExpression") {
        return lower_expr(e);
    }
    // `c = new(...)` — the rhs is a bare kClassNew, not wrapped in kExpression.
    if let Some(nw) = find(n, "kClassNew") {
        return lower_expr(nw);
    }
    Expr::Literal(LogicVec::zero(32))
}

/// Read the current value of an l-value (a plain ref or an indexed element).
fn lvalue_read(lhs: &Lvalue) -> Expr {
    match &lhs.index {
        Some(idx) => Expr::Index {
            base: Box::new(Expr::Ref(lhs.name.clone())),
            index: Box::new(idx.clone()),
        },
        None => Expr::Ref(lhs.name.clone()),
    }
}

/// `a++` / `a--` / `++a` / `--a` -> `a = a +/- 1`.
fn lower_incdec(n: &Value) -> Stmt {
    let lhs = lower_lvalue(n);
    let is_dec = kids(n).any(|c| tag(c) == "--");
    let rhs = Expr::Binary {
        op: if is_dec { BinOp::Sub } else { BinOp::Add },
        lhs: Box::new(lvalue_read(&lhs)),
        rhs: Box::new(Expr::Literal(LogicVec::from_u64(1, 32))),
    };
    Stmt::Blocking { lhs, rhs }
}

/// `a += e`, `a -= e`, ... -> `a = a <op> e`.
fn lower_compound_assign(n: &Value) -> Stmt {
    let lhs = lower_lvalue(n);
    let op = kids(n)
        .map(tag)
        .find_map(|t| match t {
            "+=" => Some(BinOp::Add),
            "-=" => Some(BinOp::Sub),
            "*=" => Some(BinOp::Mul),
            "&=" => Some(BinOp::And),
            "|=" => Some(BinOp::Or),
            "^=" => Some(BinOp::Xor),
            "<<=" => Some(BinOp::Shl),
            ">>=" => Some(BinOp::Shr),
            _ => None,
        })
        .unwrap_or(BinOp::Add);
    let operand = find(n, "kExpression")
        .map(lower_expr)
        .unwrap_or(Expr::Literal(LogicVec::zero(32)));
    let rhs = Expr::Binary {
        op,
        lhs: Box::new(lvalue_read(&lhs)),
        rhs: Box::new(operand),
    };
    Stmt::Blocking { lhs, rhs }
}

fn lower_if(n: &Value) -> Stmt {
    let if_clause = find(n, "kIfClause");
    let cond = if_clause
        .and_then(|c| find(c, "kIfHeader"))
        .and_then(|h| find(h, "kParenGroup"))
        .and_then(|p| find(p, "kExpression"))
        .map(lower_expr)
        .unwrap_or(Expr::Literal(LogicVec::from_u64(1, 1)));
    let then_branch = if_clause
        .and_then(|c| find(c, "kIfBody"))
        .and_then(|b| kids(b).next())
        .map(lower_stmt)
        .unwrap_or(Stmt::Null);
    let else_branch = find(n, "kElseClause")
        .and_then(|c| find(c, "kElseBody"))
        .and_then(|b| kids(b).next())
        .map(lower_stmt)
        .map(Box::new);
    Stmt::If {
        cond,
        then_branch: Box::new(then_branch),
        else_branch,
    }
}

fn lower_sys_call(n: &Value) -> Stmt {
    let name = find(n, "SystemTFIdentifier")
        .map(text)
        .unwrap_or_default()
        .to_string();
    let mut args = Vec::new();
    if let Some(arglist) = find_deep(n, "kArgumentList") {
        for a in kids(arglist) {
            if tag(a) == "kExpression" {
                args.push(lower_expr(a));
            }
        }
    }
    Stmt::SysCall { name, args }
}

fn lower_timing_control(n: &Value) -> TimingControl {
    match tag(n) {
        "kDelay" => {
            let amt = find(n, "kDelayValue").and_then(const_int).unwrap_or(0);
            TimingControl::Delay(Expr::Literal(LogicVec::from_u64(amt as u64, 32)))
        }
        "kEventControl" => {
            let mut events = Vec::new();
            if let Some(list) = find_deep(n, "kEventExpressionList") {
                for ev in kids(list) {
                    if tag(ev) == "kEventExpression" {
                        events.push(lower_event_expr(ev));
                    }
                }
            }
            TimingControl::Event(events)
        }
        _ => TimingControl::Delay(Expr::Literal(LogicVec::zero(32))),
    }
}

fn lower_event_expr(n: &Value) -> EventExpr {
    let edge = match kids(n).next().map(tag) {
        Some("posedge") => Edge::Posedge,
        Some("negedge") => Edge::Negedge,
        _ => Edge::AnyChange,
    };
    let expr = find(n, "kExpression")
        .map(lower_expr)
        .unwrap_or(Expr::Literal(LogicVec::zero(1)));
    EventExpr { edge, expr }
}

/// Lower an expression node (anything that may appear under `kExpression`).
pub fn lower_expr(n: &Value) -> Expr {
    match tag(n) {
        "kExpression" => kids(n)
            .next()
            .map(lower_expr)
            .unwrap_or(Expr::Literal(LogicVec::zero(32))),
        "kNumber" => Expr::Literal(lower_number(n)),
        "TK_StringLiteral" => Expr::Str(unquote(text(n))),
        // The `null` class-handle literal.
        "null" => Expr::Null,
        // A type cast `T'(expr)` -> the inner value (no strict coercion yet).
        "kCast" => find(n, "kParenGroup")
            .and_then(|p| find(p, "kExpression"))
            .map(lower_expr)
            .unwrap_or(Expr::Literal(LogicVec::zero(32))),
        // `{a, b, c}` concatenation: elements live under a `kOpenRangeList`.
        "kConcatenationExpression" => {
            let mut parts = Vec::new();
            if let Some(list) = find(n, "kOpenRangeList") {
                for c in kids(list) {
                    if tag(c) == "kExpression" {
                        parts.push(lower_expr(c));
                    }
                }
            }
            Expr::Concat(parts)
        }
        // A system function in expression position: `$sformatf(...)`,
        // `$realtime`, `$cast(...)`, ...
        "kSystemTFCall" => {
            let name = find(n, "SystemTFIdentifier")
                .map(text)
                .unwrap_or_default()
                .to_string();
            let mut args = Vec::new();
            if let Some(arglist) = find_deep(n, "kArgumentList") {
                for a in kids(arglist) {
                    if tag(a) == "kExpression" {
                        args.push(lower_expr(a));
                    }
                }
            }
            Expr::SysCall { name, args }
        }
        "kClassNew" => Expr::New {
            args: find(n, "kParenGroup")
                .map(lower_arg_list)
                .unwrap_or_default(),
        },
        "kBinaryExpression" => {
            let cs: Vec<&Value> = kids(n).collect();
            if cs.len() == 3 {
                Expr::Binary {
                    op: lower_binop(leaf_op(cs[1])),
                    lhs: Box::new(lower_expr(cs[0])),
                    rhs: Box::new(lower_expr(cs[2])),
                }
            } else {
                // n-ary chain a+b+c... fold left-associatively.
                fold_binary(&cs)
            }
        }
        "kUnaryPrefixExpression" => {
            let cs: Vec<&Value> = kids(n).collect();
            Expr::Unary {
                op: lower_unop(leaf_op(cs[0])),
                operand: Box::new(lower_expr(cs[1])),
            }
        }
        "kParenGroup" | "kParenExpression" => find(n, "kExpression")
            .map(lower_expr)
            .unwrap_or(Expr::Literal(LogicVec::zero(32))),
        // A reference / zero-arg "call" wrapping a reference -> variable read,
        // or a real function call when a paren-group of arguments is present.
        "kFunctionCall" | "kReferenceCallBase" => lower_call_or_ref(n),
        "kReference" => lower_reference_chain(n),
        "kLocalRoot" | "kUnqualifiedId" => ref_or_zero(n),
        // `a++`/`a--` in expression position: approximate to the operand value
        // (the increment side effect is applied when it is a statement).
        "kIncrementDecrementExpression" => lvalue_read(&lower_lvalue(n)),
        _ => ref_or_zero(n),
    }
}

/// Best-effort: a `SymbolIdentifier` -> `Ref`, else a number -> `Literal`,
/// else 0. Covers the Verible reference-wrapping quirks.
fn ref_or_zero(n: &Value) -> Expr {
    // Scope resolution `pkg::NAME` / `Class::NAME` -> last segment, which
    // resolves package/class enum constants (e.g. `uvm_pkg::UVM_NONE`).
    if let Some(last) = last_qualified_segment(n) {
        return Expr::Ref(last);
    }
    if let Some(id) = find_deep(n, "SymbolIdentifier") {
        return Expr::Ref(text(id).to_string());
    }
    if find_deep(n, "TK_DecNumber").is_some() {
        return Expr::Literal(lower_number(n));
    }
    Expr::Literal(LogicVec::zero(32))
}

/// The last `::` segment of a scope-resolution reference (`kQualifiedId`),
/// `#(...)` params stripped, or `None` if there is no qualified id.
fn last_qualified_segment(n: &Value) -> Option<String> {
    let qid = find_deep(n, "kQualifiedId")?;
    kids(qid)
        .filter(|c| tag(c) == "kUnqualifiedId")
        .filter_map(|u| find(u, "SymbolIdentifier"))
        .map(|id| text(id).to_string())
        .last()
}

/// Distinguish a function call, a method call (`obj.m(args)`), a field access
/// (`obj.f`), and the Verible bare-reference quirk.
fn lower_call_or_ref(n: &Value) -> Expr {
    // Call form: kReferenceCallBase carries the kReference + kParenGroup(args).
    // Bare form (the Verible zero-arg quirk): a kReference directly under a
    // kFunctionCall — a variable read or an indexed element.
    let call_base = if tag(n) == "kReferenceCallBase" {
        Some(n)
    } else {
        find(n, "kReferenceCallBase")
    };
    let (reference, paren) = match call_base {
        Some(b) => (find(b, "kReference"), find(b, "kParenGroup")),
        None => (find(n, "kReference"), None),
    };

    if let Some(reference) = reference {
        // Scope resolution `Class::member` (kQualifiedId) — static call /
        // scoped constant. Segment identifiers strip any `#(...)` params.
        if let Some(segs) = qualified_segments(reference) {
            if segs.len() == 2 {
                return match paren {
                    Some(p) => Expr::StaticCall {
                        class_name: segs[0].clone(),
                        class_args: first_segment_args(reference),
                        method: segs[1].clone(),
                        args: lower_arg_list(p),
                    },
                    // Static field read `Class::field` — keep the class name so
                    // codegen can find the correct static slot.
                    // Falls back to Expr::Ref for package-scope enum constants
                    // like `uvm_pkg::UVM_LOW` (the class lookup will fail, and
                    // the const table will catch it instead).
                    None => Expr::StaticRef {
                        class_name: segs[0].clone(),
                        field: segs[1].clone(),
                    },
                };
            }
            // 3+ segments (e.g. `T::type_id::create`) need the factory; fall
            // through to the chain path (which stubs for now).
        }

        // Build the left-leaning Ref / Index / Field chain.
        let chain = lower_reference_chain(reference);
        return match paren {
            Some(p) => match chain {
                // `recv.method(args)`.
                Expr::Field { obj, field } => Expr::MethodCall {
                    obj,
                    method: field,
                    args: lower_arg_list(p),
                },
                // `func(args)`.
                Expr::Ref(name) => Expr::Call {
                    name,
                    args: lower_arg_list(p),
                },
                // Indexed/other call target — best effort: the chain value.
                other => other,
            },
            None => chain,
        };
    }

    ref_or_zero(n)
}

/// The `#(...)` arguments of the first segment of a scope-resolution reference
/// (the class in `Class#(args)::method`).
fn first_segment_args(reference: &Value) -> Vec<TypeRef> {
    find_deep(reference, "kQualifiedId")
        .and_then(|qid| kids(qid).find(|c| tag(c) == "kUnqualifiedId"))
        .and_then(|u| find(u, "kActualParameterList"))
        .map(lower_actual_args)
        .unwrap_or_default()
}

/// Collect the segment names of a `Class::member[::...]` scope-resolution
/// reference (`kQualifiedId`), stripping any `#(...)` type parameters. Returns
/// `None` if the reference is not a qualified id.
fn qualified_segments(reference: &Value) -> Option<Vec<String>> {
    let qid = find_deep(reference, "kQualifiedId")?;
    let mut segs = Vec::new();
    for c in kids(qid) {
        if tag(c) == "kUnqualifiedId" {
            // `find` (direct child) skips the `kActualParameterList`, so only
            // the class/member identifier is taken — params are erased.
            if let Some(id) = find(c, "SymbolIdentifier") {
                segs.push(text(id).to_string());
            }
        }
    }
    if segs.len() >= 2 {
        Some(segs)
    } else {
        None
    }
}

/// Lower a `kReference` into a left-leaning Ref / Index / Field chain
/// (mirrors the Python front-end's `_lower_reference`).
fn lower_reference_chain(reference: &Value) -> Expr {
    let mut result: Option<Expr> = None;
    for c in kids(reference) {
        match tag(c) {
            "kLocalRoot" => result = Some(lower_local_root(c)),
            "kDimensionScalar" | "kDimensionRange" | "kDimensionSlice" => {
                if let Some(b) = result.take() {
                    result = Some(lower_dimension_subscript(b, c));
                }
            }
            "kHierarchyExtension" => {
                if let Some(b) = result.take() {
                    let member = find_deep(c, "SymbolIdentifier")
                        .map(text)
                        .unwrap_or_default()
                        .to_string();
                    result = Some(Expr::Field {
                        obj: Box::new(b),
                        field: member,
                    });
                }
            }
            _ => {}
        }
    }
    result.unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)))
}

/// The base identifier of a reference: `super`, `this`, a scoped `pkg::Class`
/// (last segment), or a plain identifier.
fn lower_local_root(root: &Value) -> Expr {
    if kids(root).any(|c| tag(c) == "super") {
        return Expr::Ref("super".to_string());
    }
    if kids(root).any(|c| tag(c) == "this") {
        return Expr::Ref("this".to_string());
    }
    if let Some(last) = last_qualified_segment(root) {
        return Expr::Ref(last);
    }
    if let Some(id) = find_deep(root, "SymbolIdentifier") {
        return Expr::Ref(text(id).to_string());
    }
    Expr::Literal(LogicVec::zero(32))
}

/// Lower an array subscript into an `Index` expression. Slices and part-selects
/// are approximated by the base value for now.
fn lower_dimension_subscript(base: Expr, dim: &Value) -> Expr {
    if tag(dim) != "kDimensionScalar" {
        return base;
    }
    let idx = find(dim, "kExpressionList")
        .and_then(|el| kids(el).find(|c| tag(c) == "kExpression"))
        .map(lower_expr)
        .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)));
    Expr::Index {
        base: Box::new(base),
        index: Box::new(idx),
    }
}

/// Lower the `kExpression` arguments inside a `kParenGroup`'s `kArgumentList`.
fn lower_arg_list(paren: &Value) -> Vec<Expr> {
    let mut args = Vec::new();
    if let Some(arglist) = find(paren, "kArgumentList") {
        for a in kids(arglist) {
            if tag(a) == "kExpression" {
                args.push(lower_expr(a));
            }
        }
    }
    args
}

fn fold_binary(cs: &[&Value]) -> Expr {
    // cs = [operand, op, operand, op, operand, ...]
    let mut acc = lower_expr(cs[0]);
    let mut i = 1;
    while i + 1 < cs.len() {
        let op = lower_binop(leaf_op(cs[i]));
        let rhs = lower_expr(cs[i + 1]);
        acc = Expr::Binary {
            op,
            lhs: Box::new(acc),
            rhs: Box::new(rhs),
        };
        i += 2;
    }
    acc
}

/// Strip the surrounding quotes from a `TK_StringLiteral` and resolve the
/// common backslash escapes.
fn unquote(s: &str) -> String {
    let s = s.strip_prefix('"').unwrap_or(s);
    let s = s.strip_suffix('"').unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a `kNumber` into a [`LogicVec`]: sized/based literals (`8'hF0`) via
/// [`LogicVec::parse_sized`], otherwise an unsized 32-bit decimal.
fn lower_number(n: &Value) -> LogicVec {
    if let Some(bd) = find(n, "kBaseDigits") {
        let size = kids(n)
            .find(|c| tag(c) == "TK_DecNumber")
            .and_then(|c| text(c).replace('_', "").parse::<u32>().ok());
        let base_tok = kids(bd)
            .find(|c| tag(c).ends_with("Base"))
            .map(text)
            .unwrap_or("");
        let digits = kids(bd)
            .find(|c| tag(c).ends_with("Digits"))
            .map(text)
            .unwrap_or("");
        let lit = format!(
            "{}{}{}",
            size.map(|s| s.to_string()).unwrap_or_default(),
            base_tok,
            digits
        );
        if let Some(v) = LogicVec::parse_sized(&lit) {
            return v;
        }
    }
    let v = const_int(n).unwrap_or(0);
    LogicVec::from_u64(v as u64, 32)
}

fn lower_binop(op: &str) -> BinOp {
    match op {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "&" => BinOp::And,
        "|" => BinOp::Or,
        "^" => BinOp::Xor,
        "==" => BinOp::Eq,
        "!=" => BinOp::Neq,
        "<" => BinOp::Lt,
        ">" => BinOp::Gt,
        "<=" => BinOp::Le,
        ">=" => BinOp::Ge,
        "<<" => BinOp::Shl,
        ">>" => BinOp::Shr,
        "&&" => BinOp::LogAnd,
        "||" => BinOp::LogOr,
        _ => BinOp::Add,
    }
}

fn lower_unop(op: &str) -> UnaryOp {
    match op {
        "~" => UnaryOp::BitNot,
        "!" => UnaryOp::LogNot,
        "-" => UnaryOp::Neg,
        "+" => UnaryOp::Plus,
        "&" => UnaryOp::ReduceAnd,
        "|" => UnaryOp::ReduceOr,
        "^" => UnaryOp::ReduceXor,
        _ => UnaryOp::BitNot,
    }
}
