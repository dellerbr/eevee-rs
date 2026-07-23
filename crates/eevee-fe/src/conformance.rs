use serde_json::Value;

use eevee_ast::Expr;

use crate::cst::{const_int, find, find_deep, kids, leaf_op, tag, text, FeError};
use crate::lower::lower_expr;

pub(crate) fn validate(tree: &Value, tokens: &[Value], source: &str) -> Result<(), FeError> {
    if let Some(strength) = tokens.iter().find(|token| {
        let token_tag = tag(token);
        matches!(
            token_tag,
            "supply0"
                | "supply1"
                | "strong0"
                | "strong1"
                | "pull0"
                | "pull1"
                | "weak0"
                | "weak1"
                | "highz0"
                | "highz1"
                | "large"
                | "medium"
                | "small"
        ) && !(matches!(token_tag, "supply0" | "supply1")
            && is_internal_supply_net_token(tree, token))
    }) {
        return unsupported(strength, source);
    }
    validate_node(tree, source)
}

fn validate_node(node: &Value, source: &str) -> Result<(), FeError> {
    match tag(node) {
        "kDescriptionList" => {
            for child in kids(node) {
                if !matches!(
                    tag(child),
                    "kModuleDeclaration"
                        | "kPackageDeclaration"
                        | "kClassDeclaration"
                        | "kFunctionDeclaration"
                        | "kTaskDeclaration"
                        | "kDPIImportItem"
                ) {
                    return unsupported(child, source);
                }
            }
        }
        "kModuleItemList" | "kPackageItemList" => {
            for child in kids(node) {
                if tag(node) == "kModuleItemList" && tag(child) == "kParamDeclaration" {
                    return unsupported(child, source);
                }
                if !matches!(
                    tag(child),
                    "kDataDeclaration"
                        | "kNetDeclaration"
                        | "kAlwaysStatement"
                        | "kInitialStatement"
                        | "kContinuousAssignmentStatement"
                        | "kFunctionDeclaration"
                        | "kTaskDeclaration"
                        | "kDPIImportItem"
                        | "kClassDeclaration"
                        | "kParamDeclaration"
                        | "kTypeDeclaration"
                        | "kModuleItemList"
                        | "kPackageItemList"
                ) {
                    return unsupported(child, source);
                }
            }
        }
        "kModuleHeader" => {
            if let Some(parameters) = find_deep(node, "kFormalParameterList") {
                validate_module_parameter_list(parameters, source)?;
            }
        }
        "kDataDeclaration" if find_deep(node, "kGateInstance").is_some() => {
            if let Some(parameters) = find_deep(node, "kActualParameterList") {
                validate_module_parameter_actuals(parameters, source)?;
            }
        }
        "kContinuousAssignmentStatement" => {
            if let Some(delay) = find(node, "kDelay") {
                validate_single_assignment_delay(delay, source)?;
            }
            if let Some(strength) = find(node, "kDriveStrength") {
                return unsupported(strength, source);
            }
            let Some(assignments) = find(node, "kAssignmentList") else {
                return unsupported(node, source);
            };
            validate_separated_list(assignments, "kNetVariableAssignment", source)?;
        }
        "kNetDeclaration" => {
            let Some(net_type) =
                find(node, "kDataType").and_then(|data_type| kids(data_type).next())
            else {
                return unsupported(node, source);
            };
            if !matches!(
                tag(net_type),
                "wire"
                    | "tri"
                    | "wand"
                    | "triand"
                    | "wor"
                    | "trior"
                    | "tri0"
                    | "tri1"
                    | "supply0"
                    | "supply1"
            ) {
                return unsupported(net_type, source);
            }
            if let Some(delay) = find_deep(node, "kDelay") {
                return unsupported(delay, source);
            }
            if let Some(strength) = find_deep(node, "kDriveStrength") {
                return unsupported(strength, source);
            }
            if let Some(initializer) = find_deep(node, "kNetDeclarationAssignment") {
                return unsupported(initializer, source);
            }
            let Some(declarations) = find(node, "kNetVariableDeclarationAssign") else {
                return unsupported(node, source);
            };
            validate_separated_list(declarations, "kNetVariable", source)?;
            for declaration in kids(declarations).filter(|child| tag(child) == "kNetVariable") {
                if find(declaration, "SymbolIdentifier").is_none()
                    || find(declaration, "kUnpackedDimensions")
                        .is_some_and(|dimensions| kids(dimensions).next().is_some())
                {
                    return unsupported(declaration, source);
                }
            }
        }
        "kPortDeclaration" => {
            if let Some(net_type) = kids(node).find(|child| {
                matches!(
                    tag(child),
                    "tri"
                        | "wand"
                        | "triand"
                        | "wor"
                        | "trior"
                        | "tri0"
                        | "tri1"
                        | "supply0"
                        | "supply1"
                )
            }) {
                return unsupported(net_type, source);
            }
        }
        "kNetVariableAssignment" => {
            let Some(lhs) = find(node, "kLPValue") else {
                return unsupported(node, source);
            };
            if find_deep(lhs, "kDimensionScalar").is_some()
                || find_deep(lhs, "kDimensionRange").is_some()
                || find_deep(lhs, "kDimensionSlice").is_some()
                || find_deep(lhs, "kHierarchyExtension").is_some()
                || find_deep(lhs, "kQualifiedId").is_some()
                || find_deep(lhs, "SymbolIdentifier").is_none()
            {
                return unsupported(lhs, source);
            }
        }
        "kPackedDimensions" => validate_literal_packed_dimensions(node, source)?,
        "kSystemTFCall" => {
            let identifier = find_deep(node, "SystemTFIdentifier")
                .expect("Verible system call without an identifier");
            if !matches!(
                text(identifier),
                "$display"
                    | "$write"
                    | "$swrite"
                    | "$sformat"
                    | "$cast"
                    | "$sformatf"
                    | "$psprintf"
                    | "$realtime"
                    | "$time"
                    | "$stime"
                    | "$typename"
            ) {
                return unsupported(identifier, source);
            }
        }
        "kPortActualList" => {
            let children: Vec<_> = kids(node).collect();
            if children.is_empty()
                || children.iter().enumerate().any(|(index, child)| {
                    if index % 2 == 0 {
                        !matches!(tag(child), "kActualNamedPort" | "kActualPositionalPort")
                    } else {
                        tag(child) != ","
                    }
                })
            {
                return unsupported(node, source);
            }
        }
        "kActualNamedPort" | "kActualPositionalPort" => {
            let Some(expression) = find_deep(node, "kExpression") else {
                return unsupported(node, source);
            };
            if !matches!(lower_expr(expression), Expr::Ref(_)) {
                return unsupported(expression, source);
            }
        }
        "kCast" | "kIncrementDecrementExpression" => return unsupported(node, source),
        "TK_TimeLiteral" => return unsupported(node, source),
        "kBinaryExpression" => {
            let children: Vec<_> = kids(node).collect();
            for operator in children.iter().skip(1).step_by(2) {
                if !matches!(
                    leaf_op(operator),
                    "+" | "-"
                        | "*"
                        | "&"
                        | "|"
                        | "^"
                        | "=="
                        | "!="
                        | "<"
                        | ">"
                        | "<="
                        | ">="
                        | "<<"
                        | ">>"
                        | "&&"
                        | "||"
                ) {
                    return unsupported(operator, source);
                }
            }
        }
        "kUnaryPrefixExpression" => {
            let Some(operator) = kids(node).next() else {
                return unsupported(node, source);
            };
            if !matches!(leaf_op(operator), "~" | "!" | "-" | "+" | "&" | "|" | "^") {
                return unsupported(operator, source);
            }
        }
        "kAssignModifyStatement" => {
            let supported = kids(node).any(|child| {
                matches!(
                    tag(child),
                    "+=" | "-=" | "*=" | "&=" | "|=" | "^=" | "<<=" | ">>="
                )
            });
            if !supported {
                return unsupported(node, source);
            }
        }
        "kNumber" if find(node, "kBaseDigits").is_none() && const_int(node).is_none() => {
            return unsupported(node, source);
        }
        "kExpression" => {
            if let Some(root) = kids(node).next() {
                if !matches!(
                    tag(root),
                    "kNumber"
                        | "TK_StringLiteral"
                        | "null"
                        | "kConcatenationExpression"
                        | "kSystemTFCall"
                        | "kClassNew"
                        | "kBinaryExpression"
                        | "kUnaryPrefixExpression"
                        | "kParenGroup"
                        | "kParenExpression"
                        | "kFunctionCall"
                        | "kReferenceCallBase"
                        | "kReference"
                        | "kLocalRoot"
                        | "kUnqualifiedId"
                        | "SymbolIdentifier"
                ) {
                    return unsupported(root, source);
                }
            }
        }
        statement if statement.ends_with("Statement") && !supported_statement(statement) => {
            return unsupported(node, source);
        }
        _ => {}
    }

    for child in kids(node) {
        validate_node(child, source)?;
    }
    Ok(())
}

fn is_internal_supply_net_token(tree: &Value, token: &Value) -> bool {
    let Some(start) = token.get("start").and_then(Value::as_u64) else {
        return false;
    };
    if tag(tree) == "kNetDeclaration" {
        if let Some(net_type) = find(tree, "kDataType").and_then(|data_type| kids(data_type).next())
        {
            if tag(net_type) == tag(token)
                && net_type.get("start").and_then(Value::as_u64) == Some(start)
            {
                return true;
            }
        }
    }
    kids(tree).any(|child| is_internal_supply_net_token(child, token))
}

fn supported_statement(tag: &str) -> bool {
    matches!(
        tag,
        "kAlwaysStatement"
            | "kInitialStatement"
            | "kProceduralTimingControlStatement"
            | "kBlockingAssignmentStatement"
            | "kNonblockingAssignmentStatement"
            | "kAssignModifyStatement"
            | "kConditionalStatement"
            | "kCaseStatement"
            | "kBlockingEventTriggerStatement"
            | "kWaitStatement"
            | "kWhileLoopStatement"
            | "kDoWhileLoopStatement"
            | "kForeverLoopStatement"
            | "kForeachLoopStatement"
            | "kJumpStatement"
            | "kStatement"
            | "kNullStatement"
    )
}

fn validate_separated_list(node: &Value, item_tag: &str, source: &str) -> Result<(), FeError> {
    let children: Vec<_> = kids(node).collect();
    if children.is_empty()
        || children.iter().enumerate().any(|(index, child)| {
            if index % 2 == 0 {
                tag(child) != item_tag
            } else {
                tag(child) != ","
            }
        })
    {
        return unsupported(node, source);
    }
    Ok(())
}

fn validate_single_assignment_delay(node: &Value, source: &str) -> Result<(), FeError> {
    if let Some(value) = find(node, "kDelayValue") {
        if kids(value).count() != 1 {
            return unsupported(value, source);
        }
        return Ok(());
    }
    let Some(group) = find(node, "kParenGroup") else {
        return unsupported(node, source);
    };
    let children: Vec<_> = kids(group).collect();
    if children.len() != 3 || tag(children[1]) != "kExpression" {
        return unsupported(children.get(1).copied().unwrap_or(group), source);
    }
    Ok(())
}

fn validate_module_parameter_list(node: &Value, source: &str) -> Result<(), FeError> {
    validate_separated_list(node, "kParamDeclaration", source)?;
    let mut inherited_int = false;
    for parameter in kids(node).filter(|child| tag(child) == "kParamDeclaration") {
        if let Some(localparam) = kids(parameter).find(|child| tag(child) == "localparam") {
            return unsupported(localparam, source);
        }
        if let Some(type_assignment) = find(parameter, "kTypeAssignment") {
            return unsupported(type_assignment, source);
        }
        let Some(param_type) = find(parameter, "kParamType") else {
            return unsupported(parameter, source);
        };
        let type_tokens: Vec<_> = find(param_type, "kTypeInfo")
            .map(kids)
            .into_iter()
            .flatten()
            .collect();
        if let Some(token) = type_tokens.first() {
            if type_tokens.len() != 1 || tag(token) != "int" {
                return unsupported(token, source);
            }
            inherited_int = true;
        } else if !inherited_int {
            return unsupported(param_type, source);
        }
        if let Some(dimensions) = find(param_type, "kPackedDimensions") {
            if kids(dimensions).next().is_some() {
                return unsupported(dimensions, source);
            }
        }
        if let Some(dimensions) = find(param_type, "kUnpackedDimensions") {
            if kids(dimensions).next().is_some() {
                return unsupported(dimensions, source);
            }
        }
        let Some(default) =
            find(parameter, "kTrailingAssign").and_then(|assign| find(assign, "kExpression"))
        else {
            return unsupported(parameter, source);
        };
        validate_node(default, source)?;
    }
    Ok(())
}

fn validate_literal_packed_dimensions(node: &Value, source: &str) -> Result<(), FeError> {
    for child in kids(node) {
        if tag(child) == "kDimensionRange" {
            let expressions: Vec<_> = kids(child)
                .filter(|candidate| tag(candidate) == "kExpression")
                .collect();
            if expressions.len() != 2
                || expressions
                    .iter()
                    .any(|expression| kids(expression).next().map(tag) != Some("kNumber"))
            {
                return unsupported(child, source);
            }
        } else {
            validate_literal_packed_dimensions(child, source)?;
        }
    }
    Ok(())
}

fn validate_module_parameter_actuals(node: &Value, source: &str) -> Result<(), FeError> {
    if let Some(named) = find_deep(node, "kActualParameterByNameList") {
        validate_separated_list(named, "kParamByName", source)?;
        for parameter in kids(named).filter(|child| tag(child) == "kParamByName") {
            let Some(expression) =
                find(parameter, "kParenGroup").and_then(|group| find(group, "kExpression"))
            else {
                return unsupported(parameter, source);
            };
            validate_node(expression, source)?;
        }
        return Ok(());
    }
    let Some(positional) = find_deep(node, "kActualParameterPositionalList") else {
        return unsupported(node, source);
    };
    validate_separated_list(positional, "kExpression", source)?;
    for expression in kids(positional).filter(|child| tag(child) == "kExpression") {
        validate_node(expression, source)?;
    }
    Ok(())
}

fn unsupported<T>(node: &Value, source: &str) -> Result<T, FeError> {
    let offset = first_offset(node).unwrap_or(0).min(source.len());
    let bytes = source.as_bytes();
    let line = bytes[..offset]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
        + 1;
    let line_start = bytes[..offset]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    Err(FeError::UnsupportedSyntax {
        construct: tag(node).to_string(),
        line,
        column: offset - line_start + 1,
    })
}

fn first_offset(node: &Value) -> Option<usize> {
    if let Some(offset) = node.get("start").and_then(Value::as_u64) {
        return Some(offset as usize);
    }
    kids(node).find_map(first_offset)
}
