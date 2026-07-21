//! End-to-end: SystemVerilog **source text** → Verible parse → AST → elaborate
//! → IR → run on the event-driven kernel. This is the P2 milestone: a real
//! `.sv` design (not hand-built IR) producing correct behavior.

use eevee_core::SimTime;
use eevee_elab::{elaborate, elaborate_conformant, ElabError};
use eevee_fe::{parse_source, parse_source_conformant};
use eevee_ir::Interp;

const COUNTER: &str = "module top;\n\
  logic clk = 0;\n\
  logic [31:0] c = 0;\n\
  always #5 clk = ~clk;\n\
  always_ff @(posedge clk) c <= c + 1;\n\
endmodule\n";

#[test]
fn counter_runs_from_sv_source() {
    let file = parse_source(COUNTER).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);

    let c = sim.kernel().find_net("c").expect("net c exists");
    let clk = sim.kernel().find_net("clk").expect("net clk exists");

    // Initial values from `= 0`.
    assert_eq!(sim.kernel().net_value(c).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(clk).to_u64(), 0);

    // #5 in the default 1ns/1ps timescale = 5_000_000 fs; clock period =
    // 10_000_000 fs; N-th posedge at (2N-1)*5_000_000 fs.
    let n = 200u64;
    let half = 5_000_000u64;
    sim.run_until(Some(SimTime::from_fs((2 * n - 1) * half)));

    assert_eq!(
        sim.kernel().net_value(c).to_u64(),
        n,
        "counter reached N from SV source"
    );
    // After the N-th (odd) posedge the clock is high.
    assert_eq!(sim.kernel().net_value(clk).to_u64(), 1, "clock toggled");
}

#[test]
fn child_instances_propagate_named_and_positional_ports() {
    let src = "module child(input logic [7:0] source, output logic [7:0] result);\n\
      initial begin\n\
        #5 result = source + 1;\n\
      end\n\
    endmodule\n\
    module top;\n\
      logic [7:0] source = 41;\n\
      logic [7:0] named_result = 0;\n\
      logic [7:0] positional_result = 0;\n\
      child named_child(.source(source), .result(named_result));\n\
      child positional_child(source, positional_result);\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);

    let named = sim
        .kernel()
        .find_net("named_result")
        .expect("named result net");
    let positional = sim
        .kernel()
        .find_net("positional_result")
        .expect("positional result net");
    assert_eq!(sim.kernel().net_value(named).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(positional).to_u64(), 0);

    sim.run_until(Some(SimTime::from_fs(5_000_000)));

    assert_eq!(sim.kernel().net_value(named).to_u64(), 42);
    assert_eq!(sim.kernel().net_value(positional).to_u64(), 42);
}

#[test]
fn child_instances_apply_default_named_and_positional_parameters() {
    let src = "module child #(parameter int VALUE = 3,\n\
                                parameter int BIAS = VALUE + 1)\n\
               (output logic [7:0] result);\n\
      logic [7:0] configured = VALUE + BIAS;\n\
      initial result = configured;\n\
    endmodule\n\
      module top #(parameter int BASE = 7);\n\
      logic [7:0] default_result = 0;\n\
      logic [7:0] named_result = 0;\n\
      logic [7:0] positional_result = 0;\n\
      child default_child(.result(default_result));\n\
        child #(.VALUE(BASE + 2)) named_child(.result(named_result));\n\
        child #(11, 1) positional_child(.result(positional_result));\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    sim.run();

    let default = sim
        .kernel()
        .find_net("default_result")
        .expect("default result net");
    let named = sim
        .kernel()
        .find_net("named_result")
        .expect("named result net");
    let positional = sim
        .kernel()
        .find_net("positional_result")
        .expect("positional result net");
    assert_eq!(sim.kernel().net_value(default).to_u64(), 7);
    assert_eq!(sim.kernel().net_value(named).to_u64(), 19);
    assert_eq!(sim.kernel().net_value(positional).to_u64(), 12);
}

#[test]
fn module_parameter_values_control_instance_delays() {
    let src = "module child #(parameter int VALUE = 3,\n\
                  parameter int DELAY = 5)\n\
               (output logic [7:0] result);\n\
      initial #(DELAY + 1) result = VALUE;\n\
    endmodule\n\
    module top;\n\
      logic [7:0] default_result = 0;\n\
      logic [7:0] named_result = 0;\n\
      logic [7:0] positional_result = 0;\n\
      child default_child(.result(default_result));\n\
      child #(.VALUE(9), .DELAY(2)) named_child(.result(named_result));\n\
      child #(11, 1) positional_child(.result(positional_result));\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);

    let default = sim
        .kernel()
        .find_net("default_result")
        .expect("default result net");
    let named = sim
        .kernel()
        .find_net("named_result")
        .expect("named result net");
    let positional = sim
        .kernel()
        .find_net("positional_result")
        .expect("positional result net");

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(default).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(named).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(positional).to_u64(), 11);

    sim.run_until(Some(SimTime::from_fs(3_000_000)));
    assert_eq!(sim.kernel().net_value(default).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(named).to_u64(), 9);

    sim.run_until(Some(SimTime::from_fs(6_000_000)));
    assert_eq!(sim.kernel().net_value(default).to_u64(), 3);
}

#[test]
fn conformance_mode_rejects_invalid_module_parameter_overrides() {
    let cases = [
        (
            "module child #(parameter int VALUE = 1) (); endmodule\n\
         module top; child #(.MISSING(2)) dut(); endmodule\n",
            "no parameter 'MISSING'",
        ),
        (
            "module child #(parameter int VALUE = 1) (); endmodule\n\
         module top; child #(.VALUE(2), .VALUE(3)) dut(); endmodule\n",
            "overrides parameter 'VALUE' more than once",
        ),
        (
            "module child #(parameter int VALUE = 1) (); endmodule\n\
         module top; child #(2, 3) dut(); endmodule\n",
            "more positional parameter overrides",
        ),
        (
            "module child #(parameter int VALUE = 1) (); endmodule\n\
         module top; child #(.VALUE(MISSING)) dut(); endmodule\n",
            "not a constant expression",
        ),
    ];

    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("parameter syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("invalid parameter override must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
}

#[test]
fn conformance_mode_rejects_resilient_callable_stubs() {
    let src = "class broken;\n\
      function int value();\n\
        return missing_name;\n\
      endfunction\n\
    endclass\n\
    module top; endmodule\n";
    let file = parse_source_conformant(src).expect("class parses");
    let error = match elaborate_conformant(&file, &Interp) {
        Ok(_) => panic!("stubbed callable must not pass conformance mode"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ElabError::CallableStubs { ref stubs }
            if stubs.iter().any(|stub| stub.callable == "broken::value")
    ));
}

#[test]
fn conformant_constant_initializers_apply_all_supported_operators() {
    let src = "module top;\n\
      logic less = (2 < 1);\n\
      logic [3:0] shifted = (1 << 2);\n\
      logic negated = !1;\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    let less = sim.kernel().find_net("less").expect("less net");
    let shifted = sim.kernel().find_net("shifted").expect("shifted net");
    let negated = sim.kernel().find_net("negated").expect("negated net");
    assert_eq!(sim.kernel().net_value(less).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(shifted).to_u64(), 4);
    assert_eq!(sim.kernel().net_value(negated).to_u64(), 0);
}

#[test]
fn conformance_mode_rejects_placeholder_builtin_classes() {
    let src = "module top;\n\
      mailbox messages;\n\
      initial messages = new();\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("mailbox syntax parses");
    let error = match elaborate_conformant(&file, &Interp) {
        Ok(_) => panic!("mailbox placeholder must not pass conformance mode"),
        Err(error) => error,
    };
    assert!(matches!(
      error,
      ElabError::UnsupportedSemantic { ref message }
        if message.contains("mailbox") && message.contains("no conformant runtime")
    ));
}

#[test]
fn conformance_mode_rejects_cyclic_module_hierarchy() {
    let src = "module first; second child(); endmodule\n\
      module second; first child(); endmodule\n";
    let file = parse_source_conformant(src).expect("cycle syntax parses");
    let error = match elaborate_conformant(&file, &Interp) {
        Ok(_) => panic!("cyclic hierarchy must not produce an empty simulation"),
        Err(error) => error,
    };
    assert!(matches!(
      error,
      ElabError::UnsupportedSemantic { ref message }
        if message.contains("cyclic module hierarchy")
          && message.contains("first")
          && message.contains("second")
    ));
}

#[test]
fn conformance_mode_rejects_port_width_conversion() {
    let src = "module child(input logic [7:0] value); endmodule\n\
      module top; logic [3:0] value; child dut(value); endmodule\n";
    let file = parse_source_conformant(src).expect("width mismatch syntax parses");
    let eevee_ast::Item::Module(child) = &file.items[0] else {
        panic!("expected child module");
    };
    let eevee_ast::Item::Module(top) = &file.items[1] else {
        panic!("expected top module");
    };
    assert_eq!(child.ports[0].width, 8);
    assert!(matches!(&top.items[0], eevee_ast::ModuleItem::Var(var) if var.width == 4));
    assert!(matches!(
      &top.items[1],
      eevee_ast::ModuleItem::Instance(instance) if instance.connections.len() == 1
    ));
    let error = match elaborate_conformant(&file, &Interp) {
        Ok(_) => panic!("unsupported port width conversion must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(
      error,
      ElabError::UnsupportedSemantic { ref message }
        if message.contains("port width conversion")
          && message.contains("4 bits")
          && message.contains("8 bits")
    ));
}
