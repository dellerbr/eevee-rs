use std::collections::{HashMap, HashSet};

use eevee_ast::{
    BinOp, ClassDecl, Expr, FuncDecl, Item, Lvalue, Module, ModuleItem, PortDir, SourceFile, Stmt,
    TimingControl, UnaryOp, VarDecl,
};

use crate::ElabError;

pub(crate) fn validate(file: &SourceFile) -> Result<(), ElabError> {
    validate_hierarchy(file)?;
    for item in &file.items {
        match item {
            Item::Module(module) => {
                let parameters: HashSet<&str> = module
                    .parameters
                    .iter()
                    .map(|parameter| parameter.name.as_str())
                    .collect();
                validate_items(&module.items, Some(&parameters))?;
            }
            Item::Package(package) => validate_items(&package.items, None)?,
            Item::Class(class) => validate_class(class)?,
            Item::Func(function) => validate_function(function, None)?,
        }
    }
    Ok(())
}

fn validate_hierarchy(file: &SourceFile) -> Result<(), ElabError> {
    let mut modules = HashMap::new();
    for module in file.items.iter().filter_map(|item| match item {
        Item::Module(module) => Some(module),
        _ => None,
    }) {
        if modules.insert(module.name.as_str(), module).is_some() {
            return unsupported(format!("duplicate module declaration '{}'", module.name));
        }
        validate_module_scope(module)?;
        validate_module_parameters(module)?;
        validate_continuous_assignments(module)?;
        validate_procedural_net_writes(module)?;
    }

    for module in modules.values() {
        for instance in module.items.iter().filter_map(|item| match item {
            ModuleItem::Instance(instance) => Some(instance),
            _ => None,
        }) {
            let Some(child) = modules.get(instance.module_name.as_str()) else {
                return unsupported(format!(
                    "unknown module '{}' instantiated as '{}.{}'",
                    instance.module_name, module.name, instance.name
                ));
            };
            validate_parameter_overrides(module, child, instance)?;
            let mut named = HashSet::new();
            for (position, connection) in instance.connections.iter().enumerate() {
                let port = match &connection.port {
                    Some(port) => {
                        if !named.insert(port.as_str()) {
                            return unsupported(format!(
                                "instance '{}.{}' connects port '{}' more than once",
                                module.name, instance.name, port
                            ));
                        }
                        let Some(port) =
                            child.ports.iter().find(|candidate| candidate.name == *port)
                        else {
                            return unsupported(format!(
                                "module '{}' has no port '{}' connected by '{}.{}'",
                                child.name, port, module.name, instance.name
                            ));
                        };
                        port
                    }
                    None if position >= child.ports.len() => {
                        return unsupported(format!(
                            "instance '{}.{}' has more positional connections than module '{}' has ports",
                            module.name, instance.name, child.name
                        ));
                    }
                    None => &child.ports[position],
                };
                let Expr::Ref(actual) = &connection.expr else {
                    return unsupported(format!(
                        "module connection '{}.{}' is not a simple signal reference",
                        instance.name, port.name
                    ));
                };
                let Some(actual_width) = module_signal_width(module, actual) else {
                    return unsupported(format!(
                        "unknown signal '{}' connected to '{}.{}'",
                        actual, instance.name, port.name
                    ));
                };
                if actual_width != port.width {
                    return unsupported(format!(
                        "port width conversion is unsupported for '{}.{}': actual '{}' is {} bits, port is {} bits",
                        instance.name, port.name, actual, actual_width, port.width
                    ));
                }
                if port.is_net
                    && matches!(port.dir, PortDir::Output | PortDir::Inout)
                    && !module_signal_is_net(module, actual)
                {
                    return unsupported(format!(
                        "net port '{}.{}' must connect to a parent net",
                        instance.name, port.name
                    ));
                }
            }
        }
    }

    let mut marks = HashMap::new();
    let mut stack = Vec::new();
    for &name in modules.keys() {
        visit_module(name, &modules, &mut marks, &mut stack)?;
    }
    Ok(())
}

fn validate_continuous_assignments(module: &Module) -> Result<(), ElabError> {
    let drivable: HashSet<&str> = module
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Net(net) => Some(net.name.as_str()),
            _ => None,
        })
        .chain(module.ports.iter().filter_map(|port| {
            (port.is_net && matches!(port.dir, PortDir::Output | PortDir::Inout))
                .then_some(port.name.as_str())
        }))
        .collect();
    for item in &module.items {
        let ModuleItem::ContinuousAssign { lhs, rhs } = item else {
            continue;
        };
        if lhs.receiver.is_some()
            || lhs.index.is_some()
            || lhs.scope.is_some()
            || !drivable.contains(lhs.name.as_str())
        {
            return unsupported(format!(
                "continuous assignment target '{}.{}' is not a whole net",
                module.name, lhs.name
            ));
        }
        if module_signal_signed(module, &lhs.name) {
            return unsupported(format!(
                "signed continuous assignment target '{}.{}' is unsupported",
                module.name, lhs.name
            ));
        }
        validate_continuous_expr(module, rhs)?;
        let target_width = module_signal_width(module, &lhs.name).expect("validated net target");
        let Some(rhs_width) = continuous_expr_width(module, rhs) else {
            return unsupported(format!(
                "continuous assignment expression width is not statically known in module '{}'",
                module.name
            ));
        };
        if rhs_width != target_width {
            return unsupported(format!(
                "continuous assignment width conversion is unsupported for '{}.{}': RHS is {} bits, target is {} bits",
                module.name, lhs.name, rhs_width, target_width
            ));
        }
    }
    Ok(())
}

fn continuous_expr_width(module: &Module, expr: &Expr) -> Option<u32> {
    match expr {
        Expr::Literal(value) => Some(value.width()),
        Expr::Ref(name) => module_signal_width(module, name),
        Expr::Unary { op, operand } => match op {
            UnaryOp::LogNot | UnaryOp::ReduceAnd | UnaryOp::ReduceOr | UnaryOp::ReduceXor => {
                Some(1)
            }
            UnaryOp::BitNot | UnaryOp::Neg | UnaryOp::Plus => {
                continuous_expr_width(module, operand)
            }
        },
        Expr::Binary { op, lhs, rhs } => {
            let left = continuous_expr_width(module, lhs)?;
            let right = continuous_expr_width(module, rhs)?;
            Some(match op {
                BinOp::Eq
                | BinOp::Neq
                | BinOp::Lt
                | BinOp::Gt
                | BinOp::Le
                | BinOp::Ge
                | BinOp::LogAnd
                | BinOp::LogOr => 1,
                BinOp::Shl | BinOp::Shr => left,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::And | BinOp::Or | BinOp::Xor => {
                    left.max(right)
                }
            })
        }
        Expr::Index { .. } => Some(1),
        Expr::PartSelect { left, right, .. } => {
            let Expr::Literal(left) = left.as_ref() else {
                return None;
            };
            let Expr::Literal(right) = right.as_ref() else {
                return None;
            };
            if !left.is_known() || !right.is_known() {
                return None;
            }
            Some(left.to_u64().abs_diff(right.to_u64()) as u32 + 1)
        }
        Expr::Concat(parts) => parts.iter().try_fold(0u32, |width, part| {
            width.checked_add(continuous_expr_width(module, part)?)
        }),
        _ => None,
    }
}

fn validate_continuous_expr(module: &Module, expr: &Expr) -> Result<(), ElabError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Ref(name)
            if module_signal_width(module, name).is_some()
                && !module_signal_signed(module, name) =>
        {
            Ok(())
        }
        Expr::Unary { operand, .. } => validate_continuous_expr(module, operand),
        Expr::Binary { lhs, rhs, .. } => {
            validate_continuous_expr(module, lhs)?;
            validate_continuous_expr(module, rhs)
        }
        Expr::Index { base, index } => {
            validate_continuous_expr(module, base)?;
            let Expr::Literal(index) = index.as_ref() else {
                return unsupported(format!(
                    "continuous packed index must be a constant in module '{}'",
                    module.name
                ));
            };
            let width = continuous_expr_width(module, base).unwrap_or(0);
            if !index.is_known() || index.to_u64() >= u64::from(width) {
                return unsupported(format!(
                    "continuous packed index is out of range in module '{}'",
                    module.name
                ));
            }
            Ok(())
        }
        Expr::PartSelect { base, left, right } => {
            validate_continuous_expr(module, base)?;
            let (Expr::Literal(left), Expr::Literal(right)) = (left.as_ref(), right.as_ref())
            else {
                return unsupported(format!(
                    "continuous part-select bounds must be constant in module '{}'",
                    module.name
                ));
            };
            let width = continuous_expr_width(module, base).unwrap_or(0);
            if !left.is_known()
                || !right.is_known()
                || left.to_u64() >= u64::from(width)
                || right.to_u64() >= u64::from(width)
            {
                return unsupported(format!(
                    "continuous part-select is out of range in module '{}'",
                    module.name
                ));
            }
            Ok(())
        }
        Expr::Concat(parts) => {
            for part in parts {
                validate_continuous_expr(module, part)?;
            }
            Ok(())
        }
        _ => unsupported(format!(
            "unsupported continuous assignment expression in module '{}': {expr:?}",
            module.name
        )),
    }
}

fn validate_procedural_net_writes(module: &Module) -> Result<(), ElabError> {
    let nets: HashSet<&str> = module
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Net(net) => Some(net.name.as_str()),
            _ => None,
        })
        .chain(
            module
                .ports
                .iter()
                .filter(|port| port.is_net)
                .map(|port| port.name.as_str()),
        )
        .collect();
    for item in &module.items {
        let body = match item {
            ModuleItem::Always(always) => Some(&always.body),
            ModuleItem::Initial(body) => Some(body),
            ModuleItem::Func(function) => Some(&function.body),
            _ => None,
        };
        if let Some(name) = body.and_then(|body| procedural_net_write(body, &nets)) {
            return unsupported(format!(
                "procedural assignment to net '{}.{}' is unsupported",
                module.name, name
            ));
        }
    }
    Ok(())
}

fn procedural_net_write<'a>(stmt: &'a Stmt, nets: &HashSet<&str>) -> Option<&'a str> {
    match stmt {
        Stmt::Block(statements) => statements
            .iter()
            .find_map(|statement| procedural_net_write(statement, nets)),
        Stmt::Timed { body, .. }
        | Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::Foreach { body, .. } => procedural_net_write(body, nets),
        Stmt::Blocking { lhs, .. } | Stmt::Nonblocking { lhs, .. }
            if lhs.receiver.is_none()
                && lhs.scope.is_none()
                && nets.contains(lhs.name.as_str()) =>
        {
            Some(lhs.name.as_str())
        }
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => procedural_net_write(then_branch, nets).or_else(|| {
            else_branch
                .as_deref()
                .and_then(|branch| procedural_net_write(branch, nets))
        }),
        Stmt::Case { items, default, .. } => items
            .iter()
            .find_map(|(_, body)| procedural_net_write(body, nets))
            .or_else(|| {
                default
                    .as_deref()
                    .and_then(|body| procedural_net_write(body, nets))
            }),
        Stmt::Fork { branches, .. } => branches
            .iter()
            .find_map(|branch| procedural_net_write(branch, nets)),
        _ => None,
    }
}

fn validate_module_parameters(module: &Module) -> Result<(), ElabError> {
    let mut visible = HashSet::new();
    for parameter in &module.parameters {
        if !is_parameter_constant_expr(&parameter.default, &visible) {
            return unsupported(format!(
                "default value for module parameter '{}.{}' is not a constant expression",
                module.name, parameter.name
            ));
        }
        validate_expr(&parameter.default)?;
        visible.insert(parameter.name.as_str());
    }
    Ok(())
}

fn validate_parameter_overrides(
    parent: &Module,
    child: &Module,
    instance: &eevee_ast::ModuleInstance,
) -> Result<(), ElabError> {
    let named = instance
        .parameters
        .first()
        .is_some_and(|parameter| parameter.parameter.is_some());
    if instance
        .parameters
        .iter()
        .any(|parameter| parameter.parameter.is_some() != named)
    {
        return unsupported(format!(
            "instance '{}.{}' mixes named and positional parameter overrides",
            parent.name, instance.name
        ));
    }

    let mut overridden = HashSet::new();
    let parent_parameters: HashSet<&str> = parent
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect();
    for (position, parameter_override) in instance.parameters.iter().enumerate() {
        let parameter = match &parameter_override.parameter {
            Some(name) => child
                .parameters
                .iter()
                .find(|parameter| parameter.name == *name)
                .ok_or_else(|| ElabError::UnsupportedSemantic {
                    message: format!(
                        "module '{}' has no parameter '{}' overridden by '{}.{}'",
                        child.name, name, parent.name, instance.name
                    ),
                })?,
            None => child.parameters.get(position).ok_or_else(|| {
                ElabError::UnsupportedSemantic {
                    message: format!(
                        "instance '{}.{}' has more positional parameter overrides than module '{}' has parameters",
                        parent.name, instance.name, child.name
                    ),
                }
            })?,
        };
        if !overridden.insert(parameter.name.as_str()) {
            return unsupported(format!(
                "instance '{}.{}' overrides parameter '{}' more than once",
                parent.name, instance.name, parameter.name
            ));
        }
        if !is_parameter_constant_expr(&parameter_override.value, &parent_parameters) {
            return unsupported(format!(
                "override for module parameter '{}.{}' is not a constant expression",
                child.name, parameter.name
            ));
        }
        validate_expr(&parameter_override.value)?;
    }
    Ok(())
}

fn module_signal_width(module: &Module, name: &str) -> Option<u32> {
    module
        .ports
        .iter()
        .find(|port| port.name == name)
        .map(|port| port.width)
        .or_else(|| {
            module.items.iter().find_map(|item| match item {
                ModuleItem::Var(var) if var.name == name => Some(var.width),
                ModuleItem::Net(net) if net.name == name => Some(net.width),
                _ => None,
            })
        })
}

fn module_signal_is_net(module: &Module, name: &str) -> bool {
    module
        .ports
        .iter()
        .any(|port| port.name == name && port.is_net)
        || module
            .items
            .iter()
            .any(|item| matches!(item, ModuleItem::Net(net) if net.name == name))
}

fn module_signal_signed(module: &Module, name: &str) -> bool {
    module
        .ports
        .iter()
        .find(|port| port.name == name)
        .map(|port| port.signed)
        .or_else(|| {
            module.items.iter().find_map(|item| match item {
                ModuleItem::Var(var) if var.name == name => Some(var.signed),
                ModuleItem::Net(net) if net.name == name => Some(net.signed),
                _ => None,
            })
        })
        .unwrap_or(false)
}

fn validate_module_scope(module: &Module) -> Result<(), ElabError> {
    let mut names = HashSet::new();
    for parameter in &module.parameters {
        if !names.insert(parameter.name.as_str()) {
            return unsupported(format!(
                "duplicate declaration '{}' in module '{}'",
                parameter.name, module.name
            ));
        }
    }
    for port in &module.ports {
        if !names.insert(port.name.as_str()) {
            return unsupported(format!(
                "duplicate declaration '{}' in module '{}'",
                port.name, module.name
            ));
        }
    }
    for item in &module.items {
        let name = match item {
            ModuleItem::Var(var) => Some(var.name.as_str()),
            ModuleItem::Net(net) => Some(net.name.as_str()),
            ModuleItem::Instance(instance) => Some(instance.name.as_str()),
            _ => None,
        };
        if let Some(name) = name {
            if !names.insert(name) {
                return unsupported(format!(
                    "duplicate declaration '{}' in module '{}'",
                    name, module.name
                ));
            }
        }
    }
    Ok(())
}

fn visit_module<'a>(
    name: &'a str,
    modules: &HashMap<&'a str, &'a Module>,
    marks: &mut HashMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Result<(), ElabError> {
    match marks.get(name) {
        Some(2) => return Ok(()),
        Some(1) => {
            let start = stack.iter().position(|entry| *entry == name).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(name);
            return unsupported(format!("cyclic module hierarchy: {}", cycle.join(" -> ")));
        }
        _ => {}
    }
    marks.insert(name, 1);
    stack.push(name);
    let module = modules[name];
    for child in module.items.iter().filter_map(|item| match item {
        ModuleItem::Instance(instance) => Some(instance.module_name.as_str()),
        _ => None,
    }) {
        visit_module(child, modules, marks, stack)?;
    }
    stack.pop();
    marks.insert(name, 2);
    Ok(())
}

fn validate_items(
    items: &[ModuleItem],
    module_parameters: Option<&HashSet<&str>>,
) -> Result<(), ElabError> {
    for item in items {
        match item {
            ModuleItem::Var(var) => validate_static_var(var, module_parameters)?,
            ModuleItem::Net(_) => {}
            ModuleItem::Instance(instance) => {
                for connection in &instance.connections {
                    if !matches!(connection.expr, Expr::Ref(_)) {
                        return unsupported(format!(
                            "module connection '{}.{}' is not a simple signal reference",
                            instance.name,
                            connection.port.as_deref().unwrap_or("<positional>")
                        ));
                    }
                }
            }
            ModuleItem::ContinuousAssign { lhs, rhs } => {
                validate_lvalue(lhs)?;
                validate_expr(rhs)?;
            }
            ModuleItem::Always(always) => validate_stmt(&always.body, module_parameters)?,
            ModuleItem::Initial(body) => validate_stmt(body, module_parameters)?,
            ModuleItem::Func(function) => validate_function(function, module_parameters)?,
            ModuleItem::Class(class) => validate_class(class)?,
            ModuleItem::EnumConst { .. }
            | ModuleItem::EnumType { .. }
            | ModuleItem::TypeAlias(_) => {}
        }
    }
    Ok(())
}

fn validate_static_var(
    var: &VarDecl,
    module_parameters: Option<&HashSet<&str>>,
) -> Result<(), ElabError> {
    validate_type(var.class_name.as_deref())?;
    let Some(init) = &var.init else {
        return Ok(());
    };
    let supported = if var.class_name.is_some() || var.coll.is_some() {
        matches!(init, Expr::Null)
    } else if var.is_string {
        matches!(init, Expr::Str(_))
    } else {
        is_constant_expr(init, module_parameters)
    };
    if !supported {
        return unsupported(format!(
            "static variable '{}' has an unsupported initializer",
            var.name
        ));
    }
    validate_expr(init)
}

fn validate_class(class: &ClassDecl) -> Result<(), ElabError> {
    validate_type(class.base.as_deref())?;
    for field in &class.fields {
        validate_static_var(field, None)?;
    }
    for method in &class.methods {
        validate_function(method, None)?;
    }
    if let Some(constructor) = &class.constructor {
        validate_function(constructor, None)?;
    }
    Ok(())
}

fn validate_function(
    function: &FuncDecl,
    module_parameters: Option<&HashSet<&str>>,
) -> Result<(), ElabError> {
    validate_type(function.ret_class.as_deref())?;
    for parameter in &function.params {
        validate_type(parameter.class_name.as_deref())?;
        if let Some(default) = &parameter.default {
            validate_expr(default)?;
        }
    }
    validate_stmt(&function.body, module_parameters)
}

fn validate_stmt(stmt: &Stmt, module_parameters: Option<&HashSet<&str>>) -> Result<(), ElabError> {
    match stmt {
        Stmt::Block(statements) => {
            for statement in statements {
                validate_stmt(statement, module_parameters)?;
            }
        }
        Stmt::VarDecl(var) => {
            if let Some(init) = &var.init {
                validate_expr(init)?;
            }
        }
        Stmt::Timed { control, body } => {
            match control {
                TimingControl::Delay(expr) => {
                    if !is_constant_expr(expr, module_parameters) {
                        return unsupported(
                            "nonconstant procedural delays are not implemented".to_string(),
                        );
                    }
                    validate_expr(expr)?;
                }
                TimingControl::Event(events) => {
                    if events.len() != 1 {
                        return unsupported(
                            "multi-event sensitivity requires WaitAny semantics".to_string(),
                        );
                    }
                    validate_expr(&events[0].expr)?;
                }
                TimingControl::Wait(expr) => validate_expr(expr)?,
            }
            validate_stmt(body, module_parameters)?;
        }
        Stmt::Blocking { lhs, rhs } | Stmt::Nonblocking { lhs, rhs } => {
            validate_lvalue(lhs)?;
            validate_expr(rhs)?;
        }
        Stmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            validate_expr(cond)?;
            validate_stmt(then_branch, module_parameters)?;
            if let Some(branch) = else_branch {
                validate_stmt(branch, module_parameters)?;
            }
        }
        Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => {
            validate_expr(cond)?;
            validate_stmt(body, module_parameters)?;
        }
        Stmt::Case {
            expr,
            items,
            default,
        } => {
            validate_expr(expr)?;
            for (values, body) in items {
                for value in values {
                    validate_expr(value)?;
                }
                validate_stmt(body, module_parameters)?;
            }
            if let Some(body) = default {
                validate_stmt(body, module_parameters)?;
            }
        }
        Stmt::Foreach {
            collection, body, ..
        } => {
            validate_expr(collection)?;
            validate_stmt(body, module_parameters)?;
        }
        Stmt::SysCall { name, args } => {
            let supported = match name.as_str() {
                "$display" | "$write" => true,
                "$swrite" | "$sformat" => matches!(args.first(), Some(Expr::Ref(_))),
                "$cast" => args.len() == 2,
                _ => false,
            };
            if !supported {
                return unsupported(format!("system task {name}"));
            }
            for arg in args {
                validate_expr(arg)?;
            }
        }
        Stmt::Expr(expr) | Stmt::Trigger(expr) => validate_expr(expr)?,
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                validate_expr(expr)?;
            }
        }
        Stmt::Fork { branches, .. } => {
            for branch in branches {
                validate_stmt(branch, module_parameters)?;
            }
        }
        Stmt::Break | Stmt::Continue | Stmt::Null => {}
    }
    Ok(())
}

fn validate_lvalue(lvalue: &Lvalue) -> Result<(), ElabError> {
    if let Some(receiver) = &lvalue.receiver {
        validate_expr(receiver)?;
    }
    if let Some(index) = &lvalue.index {
        validate_expr(index)?;
    }
    Ok(())
}

fn validate_expr(expr: &Expr) -> Result<(), ElabError> {
    match expr {
        Expr::Unary { operand, .. } => validate_expr(operand)?,
        Expr::Binary { lhs, rhs, .. } => {
            validate_expr(lhs)?;
            validate_expr(rhs)?;
        }
        Expr::Call { args, .. } | Expr::New { args } => validate_exprs(args)?,
        Expr::Field { obj, .. } => validate_expr(obj)?,
        Expr::MethodCall { obj, args, .. } => {
            validate_expr(obj)?;
            validate_exprs(args)?;
        }
        Expr::StaticCall {
            class_name, args, ..
        } => {
            validate_type(Some(class_name))?;
            validate_exprs(args)?;
        }
        Expr::Index { base, index } => {
            validate_expr(base)?;
            validate_expr(index)?;
        }
        Expr::PartSelect { base, left, right } => {
            validate_expr(base)?;
            validate_expr(left)?;
            validate_expr(right)?;
        }
        Expr::Concat(parts) => validate_exprs(parts)?,
        Expr::SysCall { name, args } => {
            let supported = match name.as_str() {
                "$sformatf" | "$psprintf" => true,
                "$realtime" | "$time" | "$stime" => args.is_empty(),
                "$typename" => args.len() == 1,
                "$cast" => args.len() == 2,
                _ => false,
            };
            if !supported {
                return unsupported(format!("system function {name}"));
            }
            validate_exprs(args)?;
        }
        Expr::StaticRef { class_name, .. } => validate_type(Some(class_name))?,
        Expr::Literal(_) | Expr::Str(_) | Expr::Ref(_) | Expr::Null => {}
    }
    Ok(())
}

fn validate_exprs(expressions: &[Expr]) -> Result<(), ElabError> {
    for expression in expressions {
        validate_expr(expression)?;
    }
    Ok(())
}

fn is_constant_expr(expr: &Expr, module_parameters: Option<&HashSet<&str>>) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Ref(name) => {
            module_parameters.is_some_and(|parameters| parameters.contains(name.as_str()))
        }
        Expr::Unary { operand, .. } => is_constant_expr(operand, module_parameters),
        Expr::Binary { lhs, rhs, .. } => {
            is_constant_expr(lhs, module_parameters) && is_constant_expr(rhs, module_parameters)
        }
        Expr::PartSelect { base, left, right } => {
            is_constant_expr(base, module_parameters)
                && is_constant_expr(left, module_parameters)
                && is_constant_expr(right, module_parameters)
        }
        Expr::Concat(parts) => parts
            .iter()
            .all(|part| is_constant_expr(part, module_parameters)),
        _ => false,
    }
}

fn is_parameter_constant_expr(expr: &Expr, visible: &HashSet<&str>) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Ref(name) => visible.contains(name.as_str()),
        Expr::Unary { operand, .. } => is_parameter_constant_expr(operand, visible),
        Expr::Binary { lhs, rhs, .. } => {
            is_parameter_constant_expr(lhs, visible) && is_parameter_constant_expr(rhs, visible)
        }
        Expr::PartSelect { base, left, right } => {
            is_parameter_constant_expr(base, visible)
                && is_parameter_constant_expr(left, visible)
                && is_parameter_constant_expr(right, visible)
        }
        Expr::Concat(parts) => parts
            .iter()
            .all(|part| is_parameter_constant_expr(part, visible)),
        _ => false,
    }
}

fn validate_type(class_name: Option<&str>) -> Result<(), ElabError> {
    if class_name.is_some_and(|name| matches!(name, "process" | "mailbox" | "semaphore")) {
        return unsupported(format!(
            "builtin type '{}' has no conformant runtime implementation",
            class_name.expect("checked above")
        ));
    }
    Ok(())
}

fn unsupported<T>(message: String) -> Result<T, ElabError> {
    Err(ElabError::UnsupportedSemantic { message })
}
