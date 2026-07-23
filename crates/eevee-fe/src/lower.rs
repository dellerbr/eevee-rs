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
        "kClassDeclaration" => out.push(Item::Class(Box::new(lower_class(n)))),
        "kFunctionDeclaration" | "kTaskDeclaration" => {
            out.push(Item::Func(Box::new(lower_function(n))))
        }
        "kDPIImportItem" => out.push(Item::Func(Box::new(lower_dpi_import(n)))),
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
            for instance in lower_module_instances(item) {
                out.push(ModuleItem::Instance(instance));
            }
        }
        "kNetDeclaration" => {
            for net in lower_net_decl(item) {
                out.push(ModuleItem::Net(net));
            }
        }
        "kAlwaysStatement" => out.push(ModuleItem::Always(lower_always(item))),
        "kInitialStatement" => out.push(ModuleItem::Initial(lower_initial(item))),
        "kContinuousAssignmentStatement" => lower_continuous_assignments(item, out),
        "kFunctionDeclaration" | "kTaskDeclaration" => {
            out.push(ModuleItem::Func(lower_function(item)))
        }
        "kDPIImportItem" => out.push(ModuleItem::Func(lower_dpi_import(item))),
        "kClassDeclaration" => out.push(ModuleItem::Class(Box::new(lower_class(item)))),
        // `localparam`/`parameter NAME = value;` -> a named constant.
        "kParamDeclaration" => {
            if let Some((name, value)) = param_const_pair(item) {
                out.push(ModuleItem::EnumConst { name, value });
            }
        }
        // `typedef enum {...}` contributes named compile-time constants;
        // `typedef struct {...} Name;` contributes a synthetic no-method
        // class (see `lower_struct_typedef`); `typedef <Class>#(...) <alias>;`
        // contributes a type alias.
        "kTypeDeclaration" => {
            lower_enum_consts(item, out);
            if let Some(sd) = lower_struct_typedef(item) {
                out.push(ModuleItem::Class(Box::new(sd)));
            } else if let Some(alias) = lower_class_typedef(item) {
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

fn lower_continuous_assignments(n: &Value, out: &mut Vec<ModuleItem>) {
    let Some(assignments) = find(n, "kAssignmentList") else {
        return;
    };
    let delay = find(n, "kDelay").map(lower_continuous_delay);
    for assignment in kids(assignments) {
        if tag(assignment) == "kNetVariableAssignment" {
            out.push(ModuleItem::ContinuousAssign {
                lhs: lower_lvalue(assignment),
                rhs: lower_rhs(assignment),
                delay: delay.clone(),
            });
        }
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
/// Evaluate the initializer expression rather than taking its first numeric
/// leaf: UVM's operation flags are parameters such as `(1 << 6)`, and reducing
/// every one to `1` makes unrelated bit flags alias each other at runtime.
fn param_const_pair(n: &Value) -> Option<(String, LogicVec)> {
    let pt = find(n, "kParamType")?;
    let name = find(pt, "SymbolIdentifier").map(text)?.to_string();
    if name.is_empty() {
        return None;
    }
    let value = find_deep(n, "kTrailingAssign")
        .and_then(|assign| find(assign, "kExpression"))
        .map(lower_expr)
        .as_ref()
        .and_then(eval_const_expr)
        .or_else(|| const_int(n).map(|value| LogicVec::from_i64(value, 32)))
        .unwrap_or_else(|| LogicVec::zero(32));
    Some((name, value))
}

/// Evaluate the integral expression subset valid in constant parameter
/// initializers. References are intentionally unresolved here (package-level
/// dependency resolution belongs in a later constant-table pass), but literal,
/// unary, arithmetic, bitwise, shift, comparison, and logical expressions are
/// exact and retain four-state behavior through [`LogicVec`].
fn eval_const_expr(expr: &Expr) -> Option<LogicVec> {
    match expr {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Unary { op, operand } => {
            let value = eval_const_expr(operand)?;
            Some(match op {
                UnaryOp::BitNot => value.bitnot(),
                UnaryOp::LogNot => value.lognot(),
                UnaryOp::Neg => value.neg(),
                UnaryOp::Plus => value,
                UnaryOp::ReduceAnd => LogicVec::from_bit(value.reduce_and()),
                UnaryOp::ReduceOr => LogicVec::from_bit(value.reduce_or()),
                UnaryOp::ReduceXor => LogicVec::from_bit(value.reduce_xor()),
            })
        }
        Expr::Binary { op, lhs, rhs } => {
            let left = eval_const_expr(lhs)?;
            let right = eval_const_expr(rhs)?;
            Some(match op {
                BinOp::Add => left.add(&right),
                BinOp::Sub => left.sub(&right),
                BinOp::Mul => left.mul(&right),
                BinOp::And => left.bitand(&right),
                BinOp::Or => left.bitor(&right),
                BinOp::Xor => left.bitxor(&right),
                BinOp::Eq => left.eq_logical(&right),
                BinOp::Neq => left.ne_logical(&right),
                BinOp::Lt => left.ult(&right),
                BinOp::Gt => left.ugt(&right),
                BinOp::Le => left.ule(&right),
                BinOp::Ge => left.uge(&right),
                BinOp::Shl => left.shl(right.to_u64() as u32),
                BinOp::Shr => left.shr(right.to_u64() as u32),
                BinOp::LogAnd => LogicVec::from_u64((left.is_true() && right.is_true()) as u64, 1),
                BinOp::LogOr => LogicVec::from_u64((left.is_true() || right.is_true()) as u64, 1),
            })
        }
        _ => None,
    }
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
        parameters: lower_module_parameters(n),
        ports: lower_module_ports(n),
        items,
    }
}

fn lower_module_parameters(n: &Value) -> Vec<ModuleParameter> {
    let Some(list) = find(n, "kModuleHeader")
        .and_then(|header| find(header, "kFormalParameterListDeclaration"))
        .and_then(|declaration| find_deep(declaration, "kFormalParameterList"))
    else {
        return Vec::new();
    };
    let mut parameters = Vec::new();
    let mut inherited_type = None;
    for parameter in kids(list).filter(|parameter| tag(parameter) == "kParamDeclaration") {
        let Some(param_type) = find(parameter, "kParamType") else {
            continue;
        };
        if let Some(parameter_type) = module_parameter_type(param_type) {
            inherited_type = Some(parameter_type);
        }
        let (width, signed) = inherited_type.unwrap_or((32, false));
        let Some(name) = find(param_type, "SymbolIdentifier")
            .or_else(|| find_deep(param_type, "SymbolIdentifier"))
            .map(text)
        else {
            continue;
        };
        let Some(default) = find(parameter, "kTrailingAssign")
            .and_then(|assign| find(assign, "kExpression"))
            .map(lower_expr)
        else {
            continue;
        };
        parameters.push(ModuleParameter {
            name: name.to_string(),
            width,
            signed,
            default,
        });
    }
    parameters
}

fn module_parameter_type(param_type: &Value) -> Option<(u32, bool)> {
    let primitive = find(param_type, "kTypeInfo")
        .and_then(|type_info| kids(type_info).next())
        .map(tag)?;
    let natural_width = match primitive {
        "byte" => 8,
        "shortint" => 16,
        "int" | "integer" => 32,
        "longint" | "time" => 64,
        _ => 1,
    };
    let width = find(param_type, "kPackedDimensions")
        .filter(|dimensions| kids(dimensions).next().is_some())
        .map(|_| packed_width(param_type))
        .unwrap_or(natural_width);
    let signed = matches!(
        primitive,
        "byte" | "shortint" | "int" | "integer" | "longint"
    ) && find_deep(param_type, "unsigned").is_none();
    Some((width, signed))
}

fn lower_module_ports(n: &Value) -> Vec<Port> {
    let Some(list) = find(n, "kModuleHeader")
        .and_then(|header| find(header, "kParenGroup"))
        .and_then(|group| find(group, "kPortDeclarationList"))
    else {
        return Vec::new();
    };
    kids(list)
        .filter(|declaration| tag(declaration) == "kPortDeclaration")
        .map(|declaration| {
            let dtype = find(declaration, "kDataType");
            let dir = if find_deep(declaration, "output").is_some() {
                PortDir::Output
            } else if find_deep(declaration, "inout").is_some() {
                PortDir::Inout
            } else if find_deep(declaration, "ref").is_some() {
                PortDir::Ref
            } else {
                PortDir::Input
            };
            let name = find(declaration, "kUnqualifiedId")
                .and_then(|id| find(id, "SymbolIdentifier"))
                .map(text)
                .unwrap_or_default()
                .to_string();
            Port {
                name,
                dir,
                width: dtype.map(packed_width).unwrap_or(1),
                signed: find_deep(declaration, "signed").is_some(),
                is_net: kids(declaration).any(|child| tag(child) == "wire"),
            }
        })
        .collect()
}

fn lower_net_decl(n: &Value) -> Vec<NetDecl> {
    let kind = find(n, "kDataType")
        .and_then(lower_net_kind)
        .unwrap_or(NetKind::Wire);
    let dtype = find(n, "kDataTypeImplicitIdDimensions")
        .and_then(|dimensions| find(dimensions, "kDataType"));
    let width = dtype.map(packed_width).unwrap_or(1);
    let signed = find_deep(n, "signed").is_some();
    let delay = find_deep(n, "kDelay").map(lower_continuous_delay);
    let Some(declarations) = find(n, "kNetVariableDeclarationAssign") else {
        return Vec::new();
    };
    kids(declarations)
        .filter(|declaration| tag(declaration) == "kNetVariable")
        .filter_map(|declaration| {
            let name = find(declaration, "SymbolIdentifier").map(text)?.to_string();
            Some(NetDecl {
                name,
                width,
                signed,
                kind,
                delay: delay.clone(),
            })
        })
        .collect()
}

fn lower_net_kind(dtype: &Value) -> Option<NetKind> {
    kids(dtype).find_map(|child| match tag(child) {
        "wire" | "tri" => Some(NetKind::Wire),
        "wand" | "triand" => Some(NetKind::Wand),
        "wor" | "trior" => Some(NetKind::Wor),
        "tri0" => Some(NetKind::Tri0),
        "tri1" => Some(NetKind::Tri1),
        "supply0" => Some(NetKind::Supply0),
        "supply1" => Some(NetKind::Supply1),
        _ => None,
    })
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
    let type_scope = dtype.and_then(class_type_scope);
    let type_args = dtype.map(type_args_of).unwrap_or_default();
    let is_string = dtype.map(is_string_type).unwrap_or(false);
    let is_event = dtype.is_some_and(|value| find_deep(value, "event").is_some());
    let width = dtype.map(packed_width).unwrap_or(1);
    let signed = dtype.is_some_and(|value| find_deep(value, "signed").is_some());
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
                signed,
                class_name: class_name.clone(),
                type_scope: type_scope.clone(),
                type_args: type_args.clone(),
                is_string,
                is_event,
                coll,
                key_class_name: assoc_key_class_name(rv),
                is_static,
                init,
            });
        }
    }
    out
}

fn lower_module_instances(n: &Value) -> Vec<ModuleInstance> {
    let Some(base) = find(n, "kInstantiationBase") else {
        return Vec::new();
    };
    let Some(dtype) = find(base, "kInstantiationType").and_then(|ty| find(ty, "kDataType")) else {
        return Vec::new();
    };
    let Some(module_name) = class_type_name(dtype) else {
        return Vec::new();
    };
    let Some(list) = find(base, "kGateInstanceRegisterVariableList") else {
        return Vec::new();
    };

    kids(list)
        .filter(|instance| tag(instance) == "kGateInstance")
        .map(|instance| {
            let name = find(instance, "SymbolIdentifier")
                .map(text)
                .unwrap_or_default()
                .to_string();
            let connections = find(instance, "kParenGroup")
                .and_then(|group| find(group, "kPortActualList"))
                .map(lower_port_connections)
                .unwrap_or_default();
            ModuleInstance {
                module_name: module_name.clone(),
                name,
                parameters: lower_parameter_overrides(base),
                connections,
            }
        })
        .collect()
}

fn lower_parameter_overrides(n: &Value) -> Vec<ParameterOverride> {
    let Some(actuals) = find_deep(n, "kActualParameterList") else {
        return Vec::new();
    };
    if let Some(named) = find_deep(actuals, "kActualParameterByNameList") {
        return kids(named)
            .filter(|parameter| tag(parameter) == "kParamByName")
            .filter_map(|parameter| {
                let name = find(parameter, "SymbolIdentifier").map(text)?.to_string();
                let value = find(parameter, "kParenGroup")
                    .and_then(|group| find(group, "kExpression"))
                    .map(lower_expr)?;
                Some(ParameterOverride {
                    parameter: Some(name),
                    value,
                })
            })
            .collect();
    }
    find_deep(actuals, "kActualParameterPositionalList")
        .map(|positional| {
            kids(positional)
                .filter(|value| tag(value) == "kExpression")
                .map(|value| ParameterOverride {
                    parameter: None,
                    value: lower_expr(value),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn lower_port_connections(n: &Value) -> Vec<PortConnection> {
    kids(n)
        .filter_map(|connection| match tag(connection) {
            "kActualNamedPort" => {
                let port = find(connection, "SymbolIdentifier")?
                    .get("text")?
                    .as_str()?;
                let expr = find(connection, "kParenGroup")
                    .and_then(|group| find(group, "kExpression"))
                    .map(lower_expr)?;
                Some(PortConnection {
                    port: Some(port.to_string()),
                    expr,
                })
            }
            "kActualPositionalPort" => find(connection, "kExpression").map(|expr| PortConnection {
                port: None,
                expr: lower_expr(expr),
            }),
            _ => None,
        })
        .collect()
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
    let dims = find(rv, "kUnpackedDimensions")
        .and_then(|unpacked| find(unpacked, "kDeclarationDimensions"))
        .or_else(|| find(rv, "kDeclarationDimensions"))?;
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

fn assoc_key_class_name(rv: &Value) -> Option<String> {
    if classify_coll(rv) != Some(CollKind::Assoc) {
        return None;
    }
    let dims = find(rv, "kUnpackedDimensions")
        .and_then(|unpacked| find(unpacked, "kDeclarationDimensions"))
        .or_else(|| find(rv, "kDeclarationDimensions"))?;
    kids(dims)
        .find(|dimension| tag(dimension) == "kDimensionScalar")
        .and_then(|dimension| find_deep(dimension, "SymbolIdentifier"))
        .map(|identifier| text(identifier).to_string())
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

/// The qualifier preceding the final user-type name, if present.
fn class_type_scope(dtype: &Value) -> Option<String> {
    let qid = find_deep(dtype, "kQualifiedId")?;
    let mut segments: Vec<String> = kids(qid)
        .filter(|child| tag(child) == "kUnqualifiedId")
        .filter_map(|child| find(child, "SymbolIdentifier"))
        .map(|id| text(id).to_string())
        .collect();
    if segments.len() < 2 {
        return None;
    }
    segments.pop();
    Some(segments.join("::"))
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
        dpi_name: None,
        is_void: is_task || is_void_return,
        is_virtual,
        params,
        body: Stmt::Block(stmts),
    }
}

fn lower_dpi_import(n: &Value) -> FuncDecl {
    let prototype = find(n, "kFunctionPrototype")
        .or_else(|| find(n, "kTaskPrototype"))
        .expect("DPI import without a callable prototype");
    let mut function = lower_function(prototype);
    function.dpi_name = Some(function.name.clone());
    function
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
    let type_scope = dtype.and_then(class_type_scope);
    let type_args = dtype.map(type_args_of).unwrap_or_default();
    let dir = if find_deep(n, "output").is_some() {
        PortDir::Output
    } else if find_deep(n, "inout").is_some() {
        PortDir::Inout
    } else if find_deep(n, "ref").is_some() {
        PortDir::Ref
    } else {
        PortDir::Input
    };
    let name = find(inner, "kUnqualifiedId")
        .and_then(|u| find(u, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    let default = find(n, "kTrailingAssign").and_then(|assign| {
        find(assign, "kExpression")
            .or_else(|| find(assign, "kClassNew"))
            .map(lower_expr)
    });
    Param {
        name,
        dir,
        width,
        class_name,
        type_scope,
        type_args,
        coll: classify_coll(inner).or_else(|| classify_coll(n)),
        key_class_name: assoc_key_class_name(inner).or_else(|| assoc_key_class_name(n)),
        default,
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
    let mut collection_aliases: Vec<CollectionAlias> = Vec::new();
    let mut consts: Vec<(String, LogicVec)> = Vec::new();
    if let Some(items) = find(n, "kClassItems") {
        for item in kids(items) {
            match tag(item) {
                "kDataDeclaration" => {
                    let mut declarations = lower_data_decl(item);
                    for declaration in &mut declarations {
                        if declaration.coll.is_some() {
                            continue;
                        }
                        let Some(alias_name) = declaration.class_name.as_deref() else {
                            continue;
                        };
                        let Some(alias) = collection_aliases
                            .iter()
                            .find(|alias| alias.alias == alias_name)
                        else {
                            continue;
                        };
                        declaration.coll = Some(alias.kind);
                        declaration.class_name = alias.element.as_ref().map(|ty| ty.name.clone());
                        declaration.type_scope = None;
                        declaration.type_args = alias
                            .element
                            .as_ref()
                            .map(|ty| ty.args.clone())
                            .unwrap_or_default();
                        declaration.key_class_name = alias.key.as_ref().map(|ty| ty.name.clone());
                    }
                    fields.extend(declarations);
                }
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
                    if let Some(alias) = lower_collection_typedef(item) {
                        collection_aliases.push(alias);
                    }
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
        collection_aliases,
        params,
        base_args,
        consts,
        is_struct: false,
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

/// A class-local collection typedef such as
/// `typedef bit edges_t[uvm_phase]`. Fields declared with the alias must keep
/// the collection kind; otherwise `edges_t m_predecessors` is mistaken for a
/// class handle named `edges_t` during elaboration.
fn lower_collection_typedef(n: &Value) -> Option<CollectionAlias> {
    let alias = find(n, "SymbolIdentifier").map(text)?.to_string();
    let kind = classify_coll(n)?;
    let dtype = find(n, "kDataType")?;
    let element = class_type_name(dtype).map(|name| TypeRef {
        name,
        args: type_args_of(dtype),
    });
    let key = assoc_key_class_name(n).map(TypeRef::simple);
    Some(CollectionAlias {
        alias,
        kind,
        element,
        key,
    })
}

/// `typedef struct {members...} Name;` -> a synthetic no-method, no-
/// constructor [`ClassDecl`] (`is_struct: true`). Unpacked struct instances
/// are modeled as ordinary objects reusing the class field/`GetField`
/// machinery (see `ClassInfo`/`ClassDef::struct_fields` in eevee-elab/
/// eevee-ir) — this is what lets a chained read like `lindex.ovrd.m_type`
/// (a struct-typed class field, itself holding a class-handle member)
/// resolve through the *existing* `receiver_class`/`field_class` chain
/// lookup with zero changes to expression codegen.
/// CST: `kTypeDeclaration > [typedef, kDataType > kDataTypePrimitive >
/// kStructType > [struct, kBraceGroup > { kStructUnionMemberList kStructUnion
/// Member* } ], kPackedDimensions, SymbolIdentifier(name), ;]`. Each member is
/// `kStructUnionMember > kDataTypeImplicitIdDimensions > [kDataType,
/// SymbolIdentifier(member name), kUnpackedDimensions]` (the member name is a
/// *direct* child here, unlike a port item's `kUnqualifiedId`-wrapped name).
fn lower_struct_typedef(n: &Value) -> Option<ClassDecl> {
    let dtype = find(n, "kDataType")?;
    let prim = find(dtype, "kDataTypePrimitive")?;
    let struct_ty = find(prim, "kStructType")?;
    let name = find(n, "SymbolIdentifier").map(text)?.to_string();
    let members = find(struct_ty, "kBraceGroup").and_then(|b| find(b, "kStructUnionMemberList"))?;
    let mut fields = Vec::new();
    for m in kids(members) {
        if tag(m) != "kStructUnionMember" {
            continue;
        }
        let Some(inner) = find(m, "kDataTypeImplicitIdDimensions") else {
            continue;
        };
        let mdtype = find(inner, "kDataType");
        let mname = find(inner, "SymbolIdentifier")
            .map(text)
            .unwrap_or_default()
            .to_string();
        if mname.is_empty() {
            continue;
        }
        fields.push(VarDecl {
            name: mname,
            width: mdtype.map(packed_width).unwrap_or(1),
            signed: false,
            class_name: mdtype.and_then(class_type_name),
            type_scope: mdtype.and_then(class_type_scope),
            type_args: mdtype.map(type_args_of).unwrap_or_default(),
            is_string: mdtype.map(is_string_type).unwrap_or(false),
            is_event: mdtype.is_some_and(|value| find_deep(value, "event").is_some()),
            coll: None,
            key_class_name: None,
            is_static: false,
            init: None,
        });
    }
    Some(ClassDecl {
        name,
        base: None,
        fields,
        methods: Vec::new(),
        constructor: None,
        type_aliases: Vec::new(),
        collection_aliases: Vec::new(),
        params: Vec::new(),
        base_args: Vec::new(),
        consts: Vec::new(),
        is_struct: true,
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
        dpi_name: None,
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
        "kCaseStatement" => lower_case(n),
        "kBlockingEventTriggerStatement" => find(n, "kReference")
            .map(lower_reference_chain)
            .map(Stmt::Trigger)
            .unwrap_or(Stmt::Null),
        "kWaitStatement" => {
            let cond = find(n, "kWaitHeader")
                .and_then(|header| find_deep(header, "kExpression"))
                .map(lower_expr)
                .unwrap_or_else(|| Expr::Literal(LogicVec::zero(1)));
            let body = find(n, "kWaitBody")
                .and_then(|wait_body| kids(wait_body).next())
                .map(lower_stmt)
                .unwrap_or(Stmt::Null);
            Stmt::Timed {
                control: TimingControl::Wait(cond),
                body: Box::new(body),
            }
        }
        "kWhileLoopStatement" => {
            let cond = find(n, "kExpression")
                .map(lower_expr)
                .unwrap_or_else(|| Expr::Literal(LogicVec::zero(1)));
            let body = kids(n).last().map(lower_stmt).unwrap_or(Stmt::Null);
            Stmt::While {
                cond,
                body: Box::new(body),
            }
        }
        "kDoWhileLoopStatement" => {
            let cond = find(n, "kExpression")
                .map(lower_expr)
                .unwrap_or_else(|| Expr::Literal(LogicVec::zero(1)));
            let body = kids(n).nth(1).map(lower_stmt).unwrap_or(Stmt::Null);
            Stmt::DoWhile {
                cond,
                body: Box::new(body),
            }
        }
        "kForeverLoopStatement" => {
            let body = kids(n).last().map(lower_stmt).unwrap_or(Stmt::Null);
            Stmt::While {
                cond: Expr::Literal(LogicVec::from_u64(1, 1)),
                body: Box::new(body),
            }
        }
        "kForeachLoopStatement" => {
            let chain = find(n, "kReference")
                .map(lower_reference_chain)
                .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)));
            let (collection, index) = match chain {
                Expr::Index { base, index } => {
                    let name = match *index {
                        Expr::Ref(name) => name,
                        _ => String::new(),
                    };
                    (*base, name)
                }
                other => (other, String::new()),
            };
            let body = kids(n).last().map(lower_stmt).unwrap_or(Stmt::Null);
            Stmt::Foreach {
                collection,
                index,
                body: Box::new(body),
            }
        }
        "kSystemTFCall" => lower_sys_call(n),
        "kJumpStatement" => match kids(n).next().map(tag) {
            Some("return") => Stmt::Return(find(n, "kExpression").map(lower_expr)),
            Some("break") => Stmt::Break,
            Some("continue") => Stmt::Continue,
            _ => Stmt::Null,
        },
        "kStatement" | "kStatementItem" => {
            // wrappers — descend to the inner statement
            kids(n).next().map(lower_stmt).unwrap_or(Stmt::Null)
        }
        "kParBlock" => {
            // Fork-block declarations initialize before any child process
            // starts and are then visible to every branch (LRM 9.3.2).
            let mut setup = Vec::new();
            let mut branches = Vec::new();
            let mut join = ForkJoin::All;
            for c in kids(n) {
                match tag(c) {
                    "kBlockItemStatementList" => {
                        for s in kids(c) {
                            if tag(s) == "kDataDeclaration" {
                                setup.push(lower_stmt(s));
                            } else {
                                branches.push(lower_stmt(s));
                            }
                        }
                    }
                    "join_none" => join = ForkJoin::None,
                    "join_any" => join = ForkJoin::Any,
                    "join" => join = ForkJoin::All,
                    _ => {}
                }
            }
            let fork = Stmt::Fork { branches, join };
            if setup.is_empty() {
                fork
            } else {
                setup.push(fork);
                Stmt::Block(setup)
            }
        }
        _ => Stmt::Null,
    }
}

fn lower_lvalue(n: &Value) -> Lvalue {
    let lp = find(n, "kLPValue");
    let index = lp
        .and_then(|lp| find_deep(lp, "kDimensionScalar"))
        .and_then(|dim| find(dim, "kExpressionList"))
        .and_then(|el| kids(el).find(|c| tag(c) == "kExpression"))
        .map(lower_expr);
    // A scoped target `Class::field = ...` (e.g. a static field write).
    if let Some(segs) = lp.and_then(qualified_segments) {
        return Lvalue {
            scope: Some(segs[0].clone()),
            name: segs[segs.len() - 1].clone(),
            receiver: None,
            index,
        };
    }
    if let Some(reference) = lp.and_then(|value| find(value, "kReference")) {
        match lower_reference_chain(reference) {
            Expr::Index { base, index } => match *base {
                Expr::Field { obj, field } => {
                    return Lvalue {
                        name: field,
                        receiver: Some(obj),
                        index: Some(*index),
                        scope: None,
                    };
                }
                Expr::Ref(name) => {
                    return Lvalue {
                        name,
                        receiver: None,
                        index: Some(*index),
                        scope: None,
                    };
                }
                _ => {}
            },
            Expr::Field { obj, field } => {
                return Lvalue {
                    name: field,
                    receiver: Some(obj),
                    index: None,
                    scope: None,
                };
            }
            Expr::Ref(name) => {
                return Lvalue {
                    name,
                    receiver: None,
                    index: None,
                    scope: None,
                };
            }
            _ => {}
        }
    }
    let name = lp
        .and_then(|value| find_deep(value, "SymbolIdentifier"))
        .map(text)
        .unwrap_or_default()
        .to_string();
    Lvalue {
        name,
        receiver: None,
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
    let base = match (&lhs.scope, &lhs.receiver) {
        (Some(scope), _) => Expr::StaticRef {
            class_name: scope.clone(),
            field: lhs.name.clone(),
        },
        (None, Some(receiver)) => Expr::Field {
            obj: receiver.clone(),
            field: lhs.name.clone(),
        },
        (None, None) => Expr::Ref(lhs.name.clone()),
    };
    match &lhs.index {
        Some(idx) => Expr::Index {
            base: Box::new(base),
            index: Box::new(idx.clone()),
        },
        None => base,
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

fn lower_case(n: &Value) -> Stmt {
    let expr = find(n, "kParenGroup")
        .and_then(|group| find(group, "kExpression"))
        .map(lower_expr)
        .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)));
    let mut items = Vec::new();
    let mut default = None;
    if let Some(list) = find(n, "kCaseItemList") {
        for item in kids(list) {
            match tag(item) {
                "kCaseItem" => {
                    let values = find(item, "kExpressionList")
                        .map(|expressions| {
                            kids(expressions)
                                .filter(|value| tag(value) == "kExpression")
                                .map(lower_expr)
                                .collect()
                        })
                        .unwrap_or_default();
                    let body = kids(item).last().map(lower_stmt).unwrap_or(Stmt::Null);
                    items.push((values, body));
                }
                "kDefaultItem" => {
                    let body = kids(item).last().map(lower_stmt).unwrap_or(Stmt::Null);
                    default = Some(Box::new(body));
                }
                _ => {}
            }
        }
    }
    Stmt::Case {
        expr,
        items,
        default,
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
        "kDelay" => TimingControl::Delay(lower_delay_expr(n)),
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

fn lower_delay_expr(n: &Value) -> Expr {
    find(n, "kDelayValue")
        .or_else(|| find(n, "kParenGroup"))
        .map(|value| {
            find(value, "kExpression")
                .map(lower_expr)
                .unwrap_or_else(|| lower_expr(value))
        })
        .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)))
}

fn lower_continuous_delay(n: &Value) -> ContinuousDelay {
    if find(n, "kDelayValue").is_some() {
        return ContinuousDelay::Single(lower_delay_expr(n));
    }
    let expressions: Vec<_> = find(n, "kParenGroup")
        .and_then(|group| {
            kids(group).find(|child| matches!(tag(child), "kExpression" | "kUntagged"))
        })
        .map(|body| {
            if tag(body) == "kExpression" {
                vec![lower_expr(body)]
            } else {
                kids(body)
                    .filter(|child| tag(child) == "kExpression")
                    .map(lower_expr)
                    .collect()
            }
        })
        .unwrap_or_default();
    match expressions.as_slice() {
        [delay] => ContinuousDelay::Single(delay.clone()),
        [rise, fall] => ContinuousDelay::RiseFall {
            rise: rise.clone(),
            fall: fall.clone(),
        },
        [rise, fall, turn_off] => ContinuousDelay::RiseFallTurnOff {
            rise: rise.clone(),
            fall: fall.clone(),
            turn_off: turn_off.clone(),
        },
        _ => panic!("validated continuous assignment delay must contain one to three values"),
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
    let qid = own_qualified_id(n)?;
    kids(qid)
        .filter(|c| tag(c) == "kUnqualifiedId")
        .filter_map(|u| find(u, "SymbolIdentifier"))
        .map(|id| text(id).to_string())
        .last()
}

/// The `kQualifiedId` belonging to this reference's own local root. This must
/// not use `find_deep`: a method argument may itself contain `P::get()`, and a
/// recursive search from the outer `common.find(P::get())` reference would
/// otherwise steal that nested qualified id and mis-lower the whole call as
/// `P::get`.
fn own_qualified_id(n: &Value) -> Option<&Value> {
    match tag(n) {
        "kQualifiedId" => Some(n),
        "kLocalRoot" => find(n, "kQualifiedId"),
        "kReference" => find(n, "kLocalRoot").and_then(|root| find(root, "kQualifiedId")),
        _ => find(n, "kQualifiedId")
            .or_else(|| find(n, "kLocalRoot").and_then(own_qualified_id))
            .or_else(|| find(n, "kReference").and_then(own_qualified_id)),
    }
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
            if segs.len() >= 2 {
                let member = segs.last().cloned().unwrap_or_default();
                let scope = segs[..segs.len() - 1].join("::");
                return match paren {
                    Some(p) => Expr::StaticCall {
                        class_name: scope,
                        class_args: first_segment_args(reference),
                        method: member,
                        args: lower_arg_list(p),
                    },
                    // Static field read `Class::field` — keep the class name so
                    // codegen can find the correct static slot.
                    // Falls back to Expr::Ref for package-scope enum constants
                    // like `uvm_pkg::UVM_LOW` (the class lookup will fail, and
                    // the const table will catch it instead).
                    None => lower_reference_dimensions(
                        reference,
                        Expr::StaticRef {
                            class_name: scope,
                            field: member,
                        },
                    ),
                };
            }
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
    own_qualified_id(reference)
        .and_then(|qid| kids(qid).find(|c| tag(c) == "kUnqualifiedId"))
        .and_then(|u| find(u, "kActualParameterList"))
        .map(lower_actual_args)
        .unwrap_or_default()
}

/// Collect the segment names of a `Class::member[::...]` scope-resolution
/// reference (`kQualifiedId`), stripping any `#(...)` type parameters. Returns
/// `None` if the reference is not a qualified id.
fn qualified_segments(reference: &Value) -> Option<Vec<String>> {
    let qid = own_qualified_id(reference)?;
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
                    let member = find(c, "kUnqualifiedId")
                        .and_then(|u| find(u, "SymbolIdentifier"))
                        .map(text)
                        .unwrap_or_default()
                        .to_string();
                    result = match find(c, "kParenGroup") {
                        Some(paren) => Some(Expr::MethodCall {
                            obj: Box::new(b),
                            method: member,
                            args: lower_arg_list(paren),
                        }),
                        None => Some(Expr::Field {
                            obj: Box::new(b),
                            field: member,
                        }),
                    };
                }
            }
            // Verible gives names that overlap built-in array methods (e.g.
            // `find`) a distinct extension node even when the receiver is a
            // class handle: `common.find(P::get())` becomes
            // `kReference[kLocalRoot(common), kBuiltinArrayMethodCallExtension
            // [., find, kParenGroup(args)]]`. The parentheses live inside the
            // extension, so there is no enclosing `kReferenceCallBase` for
            // `lower_call_or_ref` to recognize.
            "kBuiltinArrayMethodCallExtension" => {
                if let Some(b) = result.take() {
                    let method = kids(c)
                        .find(|child| !matches!(tag(child), "." | "kParenGroup"))
                        .map(leaf_op)
                        .unwrap_or_default()
                        .to_string();
                    let args = find(c, "kParenGroup")
                        .map(lower_arg_list)
                        .unwrap_or_default();
                    result = Some(Expr::MethodCall {
                        obj: Box::new(b),
                        method,
                        args,
                    });
                }
            }
            _ => {}
        }
    }
    result.unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)))
}

fn lower_reference_dimensions(reference: &Value, mut base: Expr) -> Expr {
    for child in kids(reference) {
        if matches!(
            tag(child),
            "kDimensionScalar" | "kDimensionRange" | "kDimensionSlice"
        ) {
            base = lower_dimension_subscript(base, child);
        }
    }
    base
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

/// Lower an array subscript, packed bit-select, or packed part-select.
fn lower_dimension_subscript(base: Expr, dim: &Value) -> Expr {
    if tag(dim) == "kDimensionRange" {
        let mut bounds = kids(dim)
            .filter(|child| tag(child) == "kExpression")
            .map(lower_expr);
        let left = bounds
            .next()
            .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)));
        let right = bounds
            .next()
            .unwrap_or_else(|| Expr::Literal(LogicVec::zero(32)));
        return Expr::PartSelect {
            base: Box::new(base),
            left: Box::new(left),
            right: Box::new(right),
        };
    }
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
