use serde_json::Value;

use eevee_ast::Expr;

use crate::cst::{const_int, find, find_deep, kids, leaf_op, tag, text, FeError};
use crate::lower::lower_expr;

pub(crate) fn validate(tree: &Value, source: &str) -> Result<(), FeError> {
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
                if !matches!(
                    tag(child),
                    "kDataDeclaration"
                        | "kAlwaysStatement"
                        | "kInitialStatement"
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
        "kModuleHeader" if find_deep(node, "kFormalParameterList").is_some() => {
            return unsupported(
                find_deep(node, "kFormalParameterList").expect("checked above"),
                source,
            );
        }
        "kDataDeclaration"
            if find_deep(node, "kGateInstance").is_some()
                && find_deep(node, "kActualParameterList").is_some() =>
        {
            return unsupported(
                find_deep(node, "kActualParameterList").expect("checked above"),
                source,
            );
        }
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
