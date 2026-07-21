use std::collections::{HashMap, HashSet};

use eevee_ast::{
    ClassDecl, Expr, FuncDecl, Item, Lvalue, Module, ModuleItem, SourceFile, Stmt, TimingControl,
    VarDecl,
};

use crate::ElabError;

pub(crate) fn validate(file: &SourceFile) -> Result<(), ElabError> {
    validate_hierarchy(file)?;
    for item in &file.items {
        match item {
            Item::Module(module) => validate_items(&module.items)?,
            Item::Package(package) => validate_items(&package.items)?,
            Item::Class(class) => validate_class(class)?,
            Item::Func(function) => validate_function(function)?,
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

fn module_signal_width(module: &Module, name: &str) -> Option<u32> {
    module
        .ports
        .iter()
        .find(|port| port.name == name)
        .map(|port| port.width)
        .or_else(|| {
            module.items.iter().find_map(|item| match item {
                ModuleItem::Var(var) if var.name == name => Some(var.width),
                _ => None,
            })
        })
}

fn validate_module_scope(module: &Module) -> Result<(), ElabError> {
    let mut names = HashSet::new();
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

fn validate_items(items: &[ModuleItem]) -> Result<(), ElabError> {
    for item in items {
        match item {
            ModuleItem::Var(var) => validate_static_var(var)?,
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
            ModuleItem::Always(always) => validate_stmt(&always.body)?,
            ModuleItem::Initial(body) => validate_stmt(body)?,
            ModuleItem::Func(function) => validate_function(function)?,
            ModuleItem::Class(class) => validate_class(class)?,
            ModuleItem::EnumConst { .. }
            | ModuleItem::EnumType { .. }
            | ModuleItem::TypeAlias(_) => {}
        }
    }
    Ok(())
}

fn validate_static_var(var: &VarDecl) -> Result<(), ElabError> {
    validate_type(var.class_name.as_deref())?;
    let Some(init) = &var.init else {
        return Ok(());
    };
    let supported = if var.class_name.is_some() || var.coll.is_some() {
        matches!(init, Expr::Null)
    } else if var.is_string {
        matches!(init, Expr::Str(_))
    } else {
        is_constant_expr(init)
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
        validate_static_var(field)?;
    }
    for method in &class.methods {
        validate_function(method)?;
    }
    if let Some(constructor) = &class.constructor {
        validate_function(constructor)?;
    }
    Ok(())
}

fn validate_function(function: &FuncDecl) -> Result<(), ElabError> {
    validate_type(function.ret_class.as_deref())?;
    for parameter in &function.params {
        validate_type(parameter.class_name.as_deref())?;
        if let Some(default) = &parameter.default {
            validate_expr(default)?;
        }
    }
    validate_stmt(&function.body)
}

fn validate_stmt(stmt: &Stmt) -> Result<(), ElabError> {
    match stmt {
        Stmt::Block(statements) => {
            for statement in statements {
                validate_stmt(statement)?;
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
                    if !is_constant_expr(expr) {
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
            validate_stmt(body)?;
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
            validate_stmt(then_branch)?;
            if let Some(branch) = else_branch {
                validate_stmt(branch)?;
            }
        }
        Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => {
            validate_expr(cond)?;
            validate_stmt(body)?;
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
                validate_stmt(body)?;
            }
            if let Some(body) = default {
                validate_stmt(body)?;
            }
        }
        Stmt::Foreach {
            collection, body, ..
        } => {
            validate_expr(collection)?;
            validate_stmt(body)?;
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
                validate_stmt(branch)?;
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

fn is_constant_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Unary { operand, .. } => is_constant_expr(operand),
        Expr::Binary { lhs, rhs, .. } => is_constant_expr(lhs) && is_constant_expr(rhs),
        Expr::PartSelect { base, left, right } => {
            is_constant_expr(base) && is_constant_expr(left) && is_constant_expr(right)
        }
        Expr::Concat(parts) => parts.iter().all(is_constant_expr),
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
