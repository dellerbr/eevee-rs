//! End-to-end: SystemVerilog **source text** → Verible parse → AST → elaborate
//! → IR → run on the event-driven kernel. This is the P2 milestone: a real
//! `.sv` design (not hand-built IR) producing correct behavior.

use eevee_core::{Bit, LogicVec, SimTime};
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
fn resolved_net_ports_collapse_with_matching_parent_nets() {
    let src = "module leaf(input triand sensed, output wire seen);\n\
      assign seen = sensed;\n\
    endmodule\n\
    module child(input wand sensed, output triand all,\n\
                inout wor any, output tri0 pulled,\n\
                output tri ordinary, output wire seen);\n\
      assign all = 1'b1;\n\
      assign all = 1'b0;\n\
      assign any = 1'b0;\n\
      assign pulled = 1'bz;\n\
      assign ordinary = 1'b1;\n\
      leaf observer(.sensed(sensed), .seen(seen));\n\
    endmodule\n\
    module top(output tri1 root_pull, inout tri0 root_inout);\n\
      logic left = 1;\n\
      logic right = 0;\n\
      wand sensed;\n\
      wand all;\n\
      wor any;\n\
      tri0 pulled;\n\
      wire ordinary;\n\
      wire seen;\n\
      wire seen_second;\n\
      assign sensed = left;\n\
      assign sensed = right;\n\
      assign any = 1'b1;\n\
      assign root_pull = 1'bz;\n\
      assign root_inout = 1'bz;\n\
      child dut(.sensed(sensed), .all(all), .any(any),\n\
          .pulled(pulled), .ordinary(ordinary), .seen(seen));\n\
      child dut_second(.sensed(sensed), .all(all), .any(any),\n\
          .pulled(pulled), .ordinary(ordinary), .seen(seen_second));\n\
      initial #1 right = 1;\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let all = sim.kernel().find_net("all").expect("wand output");
    let any = sim.kernel().find_net("any").expect("wor inout");
    let pulled = sim.kernel().find_net("pulled").expect("tri0 output");
    let ordinary = sim.kernel().find_net("ordinary").expect("tri/wire output");
    let seen = sim.kernel().find_net("seen").expect("wand input observer");
    let seen_second = sim
        .kernel()
        .find_net("seen_second")
        .expect("second resolved input observer");
    let root_pull = sim
        .kernel()
        .find_net("root_pull")
        .expect("unbound root tri1 port");
    let root_inout = sim
        .kernel()
        .find_net("root_inout")
        .expect("unbound root tri0 inout port");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(all).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(pulled).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(ordinary).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(seen).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(seen_second).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(root_pull).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(root_inout).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(seen).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(seen_second).get_bit(0), Bit::One);
}

#[test]
fn cross_resolution_input_and_output_ports_bridge_resolved_values() {
    let src = "module child(input wire variable_in, input wand resolved_in,\n\
                            output wor any, output wire plain,\n\
                            output wire variable_seen, output wire resolved_seen);\n\
      assign any = 1'b0;\n\
      assign any = resolved_in;\n\
      assign plain = resolved_in;\n\
      assign variable_seen = variable_in;\n\
      assign resolved_seen = resolved_in;\n\
    endmodule\n\
    module top;\n\
      logic variable_source = 1;\n\
      logic left = 0;\n\
      logic right = 0;\n\
      wor resolved_source;\n\
      wire any;\n\
      wand plain;\n\
      wire variable_seen;\n\
      wire resolved_seen;\n\
      assign resolved_source = left;\n\
      assign resolved_source = right;\n\
      assign any = 1'b0;\n\
      assign plain = 1'b1;\n\
      child dut(.variable_in(variable_source), .resolved_in(resolved_source),\n\
                .any(any), .plain(plain),\n\
                .variable_seen(variable_seen), .resolved_seen(resolved_seen));\n\
      initial begin\n\
        #1 variable_source = 0;\n\
        right = 1;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let any = sim.kernel().find_net("any").expect("ordinary parent net");
    let plain = sim.kernel().find_net("plain").expect("wand parent net");
    let variable_seen = sim
        .kernel()
        .find_net("variable_seen")
        .expect("variable input observer");
    let resolved_seen = sim
        .kernel()
        .find_net("resolved_seen")
        .expect("resolved input observer");
    let child_variable = sim
        .kernel()
        .find_net("dut.variable_in")
        .expect("local bridged wire input");
    let child_resolved = sim
        .kernel()
        .find_net("dut.resolved_in")
        .expect("local bridged wand input");
    let child_any = sim
        .kernel()
        .find_net("dut.any")
        .expect("local bridged wor output");
    let child_plain = sim
        .kernel()
        .find_net("dut.plain")
        .expect("local bridged wire output");
    let variable_source = sim
        .kernel()
        .find_net("variable_source")
        .expect("parent variable");
    let resolved_source = sim
        .kernel()
        .find_net("resolved_source")
        .expect("parent wor net");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(plain).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(variable_source).get_bit(0), Bit::One);
    assert_eq!(
        sim.kernel().net_value(resolved_source).get_bit(0),
        Bit::Zero
    );
    assert_eq!(sim.kernel().net_value(child_variable).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(child_resolved).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(child_any).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(child_plain).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(variable_seen).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(resolved_seen).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::X);
    assert_eq!(sim.kernel().net_value(plain).get_bit(0), Bit::One);
    assert_eq!(
        sim.kernel().net_value(variable_source).get_bit(0),
        Bit::Zero
    );
    assert_eq!(sim.kernel().net_value(resolved_source).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(child_variable).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(child_resolved).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(child_any).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(child_plain).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(variable_seen).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(resolved_seen).get_bit(0), Bit::One);
}

#[test]
fn conformance_mode_rejects_resolved_port_mismatches() {
    let cases = [
        (
            "module top(output wand value);\n\
           assign (strong1, pull0) value = 1'b1; endmodule",
            "explicit drive strengths on wired net",
        ),
        (
            "module child(output tri0 value); assign value = 1'bz; endmodule\n\
         module top; wire value; child dut(.value(value)); endmodule",
            "unsupported port resolution bridge",
        ),
        (
            "module child(input tri1 value); endmodule\n\
       module top; logic value; child dut(.value(value)); endmodule",
            "unsupported port resolution bridge",
        ),
        (
            "module child(input wire value); endmodule\n\
         module top; tri1 value; child dut(.value(value)); endmodule",
            "unsupported port resolution bridge",
        ),
        (
            "module child(inout wand value); endmodule\n\
       module top; wor value; child dut(.value(value)); endmodule",
            "cross-resolution inout port",
        ),
    ];
    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("resolved port syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("incompatible resolved port must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
}

#[test]
fn continuous_assignments_propagate_and_resolve_drivers() {
    let src = "module child(input logic [7:0] source, output wire [7:0] result);\n\
      assign result = source + 8'd1;\n\
    endmodule\n\
    module top;\n\
      logic [7:0] source = 1;\n\
      logic left = 0;\n\
      logic right = 1;\n\
      wire [7:0] result;\n\
      wire [3:0] selected;\n\
      wire resolved;\n\
      child dut(.source(source), .result(result));\n\
      assign selected = {source[0], source[3:1]};\n\
      assign resolved = left;\n\
      assign resolved = right;\n\
      initial begin\n\
        #1 right = 0;\n\
        #1 source = 41;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);

    let result = sim.kernel().find_net("result").expect("result net");
    let selected = sim.kernel().find_net("selected").expect("selected net");
    let resolved = sim.kernel().find_net("resolved").expect("resolved net");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 2);
    assert_eq!(sim.kernel().net_value(selected).to_u64(), 8);
    assert_eq!(sim.kernel().net_value(resolved).get_bit(0), Bit::X);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(resolved).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 42);
    assert_eq!(sim.kernel().net_value(selected).to_u64(), 12);
}

#[test]
fn conditional_continuous_drivers_release_to_high_impedance() {
    let src = "module top;\n\
      logic enable = 0;\n\
      logic other_enable = 1;\n\
      logic delayed_enable = 0;\n\
      logic [3:0] data = 4'ha;\n\
      logic [3:0] other = 4'h5;\n\
      wire [3:0] bus;\n\
      wire [3:0] always_released;\n\
      wire [3:0] masked_fill;\n\
      wire delayed;\n\
      tri1 fallback;\n\
      assign bus = enable ? data : 'z;\n\
      assign bus = other_enable ? other : 'z;\n\
      assign always_released = 'z;\n\
      assign masked_fill = '1 & 4'ha;\n\
      assign #2 delayed = delayed_enable ? 1'b1 : 'z;\n\
      assign fallback = enable ? 1'b0 : 'z;\n\
      initial begin\n\
        #1 enable = 1;\n\
        delayed_enable = 1;\n\
        #1 other_enable = 0;\n\
        delayed_enable = 0;\n\
        #1 enable = 1'bx;\n\
        #1 enable = 0;\n\
        delayed_enable = 1;\n\
        #3 delayed_enable = 0;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let bus = sim.kernel().find_net("bus").expect("tri-state bus");
    let always_released = sim
        .kernel()
        .find_net("always_released")
        .expect("directly released driver");
    let delayed = sim.kernel().find_net("delayed").expect("delayed release");
    let masked_fill = sim
        .kernel()
        .find_net("masked_fill")
        .expect("nested fill expression");
    let fallback = sim.kernel().find_net("fallback").expect("pulled fallback");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(bus).to_u64(), 5);
    assert!((0..4).all(|bit| sim.kernel().net_value(always_released).get_bit(bit) == Bit::Z));
    assert_eq!(sim.kernel().net_value(delayed).get_bit(0), Bit::Z);
    assert_eq!(sim.kernel().net_value(masked_fill).to_u64(), 10);
    assert_eq!(sim.kernel().net_value(fallback).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert!((0..4).all(|bit| sim.kernel().net_value(bus).get_bit(bit) == Bit::X));
    assert_eq!(sim.kernel().net_value(fallback).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(bus).to_u64(), 10);

    sim.run_until(Some(SimTime::from_fs(3_000_000)));
    assert!((0..4).all(|bit| sim.kernel().net_value(bus).get_bit(bit) == Bit::X));
    assert_eq!(sim.kernel().net_value(fallback).get_bit(0), Bit::X);
    assert_eq!(sim.kernel().net_value(delayed).get_bit(0), Bit::Z);

    sim.run_until(Some(SimTime::from_fs(4_000_000)));
    assert!((0..4).all(|bit| sim.kernel().net_value(bus).get_bit(bit) == Bit::Z));
    assert_eq!(sim.kernel().net_value(fallback).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(6_000_000)));
    assert_eq!(sim.kernel().net_value(delayed).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(8_000_000)));
    assert_eq!(sim.kernel().net_value(delayed).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(9_000_000)));
    assert_eq!(sim.kernel().net_value(delayed).get_bit(0), Bit::Z);
}

#[test]
fn conditional_expression_width_and_branch_execution_are_stable() {
    let src = "module top;\n\
      logic condition = 1;\n\
      logic [3:0] width_result = 0;\n\
      logic [3:0] fill_result = 0;\n\
      logic [31:0] lazy_result = 0;\n\
      function int mark(input int value);\n\
        $display(\"MARK %0d\", value);\n\
        mark = value;\n\
      endfunction\n\
      initial begin\n\
        width_result = condition ? 1'b1 : 4'ha;\n\
        fill_result = '1;\n\
        lazy_result = condition ? mark(1) : mark(2);\n\
        #1 condition = 0;\n\
        width_result = condition ? 1'b1 : 4'ha;\n\
        fill_result = 'x;\n\
        lazy_result = condition ? mark(1) : mark(2);\n\
        #1 condition = 1'bx;\n\
        width_result = condition ? 1'b1 : 4'ha;\n\
        fill_result = '1 & 4'ha;\n\
        lazy_result = condition ? mark(1) : mark(2);\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let width_result = sim
        .kernel()
        .find_net("width_result")
        .expect("conditional result");
    let fill_result = sim.kernel().find_net("fill_result").expect("fill result");
    let lazy_result = sim.kernel().find_net("lazy_result").expect("lazy result");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(width_result).to_u64(), 1);
    assert_eq!(sim.kernel().net_value(fill_result).to_u64(), 15);
    assert_eq!(sim.kernel().net_value(lazy_result).width(), 32);
    assert_eq!(sim.kernel().net_value(lazy_result).to_u64(), 1);
    assert_eq!(sim.kernel().output(), &["MARK 1"]);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(width_result).to_u64(), 10);
    assert!((0..4).all(|bit| sim.kernel().net_value(fill_result).get_bit(bit) == Bit::X));
    assert_eq!(sim.kernel().net_value(lazy_result).width(), 32);
    assert_eq!(sim.kernel().net_value(lazy_result).to_u64(), 2);
    assert_eq!(sim.kernel().output(), &["MARK 1", "MARK 2"]);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    let result = sim.kernel().net_value(width_result);
    assert_eq!(result.width(), 4);
    assert_eq!(result.get_bit(0), Bit::X);
    assert_eq!(result.get_bit(1), Bit::X);
    assert_eq!(result.get_bit(2), Bit::Zero);
    assert_eq!(result.get_bit(3), Bit::X);
    assert_eq!(sim.kernel().net_value(fill_result).to_u64(), 10);
    let lazy = sim.kernel().net_value(lazy_result);
    assert_eq!(lazy.width(), 32);
    assert_eq!(lazy.get_bit(0), Bit::X);
    assert_eq!(lazy.get_bit(1), Bit::X);
    assert!((2..32).all(|bit| lazy.get_bit(bit) == Bit::Zero));
    assert_eq!(
        sim.kernel().output(),
        &["MARK 1", "MARK 2", "MARK 1", "MARK 2"]
    );
}

#[test]
fn conformance_mode_rejects_nonintegral_conditionals() {
    let src = "module top;\n\
      logic condition;\n\
      string result;\n\
      initial result = condition ? \"yes\" : \"no\";\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conditional syntax parses");
    let error = match elaborate_conformant(&file, &Interp) {
        Ok(_) => panic!("non-integral conditional must fail closed"),
        Err(error) => error,
    };
    assert!(matches!(
      error,
      ElabError::UnsupportedSemantic { ref message }
        if message.contains("non-integral conditional expressions")
    ));
}

#[test]
fn wired_and_or_net_types_resolve_continuous_drivers() {
    let src = "module top;\n\
      logic left = 1;\n\
      logic right = 1;\n\
      wand all;\n\
      wor any;\n\
      assign all = left;\n\
      assign all = right;\n\
      assign any = left;\n\
      assign any = right;\n\
      initial begin\n\
        #1 right = 0;\n\
        #1 left = 0;\n\
        #1 left = 1'bx;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let all = sim.kernel().find_net("all").expect("wand net");
    let any = sim.kernel().find_net("any").expect("wor net");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(all).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(all).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(all).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(3_000_000)));
    assert_eq!(sim.kernel().net_value(all).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(any).get_bit(0), Bit::X);
}

#[test]
fn implicit_pull_and_supply_net_drivers_resolve_by_strength() {
    let src = "module top;\n\
      logic low = 1'bz;\n\
      logic high = 1'bz;\n\
      tri0 pulled_low;\n\
      tri1 pulled_high;\n\
      tri0 [3:0] pulled_bus;\n\
      supply0 supply_low;\n\
      supply1 supply_high;\n\
      assign pulled_low = high;\n\
      assign pulled_high = low;\n\
      assign supply_low = high;\n\
      assign supply_high = low;\n\
      initial begin\n\
        #1 low = 0;\n\
        high = 1;\n\
        #1 low = 1'bz;\n\
        high = 1'bz;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let pulled_low = sim.kernel().find_net("pulled_low").expect("tri0 net");
    let pulled_high = sim.kernel().find_net("pulled_high").expect("tri1 net");
    let pulled_bus = sim.kernel().find_net("pulled_bus").expect("tri0 bus");
    let supply_low = sim.kernel().find_net("supply_low").expect("supply0 net");
    let supply_high = sim.kernel().find_net("supply_high").expect("supply1 net");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(pulled_low).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(pulled_high).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(pulled_bus), &LogicVec::zero(4));
    assert_eq!(sim.kernel().net_value(supply_low).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(supply_high).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(pulled_low).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(pulled_high).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(supply_low).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(supply_high).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(pulled_low).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(pulled_high).get_bit(0), Bit::One);
}

#[test]
fn explicit_drive_strengths_resolve_asymmetric_and_highz_values() {
    let src = "module top;\n\
      logic asymmetric_source = 1;\n\
      logic pull_source = 0;\n\
      logic open_source = 0;\n\
      logic strong_low = 0;\n\
      logic [1:0] vector_source = 2'b10;\n\
      logic [1:0] vector_pull = 2'b01;\n\
      wire asymmetric_result;\n\
      wire open_result;\n\
      wire supply_result;\n\
      wire [1:0] vector_result;\n\
      assign (strong1, pull0) #0 asymmetric_result = asymmetric_source;\n\
      assign (pull1, pull0) asymmetric_result = pull_source;\n\
      assign (strong1, highz0) open_result = open_source;\n\
      assign (supply1, supply0) supply_result = 1'b1;\n\
      assign (strong1, strong0) supply_result = strong_low;\n\
      assign (strong1, pull0) vector_result = vector_source;\n\
      assign (pull1, pull0) vector_result = vector_pull;\n\
      initial begin\n\
      #1 asymmetric_source = 0;\n\
      pull_source = 1;\n\
      open_source = 1;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let asymmetric = sim
        .kernel()
        .find_net("asymmetric_result")
        .expect("asymmetric result");
    let open = sim.kernel().find_net("open_result").expect("open result");
    let supply = sim
        .kernel()
        .find_net("supply_result")
        .expect("supply result");
    let vector = sim
        .kernel()
        .find_net("vector_result")
        .expect("vector result");

    sim.run_until(Some(SimTime::ZERO));
    assert_eq!(sim.kernel().net_value(asymmetric).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(open).get_bit(0), Bit::Z);
    assert_eq!(sim.kernel().net_value(supply).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(vector).get_bit(1), Bit::One);
    assert_eq!(sim.kernel().net_value(vector).get_bit(0), Bit::X);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(sim.kernel().net_value(asymmetric).get_bit(0), Bit::X);
    assert_eq!(sim.kernel().net_value(open).get_bit(0), Bit::One);
}

#[test]
fn continuous_assignment_uses_rhs_source_sensitivity() {
    let src = "module top;\n\
      logic source = 0;\n\
      logic unrelated = 0;\n\
      wire result;\n\
      assign result = source;\n\
      initial begin\n\
        #1 unrelated = 1;\n\
        #1 source = 1;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);

    let result = sim.kernel().find_net("result").expect("result net");
    sim.run_until(Some(SimTime::ZERO));
    let initial_resumes = sim.kernel_ref().stats().resumes;
    assert_eq!(sim.kernel_ref().net_value(result).to_u64(), 0);

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    assert_eq!(
        sim.kernel_ref().stats().resumes,
        initial_resumes + 1,
        "unrelated write must not resume continuous assignment"
    );

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel_ref().net_value(result).to_u64(), 1);
    assert_eq!(sim.kernel_ref().stats().resumes, initial_resumes + 3);
}

#[test]
fn delayed_continuous_assignment_rejects_short_pulses() {
    let src = "module child #(parameter int DELAY = 4)\n\
                         (input logic source, output wire result,\n\
                          output wire immediate);\n\
      assign #(DELAY + 1) result = source;\n\
      assign #0 immediate = source;\n\
    endmodule\n\
    module top;\n\
      logic source = 0;\n\
      wire result;\n\
      wire immediate;\n\
      child #(.DELAY(2)) dut(.source(source), .result(result),\n\
                              .immediate(immediate));\n\
      initial begin\n\
        #1 source = 1;\n\
        #1 source = 0;\n\
        #4 source = 1;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let result = sim.kernel().find_net("result").expect("result net");
    let immediate = sim.kernel().find_net("immediate").expect("immediate net");

    sim.run_until(Some(SimTime::from_fs(4_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Z);
    assert_eq!(sim.kernel().net_value(immediate).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(5_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(8_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Zero);
    assert_eq!(sim.kernel().net_value(immediate).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(9_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::One);
}

#[test]
fn transition_specific_continuous_assignment_delays_apply_per_bit() {
    let src = "module top #(parameter int RISE = 2);\n\
      logic [2:0] source = 3'b110;\n\
      logic off_source = 0;\n\
      wire [2:0] result;\n\
      wire two_delay;\n\
      assign #(RISE, RISE + 2, RISE + 4) result = source;\n\
      assign #(3, 5) two_delay = off_source;\n\
      initial begin\n\
        #10 source = 3'bz01;\n\
        off_source = 1;\n\
        #10 off_source = 1'bz;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let result = sim.kernel().find_net("result").expect("result net");
    let two_delay = sim.kernel().find_net("two_delay").expect("two-delay net");

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Z);
    assert_eq!(sim.kernel().net_value(result).get_bit(1), Bit::One);
    assert_eq!(sim.kernel().net_value(result).get_bit(2), Bit::One);

    sim.run_until(Some(SimTime::from_fs(5_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 6);
    assert_eq!(sim.kernel().net_value(two_delay).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(12_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 7);

    sim.run_until(Some(SimTime::from_fs(14_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 5);

    sim.run_until(Some(SimTime::from_fs(16_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(result).get_bit(1), Bit::Zero);
    assert_eq!(sim.kernel().net_value(result).get_bit(2), Bit::Z);

    sim.run_until(Some(SimTime::from_fs(22_000_000)));
    assert_eq!(sim.kernel().net_value(two_delay).get_bit(0), Bit::One);

    sim.run_until(Some(SimTime::from_fs(23_000_000)));
    assert_eq!(sim.kernel().net_value(two_delay).get_bit(0), Bit::Z);
}

#[test]
fn net_declaration_delays_apply_to_resolved_values() {
    let src = "module top #(parameter int RISE = 2);\n\
      logic [2:0] source = 3'b110;\n\
      wire [2:0] #(RISE, RISE + 2, RISE + 4) result;\n\
      assign result = source;\n\
      initial #10 source = 3'bz01;\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let result = sim.kernel().find_net("result").expect("delayed net");

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Z);
    assert_eq!(sim.kernel().net_value(result).get_bit(1), Bit::One);
    assert_eq!(sim.kernel().net_value(result).get_bit(2), Bit::One);

    sim.run_until(Some(SimTime::from_fs(5_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 6);

    sim.run_until(Some(SimTime::from_fs(12_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 7);

    sim.run_until(Some(SimTime::from_fs(14_000_000)));
    assert_eq!(sim.kernel().net_value(result).to_u64(), 5);

    sim.run_until(Some(SimTime::from_fs(16_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::One);
    assert_eq!(sim.kernel().net_value(result).get_bit(1), Bit::Zero);
    assert_eq!(sim.kernel().net_value(result).get_bit(2), Bit::Z);
}

#[test]
fn continuous_and_net_declaration_delays_compose() {
    let src = "module top;\n\
      logic source = 0;\n\
      wire #3 result;\n\
      assign (strong1, pull0) #2 result = source;\n\
      initial #10 source = 1;\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    let result = sim.kernel().find_net("result").expect("delayed net");

    sim.run_until(Some(SimTime::from_fs(4_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Z);

    sim.run_until(Some(SimTime::from_fs(5_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(14_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(15_000_000)));
    assert_eq!(sim.kernel().net_value(result).get_bit(0), Bit::One);
}

#[test]
fn conformance_mode_rejects_invalid_net_declaration_delays() {
    let cases = [
        (
            "module top; logic source; wire #source result; endmodule",
            "net declaration delay for 'top.result' is not a constant parameter expression",
        ),
        (
            "module top; wire #(1, -2) result; endmodule",
            "delay is negative",
        ),
        (
            "module top; wire #(1, 2, 1'bx) result; endmodule",
            "delay contains X or Z",
        ),
    ];
    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("net delay syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("invalid net declaration delay must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
}

#[test]
fn conformance_mode_rejects_invalid_continuous_drivers() {
    let cases = [
        (
            "module top; logic source, result; assign result = source; endmodule",
            "not a whole net",
        ),
        (
            "module top; wire result; initial result = 1; endmodule",
            "procedural assignment to net",
        ),
        (
            "module top; wire result; assign result = missing; endmodule",
            "unsupported continuous assignment expression",
        ),
        (
            "module top; logic signed source; wire [7:0] result; assign result = source; endmodule",
            "unsupported continuous assignment expression",
        ),
        (
            "module top; logic source; wire [7:0] result; assign result = source; endmodule",
            "width conversion is unsupported",
        ),
        (
          "module top; logic enable; logic [1:0] source; wire [1:0] result;\n\
           assign result = enable ? source : 1'bz; endmodule",
          "continuous conditional branch width mismatch",
        ),
        (
            "module top; logic source; wire result; assign #(-1) result = source; endmodule",
            "delay is negative",
        ),
        (
            "module top; logic source; wire result; assign #(1'bx) result = source; endmodule",
            "delay contains X or Z",
        ),
        (
            "module top; logic source; wire result; assign #source result = source; endmodule",
            "delay in module 'top' is not a constant parameter expression",
        ),
        (
          "module top; logic source; wand result; assign (strong1, pull0) result = source; endmodule",
          "explicit drive strengths on wired net 'top.result' are unsupported",
        ),
        (
          "module top; logic source; wire result; assign #(1, source) result = source; endmodule",
          "delay in module 'top' is not a constant parameter expression",
        ),
        (
          "module top; logic source; wire result; assign #(1, -2) result = source; endmodule",
          "delay is negative",
        ),
        (
          "module top; logic source; wire result; assign #(1, 2, 1'bz) result = source; endmodule",
          "delay contains X or Z",
        ),
    ];
    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("continuous syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("invalid continuous driver must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
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
fn module_parameters_control_per_instance_packed_widths() {
    let src = "module child #(parameter int WIDTH = 4)\n\
                 (input logic [WIDTH-1:0] value,\n\
                  output wire [WIDTH-1:0] result);\n\
      logic [WIDTH-1:0] storage;\n\
      wire [WIDTH-1:0] inverted;\n\
      initial storage = value;\n\
      assign inverted = ~storage;\n\
      assign result = inverted;\n\
    endmodule\n\
    module top;\n\
      logic [3:0] narrow_value = 4'h5;\n\
      wire [3:0] narrow_result;\n\
      logic [7:0] wide_value = 8'h3c;\n\
      wire [7:0] wide_result;\n\
      child narrow(.value(narrow_value), .result(narrow_result));\n\
      child #(.WIDTH(8)) wide(.value(wide_value), .result(wide_result));\n\
    endmodule\n";
    let file = parse_source_conformant(src).expect("conformant parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("conformant elaboration");
    sim.kernel().set_echo(false);
    sim.run();

    for (name, width, value) in [
        ("narrow.storage", 4, 0x5),
        ("narrow.inverted", 4, 0xa),
        ("narrow_result", 4, 0xa),
        ("wide.storage", 8, 0x3c),
        ("wide.inverted", 8, 0xc3),
        ("wide_result", 8, 0xc3),
    ] {
        let net = sim.kernel().find_net(name).expect("specialized-width net");
        let result = sim.kernel().net_value(net);
        assert_eq!(result.width(), width, "width of {name}");
        assert_eq!(result.to_u64(), value, "value of {name}");
    }
}

#[test]
fn generate_if_case_and_for_elaborate_selected_scopes() {
    let source = "module leaf #(parameter int VALUE = 0)\n\
           (output logic [3:0] result);\n\
      initial result = VALUE[3:0];\n\
  endmodule\n\
  module top #(parameter int COUNT = 2,\n\
                               parameter int MODE = 2);\n\
      generate\n\
        if (MODE == 2) begin : selected\n\
          logic [3:0] value = 4'ha;\n\
        end else begin : rejected\n\
          logic [3:0] value = 4'hf;\n\
        end\n\
        case (MODE)\n\
          1: begin : case_one logic [3:0] value = 4'h1; end\n\
          2, 3: begin : case_two logic [3:0] value = 4'h2; end\n\
          default: begin : case_default logic [3:0] value = 4'hf; end\n\
        endcase\n\
        for (genvar index = 0; index < COUNT; index++) begin : lane\n\
          logic [3:0] value = index;\n\
          logic [3:0] child_value;\n\
          wire [3:0] driven;\n\
          leaf #(.VALUE(index + 4)) child(.result(child_value));\n\
          assign driven = child_value;\n\
          if (index == 1) begin : special logic marker = 1; end\n\
          else begin : ordinary logic marker = 0; end\n\
        end\n\
      endgenerate\n\
    endmodule\n";
    let file = parse_source_conformant(source).expect("conformant generate parse");
    let mut sim = elaborate_conformant(&file, &Interp).expect("generate elaboration");
    sim.kernel().set_echo(false);
    sim.run();

    for (name, width, value) in [
        ("selected.value", 4, 0xa),
        ("case_two.value", 4, 0x2),
        ("lane[0].value", 4, 0x0),
        ("lane[0].child_value", 4, 0x4),
        ("lane[0].driven", 4, 0x4),
        ("lane[0].ordinary.marker", 1, 0x0),
        ("lane[1].value", 4, 0x1),
        ("lane[1].child_value", 4, 0x5),
        ("lane[1].driven", 4, 0x5),
        ("lane[1].special.marker", 1, 0x1),
    ] {
        let net = sim.kernel().find_net(name).expect("generated net");
        let result = sim.kernel().net_value(net);
        assert_eq!(result.width(), width, "width of {name}");
        assert_eq!(result.to_u64(), value, "value of {name}");
    }
    for absent in [
        "rejected.value",
        "case_one.value",
        "case_default.value",
        "lane[0].special.marker",
        "lane[1].ordinary.marker",
        "lane[2].value",
    ] {
        assert!(
            sim.kernel().find_net(absent).is_none(),
            "unexpected {absent}"
        );
    }
}

#[test]
fn fixed_memories_and_dynamic_selects_follow_scheduler_semantics() {
    let source = "module top #(parameter int WIDTH = 8, DEPTH = 4);\n\
      logic [WIDTH-1:0] memory [0:DEPTH-1];\n\
      logic [WIDTH-1:0] reverse [DEPTH-1:0];\n\
      logic [2:0] address = 0;\n\
      logic [2:0] bit_index = 3;\n\
      logic [3:0] bad_index = 9;\n\
      logic [7:0] vector = 0;\n\
      logic bit_value = 0;\n\
      logic invalid_bit = 0;\n\
      logic [WIDTH-1:0] invalid = 0;\n\
      wire [WIDTH-1:0] observed;\n\
      wire observed_bit;\n\
      wire out_of_range_bit;\n\
      assign observed = memory[address];\n\
      assign observed_bit = vector[bit_index];\n\
      assign out_of_range_bit = vector[bad_index];\n\
      initial begin\n\
        memory[0] = 8'h11;\n\
        memory[1] = 8'h22;\n\
        reverse[3] = 8'h33;\n\
        reverse[0] = 8'h00;\n\
        address = 1;\n\
        vector[bit_index] = 1'b1;\n\
        bit_value = vector[bit_index];\n\
        #1;\n\
        memory[2] <= 8'hcc;\n\
        address = 2;\n\
        #1;\n\
        invalid = memory[7];\n\
        invalid_bit = vector[bad_index];\n\
        memory[7] = 8'hff;\n\
        vector[7] <= 1'b1;\n\
      end\n\
    endmodule\n";
    let file = parse_source_conformant(source).expect("fixed memory syntax parses");
    let mut sim = elaborate_conformant(&file, &Interp).expect("fixed memory elaboration");
    sim.kernel().set_echo(false);

    sim.run_until(Some(SimTime::from_fs(0)));
    for (name, value) in [
        ("memory[0]", 0x11),
        ("memory[1]", 0x22),
        ("reverse[3]", 0x33),
        ("reverse[0]", 0x00),
        ("observed", 0x22),
        ("vector", 0x08),
        ("bit_value", 0x01),
        ("observed_bit", 0x01),
    ] {
        let net = sim.kernel().find_net(name).expect("memory/select net");
        assert_eq!(
            sim.kernel().net_value(net).to_u64(),
            value,
            "value of {name}"
        );
    }

    sim.run_until(Some(SimTime::from_fs(1_000_000)));
    let memory_two = sim.kernel().find_net("memory[2]").expect("memory element");
    let observed = sim.kernel().find_net("observed").expect("observed net");
    assert_eq!(sim.kernel().net_value(memory_two).to_u64(), 0xcc);
    assert_eq!(sim.kernel().net_value(observed).to_u64(), 0xcc);

    sim.run_until(Some(SimTime::from_fs(2_000_000)));
    let invalid = sim
        .kernel()
        .find_net("invalid")
        .expect("invalid read result");
    let invalid_bit = sim
        .kernel()
        .find_net("invalid_bit")
        .expect("invalid packed read result");
    let out_of_range_bit = sim
        .kernel()
        .find_net("out_of_range_bit")
        .expect("continuous invalid packed read result");
    let packed = sim.kernel().find_net("vector").expect("packed vector");
    assert!(!sim.kernel().net_value(invalid).is_known());
    assert!(!sim.kernel().net_value(invalid_bit).is_known());
    assert!(!sim.kernel().net_value(out_of_range_bit).is_known());
    assert_eq!(sim.kernel().net_value(memory_two).to_u64(), 0xcc);
    assert_eq!(sim.kernel().net_value(packed).to_u64(), 0x88);
}

#[test]
fn conformance_mode_rejects_unsupported_fixed_memory_forms() {
    let cases = [
        (
            "module top; logic [7:0] dynamic_memory[]; endmodule\n",
            "module collection 'dynamic_memory' is not a fixed unpacked memory",
        ),
        (
            "module top; logic signed [7:0] memory[0:1]; endmodule\n",
            "fixed memory 'memory' must have an unsigned integral element type",
        ),
        (
            "module top; initial begin logic [7:0] local_memory[0:1]; end endmodule\n",
            "procedural fixed unpacked array 'local_memory' is unsupported",
        ),
        (
            "class holder; logic [7:0] memory[0:1]; endclass module top; endmodule\n",
            "fixed unpacked array 'memory' is only supported for module memories",
        ),
        (
            "module top; logic [7:0] memory[-1:1]; endmodule\n",
            "fixed memory bounds must be nonnegative",
        ),
    ];
    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("fixed-array syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("unsupported fixed-array form must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
}

#[test]
fn conformance_mode_rejects_invalid_generate_semantics() {
    let cases = [
      (
        "module top; logic select; if (select) begin : chosen logic value; end endmodule\n",
        "generate if condition is not a constant parameter expression",
      ),
      (
        "module top; if (1) logic value; endmodule\n",
        "unnamed generate blocks are unsupported",
      ),
      (
        "module top; for (index = 0; index < 2; index++) begin : lane logic value; end endmodule\n",
        "generate-for variable 'index' is not a declared genvar",
      ),
      (
        "module top; genvar index, other; for (index = 0; index < 2; other++) begin : lane logic value; end endmodule\n",
        "generate-for step updates 'other' instead of 'index'",
      ),
    ];
    for (source, expected) in cases {
        let file = parse_source_conformant(source).expect("generate syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("invalid generate semantics must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message } if message.contains(expected)
        ));
    }
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
    for src in [
        "module child(input logic [7:0] value); endmodule\n\
         module top; logic [3:0] value; child dut(value); endmodule\n",
        "module child #(parameter int WIDTH = 4)\n\
           (input logic [WIDTH-1:0] value);\n\
         endmodule\n\
         module top; logic [3:0] value; child #(.WIDTH(8)) dut(value); endmodule\n",
    ] {
        let file = parse_source_conformant(src).expect("width mismatch syntax parses");
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
}

#[test]
fn conformance_mode_rejects_nonparameter_packed_widths() {
    for (src, declaration) in [
        (
            "module child(input logic [MISSING-1:0] value); endmodule\n",
            "child.value",
        ),
        (
            "module top #(parameter int WIDTH = 4);\n\
         function logic [WIDTH-1:0] value(); return '0; endfunction\n\
       endmodule\n",
            "value return",
        ),
        (
            "module top #(parameter int WIDTH = 4);\n\
         initial begin logic [WIDTH-1:0] local_value; local_value = '0; end\n\
       endmodule\n",
            "local_value",
        ),
    ] {
        let file = parse_source_conformant(src).expect("symbolic packed range syntax parses");
        let error = match elaborate_conformant(&file, &Interp) {
            Ok(_) => panic!("unsupported symbolic packed width must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(
          error,
          ElabError::UnsupportedSemantic { ref message }
          if message.contains(&format!("packed range for '{declaration}'"))
            && message.contains("constant parameter expression")
        ));
    }
}
