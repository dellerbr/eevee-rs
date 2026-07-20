//! Front-end parse/lower tests against the real Verible binary.

use eevee_ast::*;
use eevee_fe::parse_source;

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
