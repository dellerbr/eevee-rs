//! Front-end parse/lower tests against the real Verible binary.

use eevee_ast::*;
use eevee_fe::{parse_source, parse_source_conformant, FeError};

const COUNTER: &str = "module top;\n\
  logic clk = 0;\n\
  logic [31:0] c = 0;\n\
  always #5 clk = ~clk;\n\
  always_ff @(posedge clk) c <= c + 1;\n\
endmodule\n";

fn module(file: &SourceFile) -> &Module {
    match &file.items[0] {
        Item::Module(m) => m,
        other => panic!("expected a module, got {other:?}"),
    }
}

#[test]
fn parses_counter_structure() {
    let file = parse_source(COUNTER).expect("verible parse");
    assert_eq!(file.items.len(), 1);
    let m = module(&file);
    assert_eq!(m.name, "top");

    let vars: Vec<&VarDecl> = m
        .items
        .iter()
        .filter_map(|i| match i {
            ModuleItem::Var(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(vars.len(), 2, "two variable decls");
    assert_eq!(vars[0].name, "clk");
    assert_eq!(vars[0].width, 1, "scalar logic is 1 bit");
    assert_eq!(vars[1].name, "c");
    assert_eq!(vars[1].width, 32, "[31:0] is 32 bits");

    let always: Vec<&AlwaysBlock> = m
        .items
        .iter()
        .filter_map(|i| match i {
            ModuleItem::Always(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(always.len(), 2);
    assert_eq!(always[0].kind, AlwaysKind::Plain);
    assert_eq!(always[1].kind, AlwaysKind::Ff);
}

#[test]
fn parses_clock_always() {
    let file = parse_source(COUNTER).expect("verible parse");
    let m = module(&file);
    let clk_always = m
        .items
        .iter()
        .find_map(|i| match i {
            ModuleItem::Always(a) if a.kind == AlwaysKind::Plain => Some(a),
            _ => None,
        })
        .unwrap();

    // always #5 clk = ~clk;
    let Stmt::Timed { control, body } = &clk_always.body else {
        panic!("expected timed statement");
    };
    match control {
        TimingControl::Delay(Expr::Literal(d)) => assert_eq!(d.to_u64(), 5),
        other => panic!("expected #5 delay, got {other:?}"),
    }
    let Stmt::Blocking { lhs, rhs } = &**body else {
        panic!("expected blocking assign");
    };
    assert_eq!(lhs.name, "clk");
    match rhs {
        Expr::Unary {
            op: UnaryOp::BitNot,
            operand,
        } => {
            assert!(matches!(&**operand, Expr::Ref(n) if n == "clk"));
        }
        other => panic!("expected ~clk, got {other:?}"),
    }
}

#[test]
fn parses_counter_always_ff() {
    let file = parse_source(COUNTER).expect("verible parse");
    let m = module(&file);
    let ff = m
        .items
        .iter()
        .find_map(|i| match i {
            ModuleItem::Always(a) if a.kind == AlwaysKind::Ff => Some(a),
            _ => None,
        })
        .unwrap();

    // always_ff @(posedge clk) c <= c + 1;
    let Stmt::Timed { control, body } = &ff.body else {
        panic!("expected timed statement");
    };
    match control {
        TimingControl::Event(evs) => {
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0].edge, Edge::Posedge);
            assert!(matches!(&evs[0].expr, Expr::Ref(n) if n == "clk"));
        }
        other => panic!("expected @(posedge clk), got {other:?}"),
    }
    let Stmt::Nonblocking { lhs, rhs } = &**body else {
        panic!("expected nonblocking assign");
    };
    assert_eq!(lhs.name, "c");
    match rhs {
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => {
            assert!(matches!(&**lhs, Expr::Ref(n) if n == "c"));
            assert!(matches!(&**rhs, Expr::Literal(v) if v.to_u64() == 1));
        }
        other => panic!("expected c + 1, got {other:?}"),
    }
}

#[test]
fn init_values_present() {
    let file = parse_source(COUNTER).expect("verible parse");
    let m = module(&file);
    for i in &m.items {
        if let ModuleItem::Var(v) = i {
            let init = v.init.as_ref().expect("has init");
            assert!(matches!(init, Expr::Literal(val) if val.to_u64() == 0));
        }
    }
}

#[test]
fn parses_module_ports_and_instances() {
    let src = "module child(input logic [3:0] a, output logic [3:0] y);\n\
      initial y = a;\n\
    endmodule\n\
    module top;\n\
      logic [3:0] source, named_y, positional_y;\n\
      child named_child(.a(source), .y(named_y));\n\
      child positional_child(source, positional_y);\n\
    endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let Item::Module(child) = &file.items[0] else {
        panic!("expected child module, got {:?}", file.items[0]);
    };
    assert_eq!(child.ports.len(), 2);
    assert_eq!(child.ports[0].name, "a");
    assert_eq!(child.ports[0].dir, PortDir::Input);
    assert_eq!(child.ports[0].width, 4);
    assert_eq!(child.ports[1].name, "y");
    assert_eq!(child.ports[1].dir, PortDir::Output);

    let Item::Module(top) = &file.items[1] else {
        panic!("expected top module, got {:?}", file.items[1]);
    };
    let instances: Vec<&ModuleInstance> = top
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Instance(instance) => Some(instance),
            _ => None,
        })
        .collect();
    assert_eq!(instances.len(), 2);
    assert_eq!(instances[0].module_name, "child");
    assert_eq!(instances[0].name, "named_child");
    assert!(matches!(
        instances[0].connections.as_slice(),
        [
            PortConnection {
                port: Some(a),
                expr: Expr::Ref(source)
            },
            PortConnection {
                port: Some(y),
                expr: Expr::Ref(named_y)
            }
        ] if a == "a" && source == "source" && y == "y" && named_y == "named_y"
    ));
    assert!(matches!(
        instances[1].connections.as_slice(),
        [
            PortConnection {
                port: None,
                expr: Expr::Ref(source)
            },
            PortConnection {
                port: None,
                expr: Expr::Ref(positional_y)
            }
        ] if source == "source" && positional_y == "positional_y"
    ));
}

#[test]
fn parses_continuous_assignments() {
    let src = "module top;\n\
      logic a, b;\n\
      wire y, z;\n\
      assign y = a & b;\n\
      assign z = a, y = b;\n\
            assign #2 z = b;\n\
    endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let continuous: Vec<(&Lvalue, &Expr, &Option<Expr>)> = module(&file)
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::ContinuousAssign { lhs, rhs, delay } => Some((lhs, rhs, delay)),
            _ => None,
        })
        .collect();
    let nets: Vec<&NetDecl> = module(&file)
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Net(net) => Some(net),
            _ => None,
        })
        .collect();
    assert_eq!(nets.len(), 2);
    assert_eq!(nets[0].name, "y");
    assert_eq!(nets[1].name, "z");
    assert_eq!(continuous.len(), 4);
    assert_eq!(continuous[0].0.name, "y");
    assert!(matches!(
        continuous[0].1,
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } if matches!(&**lhs, Expr::Ref(name) if name == "a")
            && matches!(&**rhs, Expr::Ref(name) if name == "b")
    ));
    assert_eq!(continuous[1].0.name, "z");
    assert!(matches!(continuous[1].1, Expr::Ref(name) if name == "a"));
    assert_eq!(continuous[2].0.name, "y");
    assert!(matches!(continuous[2].1, Expr::Ref(name) if name == "b"));
    assert!(continuous[0].2.is_none());
    assert!(matches!(
        continuous[3],
        (
            Lvalue { name, .. },
            Expr::Ref(source),
            Some(Expr::Literal(delay))
        ) if name == "z" && source == "b" && delay.to_u64() == 2
    ));
}

#[test]
fn parses_resolved_net_types() {
    let src = "module top;\n\
      wire wire_net;\n\
      tri tri_net;\n\
      wand wand_net;\n\
      triand triand_net;\n\
      wor wor_net;\n\
      trior trior_net;\n\
    tri0 tri0_net;\n\
    tri1 tri1_net;\n\
    supply0 supply0_net;\n\
    supply1 supply1_net;\n\
    endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let nets: Vec<_> = module(&file)
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Net(net) => Some((net.name.as_str(), net.kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        nets,
        vec![
            ("wire_net", NetKind::Wire),
            ("tri_net", NetKind::Wire),
            ("wand_net", NetKind::Wand),
            ("triand_net", NetKind::Wand),
            ("wor_net", NetKind::Wor),
            ("trior_net", NetKind::Wor),
            ("tri0_net", NetKind::Tri0),
            ("tri1_net", NetKind::Tri1),
            ("supply0_net", NetKind::Supply0),
            ("supply1_net", NetKind::Supply1),
        ]
    );
}

#[test]
fn conformance_mode_accepts_resolved_and_implicit_strength_nets() {
    let supported = "module top; tri t; wand wa; triand ta; wor wo; trior to;\n\
                     tri0 t0; tri1 t1; supply0 s0; supply1 s1; endmodule";
    parse_source_conformant(supported).expect("resolved and implicit-strength nets supported");

    let resolved_port = "module top(output tri value); endmodule";
    let error =
        parse_source_conformant(resolved_port).expect_err("resolved net ports are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "tri"
    ));

    let strength = "module top; logic source; wire result;\n\
                    assign (strong1, strong0) result = source; endmodule";
    let error = parse_source_conformant(strength).expect_err("drive strengths unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "strong1"
    ));

    let supply_strength = "module top; logic source; wire result;\n\
                           assign (supply1, supply0) result = source; endmodule";
    let error = parse_source_conformant(supply_strength)
        .expect_err("supply drive strengths remain unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "supply1"
    ));
}

#[test]
fn parses_module_parameter_defaults_and_overrides() {
    let src = "module child #(parameter int VALUE = 3, DELAY = 5) ();\n\
      endmodule\n\
      module top;\n\
        child default_child();\n\
        child #(.VALUE(9), .DELAY(2)) named_child();\n\
        child #(11, 1) positional_child();\n\
      endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let Item::Module(child) = &file.items[0] else {
        panic!("expected child module, got {:?}", file.items[0]);
    };
    assert!(matches!(
        child.parameters.as_slice(),
        [
            ModuleParameter {
                name: value,
                width: 32,
                signed: true,
                default: Expr::Literal(value_default),
            },
            ModuleParameter {
                name: delay,
                width: 32,
                signed: true,
                default: Expr::Literal(delay_default),
            }
        ] if value == "VALUE"
            && value_default.to_u64() == 3
            && delay == "DELAY"
            && delay_default.to_u64() == 5
    ));

    let Item::Module(top) = &file.items[1] else {
        panic!("expected top module, got {:?}", file.items[1]);
    };
    let instances: Vec<&ModuleInstance> = top
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::Instance(instance) => Some(instance),
            _ => None,
        })
        .collect();
    assert!(instances[0].parameters.is_empty());
    assert!(matches!(
        instances[1].parameters.as_slice(),
        [
            ParameterOverride {
                parameter: Some(value),
                value: Expr::Literal(value_override),
            },
            ParameterOverride {
                parameter: Some(delay),
                value: Expr::Literal(delay_override),
            }
        ] if value == "VALUE"
            && value_override.to_u64() == 9
            && delay == "DELAY"
            && delay_override.to_u64() == 2
    ));
    assert!(matches!(
        instances[2].parameters.as_slice(),
        [
            ParameterOverride {
                parameter: None,
                value: Expr::Literal(value_override),
            },
            ParameterOverride {
                parameter: None,
                value: Expr::Literal(delay_override),
            }
        ] if value_override.to_u64() == 11 && delay_override.to_u64() == 1
    ));
}

#[test]
fn conformance_mode_accepts_single_and_rejects_multi_delayed_continuous_assignments() {
    let simple =
        "module top;\n  logic source; wire result;\n  assign result = source;\nendmodule\n";
    parse_source_conformant(simple).expect("simple continuous assignment is supported");

    let delayed =
        "module top;\n  logic source; wire result;\n  assign #2 result = source;\nendmodule\n";
    parse_source_conformant(delayed).expect("single assignment delay is supported");

    let multi =
        "module top;\n  logic source; wire result;\n  assign #(1, 2) result = source;\nendmodule\n";
    let error = parse_source_conformant(multi).expect_err("multi-value delay is unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax {
            ref construct,
            line: 3,
            ..
        } if construct == "kUntagged"
    ));

    let declaration_assign = "module top;\n  logic source;\n  wire result = source;\nendmodule\n";
    let error = parse_source_conformant(declaration_assign)
        .expect_err("net declaration assignments are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax {
            ref construct,
            line: 3,
            column: 8,
        } if construct == "kNetDeclarationAssignment"
    ));
}

#[test]
fn conformance_mode_rejects_noop_system_tasks_with_location() {
    let src = "module top;\n  initial $finish;\nendmodule\n";
    let error = parse_source_conformant(src).expect_err("$finish is not implemented");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax {
            ref construct,
            line: 2,
            column: 11,
        } if construct == "SystemTFIdentifier"
    ));
}

#[test]
fn conformance_mode_rejects_unsupported_port_actuals() {
    let expression = "module child(input logic a); endmodule\n\
      module top; logic a, b; child c(.a(a | b)); endmodule\n";
    let error = parse_source_conformant(expression).expect_err("expression actual is unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax {
            ref construct,
            line: 2,
            ..
        } if construct == "kExpression"
    ));

    let hole = "module child(input logic a, output logic y); endmodule\n\
      module top; logic y; child c(, y); endmodule\n";
    let error = parse_source_conformant(hole).expect_err("positional hole is unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax {
            ref construct,
            line: 2,
            ..
        } if construct == "kPortActualList"
    ));
}

#[test]
fn conformance_mode_rejects_operator_and_cast_approximations() {
    let equality = "module top; logic result; initial result = (1 === 1); endmodule";
    let error = parse_source_conformant(equality).expect_err("case equality is not represented");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "==="
    ));

    let cast = "module top; logic [7:0] result; initial result = 8'(1); endmodule";
    let error = parse_source_conformant(cast).expect_err("sized cast is not implemented");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "kCast"
    ));

    let time_literal = "module top; initial #1ns $display(\"done\"); endmodule";
    let error = parse_source_conformant(time_literal)
        .expect_err("explicit-unit procedural delays are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "TK_TimeLiteral"
    ));
}

#[test]
fn conformance_mode_accepts_integral_module_parameterization() {
    let src = "module child #(parameter int WIDTH = 8) (); endmodule\n\
            module top; child #(.WIDTH(16)) dut(); endmodule\n";
    parse_source_conformant(src).expect("integral value parameters are supported");
}

#[test]
fn conformance_mode_rejects_type_module_parameters() {
    let src = "module child #(parameter type T = int) (); endmodule\n\
            module top; child #(logic) dut(); endmodule\n";
    let error = parse_source_conformant(src).expect_err("type parameters are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. }
            if construct == "kTypeAssignment"
    ));

    let type_actual = "module child #(parameter int VALUE = 1) (); endmodule\n\
            module top; child #(logic) dut(); endmodule\n";
    let error = parse_source_conformant(type_actual)
        .expect_err("type-valued module parameter actuals are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. }
            if construct == "kActualParameterPositionalList"
    ));
}

#[test]
fn conformance_mode_rejects_unimplemented_parameter_types_and_widths() {
    let non_int = "module child #(parameter byte VALUE = 1) (); endmodule";
    let error =
        parse_source_conformant(non_int).expect_err("byte parameter coercion is unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "byte"
    ));

    let symbolic_width = "module child #(parameter int WIDTH = 8)\n\
                                (input logic [WIDTH-1:0] value);\n\
                          endmodule\n";
    let error = parse_source_conformant(symbolic_width)
        .expect_err("parameter-dependent packed widths are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. }
            if construct == "kDimensionRange"
    ));

    let localparam = "module child #(localparam int LOCKED = 1) (); endmodule";
    let error = parse_source_conformant(localparam)
        .expect_err("module parameter-port localparams are unsupported");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. } if construct == "localparam"
    ));

    let body_parameter = "module child; parameter int VALUE = 1; endmodule";
    let error = parse_source_conformant(body_parameter)
        .expect_err("module-body parameters are not per-instance yet");
    assert!(matches!(
        error,
        FeError::UnsupportedSyntax { ref construct, .. }
            if construct == "kParamDeclaration"
    ));
}

#[test]
fn conformance_mode_keeps_class_type_parameters_separate() {
    let src = "class box #(type T = int); endclass\n\
            module top; box #(int) value; endmodule\n";
    parse_source_conformant(src).expect("class type parameters remain supported syntax");
}

#[test]
fn conformance_mode_rejects_verible_recovery_trees() {
    let src = "module child; endmodule module top; child instance(); endmodule";
    let error = parse_source_conformant(src).expect_err("keyword identifier requires recovery");
    assert!(matches!(
        error,
        FeError::Syntax {
            ref phase,
            ref text,
            line: 1,
            ..
        } if phase == "parse" && text == "instance"
    ));
}

#[test]
fn nested_static_call_is_preserved_as_method_argument() {
    let src = "module top;\n\
      initial begin\n\
        C common;\n\
        P bld;\n\
        bld = common.find(P::get());\n\
      end\n\
    endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let m = module(&file);
    let ModuleItem::Initial(Stmt::Block(stmts)) = &m.items[0] else {
        panic!("expected initial block, got {:?}", m.items[0]);
    };
    let Stmt::Blocking { rhs, .. } = &stmts[2] else {
        panic!("expected blocking assignment, got {:?}", stmts[2]);
    };
    let Expr::MethodCall { obj, method, args } = rhs else {
        panic!("expected common.find(P::get()), got {rhs:?}");
    };
    assert!(matches!(&**obj, Expr::Ref(name) if name == "common"));
    assert_eq!(method, "find");
    assert!(matches!(
        args.as_slice(),
        [Expr::StaticCall {
            class_name,
            method,
            args,
            ..
        }] if class_name == "P" && method == "get" && args.is_empty()
    ));
}

#[test]
fn package_parameter_constant_expressions_are_evaluated() {
    let src = "package p;\n\
      parameter int COPY = (1 << 0);\n\
      parameter int RECORD = (1 << 6);\n\
      parameter int COMBINED = (4 | 16);\n\
    endpackage\n";
    let file = parse_source(src).expect("verible parse");
    let Item::Package(package) = &file.items[0] else {
        panic!("expected package, got {:?}", file.items[0]);
    };
    let constants: std::collections::HashMap<_, _> = package
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::EnumConst { name, value } => Some((name.as_str(), value.to_u64())),
            _ => None,
        })
        .collect();
    assert_eq!(constants["COPY"], 1);
    assert_eq!(constants["RECORD"], 64);
    assert_eq!(constants["COMBINED"], 20);
}

#[test]
fn implicit_enum_members_increment_from_zero() {
    let src = "package p;\n\
      typedef enum { IMP, NODE, TERMINAL, SCHEDULE, DOMAIN, GLOBAL } phase_type;\n\
    endpackage\n";
    let file = parse_source(src).expect("verible parse");
    let Item::Package(package) = &file.items[0] else {
        panic!("expected package, got {:?}", file.items[0]);
    };
    let constants: std::collections::HashMap<_, _> = package
        .items
        .iter()
        .filter_map(|item| match item {
            ModuleItem::EnumConst { name, value } => Some((name.as_str(), value.to_u64())),
            _ => None,
        })
        .collect();
    assert_eq!(constants["IMP"], 0);
    assert_eq!(constants["NODE"], 1);
    assert_eq!(constants["TERMINAL"], 2);
    assert_eq!(constants["SCHEDULE"], 3);
    assert_eq!(constants["DOMAIN"], 4);
    assert_eq!(constants["GLOBAL"], 5);
}

#[test]
fn class_field_preserves_collection_typedef_kind() {
    let src = "module top;\n\
            class Node;\n\
                typedef bit edges_t[Node];\n\
                edges_t predecessors;\n\
            endclass\n\
        endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let m = module(&file);
    let ModuleItem::Class(class) = &m.items[0] else {
        panic!("expected class, got {:?}", m.items[0]);
    };
    let field = &class.fields[0];
    assert_eq!(field.name, "predecessors");
    assert_eq!(field.coll, Some(CollKind::Assoc));
    assert_eq!(field.class_name, None);
    assert_eq!(field.key_class_name.as_deref(), Some("Node"));
}

#[test]
fn foreach_preserves_collection_and_index() {
    let src = "module top;\n\
          initial begin\n\
            int values[int];\n\
            foreach (values[key]) values[key] = key;\n\
          end\n\
        endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let m = module(&file);
    let ModuleItem::Initial(Stmt::Block(stmts)) = &m.items[0] else {
        panic!("expected initial block, got {:?}", m.items[0]);
    };
    let Stmt::Foreach {
        collection, index, ..
    } = &stmts[1]
    else {
        panic!("expected foreach, got {:?}", stmts[1]);
    };
    assert!(matches!(collection, Expr::Ref(name) if name == "values"));
    assert_eq!(index, "key");
}

#[test]
fn class_scoped_typedef_static_call_keeps_full_scope() {
    let src = "module top;\n\
          initial begin\n\
            Product product;\n\
            product = Product::type_id::create();\n\
          end\n\
        endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let m = module(&file);
    let ModuleItem::Initial(Stmt::Block(stmts)) = &m.items[0] else {
        panic!("expected initial block, got {:?}", m.items[0]);
    };
    let Stmt::Blocking { rhs, .. } = &stmts[1] else {
        panic!("expected assignment, got {:?}", stmts[1]);
    };
    assert!(matches!(
        rhs,
        Expr::StaticCall {
            class_name,
            method,
            args,
            ..
        } if class_name == "Product::type_id" && method == "create" && args.is_empty()
    ));
}

#[test]
fn function_formal_directions_are_preserved() {
    let src = "module top;\n\
          function void transfer(input int source, output int result,\n\
                                 inout int state, ref int shared);\n\
          endfunction\n\
        endmodule\n";
    let file = parse_source(src).expect("verible parse");
    let m = module(&file);
    let ModuleItem::Func(func) = &m.items[0] else {
        panic!("expected function, got {:?}", m.items[0]);
    };
    let directions: Vec<PortDir> = func.params.iter().map(|param| param.dir).collect();
    assert_eq!(
        directions,
        vec![
            PortDir::Input,
            PortDir::Output,
            PortDir::Inout,
            PortDir::Ref,
        ]
    );
}
