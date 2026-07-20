//! Procedural-breadth end-to-end tests: `initial`, `begin/end`, local
//! variables (registers), `if/else`, sized literals, and `$display`, all from
//! real SystemVerilog source.

use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;

fn run(src: &str) -> eevee_sched::Sim {
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);
    sim.kernel().set_echo(false);
    sim.run();
    sim
}

#[test]
fn if_then_with_locals_and_display() {
    let src = "module top;\n\
      logic [7:0] r = 0;\n\
      initial begin\n\
        int x;\n\
        x = 5;\n\
        if (x == 5) begin\n\
          r = 8'hAA;\n\
          $display(\"x is %0d\", x);\n\
        end else\n\
          r = 8'h55;\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    let r = sim.kernel().find_net("r").expect("net r");
    assert_eq!(
        sim.kernel().net_value(r).to_u64(),
        0xAA,
        "then-branch taken"
    );
    assert_eq!(sim.kernel().output(), ["x is 5"]);
}

#[test]
fn if_else_branch_taken() {
    let src = "module top;\n\
      logic [7:0] r = 0;\n\
      initial begin\n\
        int x;\n\
        x = 4;\n\
        if (x == 5) r = 8'hAA;\n\
        else r = 8'h55;\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    let r = sim.kernel().find_net("r").expect("net r");
    assert_eq!(
        sim.kernel().net_value(r).to_u64(),
        0x55,
        "else-branch taken"
    );
}

#[test]
fn sized_literals_and_local_arithmetic() {
    let src = "module top;\n\
      logic [7:0] r = 0;\n\
      initial begin\n\
        byte a;\n\
        byte b;\n\
        a = 8'h0F;\n\
        b = 8'hF0;\n\
        r = a | b;\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    let r = sim.kernel().find_net("r").expect("net r");
    assert_eq!(sim.kernel().net_value(r).to_u64(), 0xFF);
}

#[test]
fn packed_bit_part_select_and_concatenation() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      initial begin\n\
        bit [31:0] value;\n\
        value = 32'h12345678;\n\
        value[0] = 1;\n\
        value[31] = 1;\n\
        r = {value[7:0], value[15:8], value[23:16], value[31:24]};\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    let r = sim.kernel().find_net("r").expect("net r");
    assert_eq!(sim.kernel().net_value(r).to_u64(), 0x7956_3492);
}

#[test]
fn expression_event_observes_procedural_nba_update() {
    let src = "module top;\n\
      initial begin\n\
        static int value;\n\
        int next = 42;\n\
        value <= next;\n\
        @(value);\n\
        $display(\"value=%0d\", value);\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    assert_eq!(sim.kernel().output(), ["value=42"]);
}

#[test]
fn display_formats_hex_and_string() {
    let src = "module top;\n\
      initial begin\n\
        int v;\n\
        v = 255;\n\
        $display(\"val=%0d hex=%h %s\", v, v, \"end\");\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    assert_eq!(sim.kernel().output(), ["val=255 hex=ff end"]);
}

#[test]
fn break_and_continue_control_forever_loop() {
    let src = "module top;\n\
      initial begin\n\
        int i = 0;\n\
        int total = 0;\n\
        forever begin\n\
          i++;\n\
          if (i == 2) continue;\n\
          total += i;\n\
          if (i == 3) break;\n\
        end\n\
        $display(\"i=%0d total=%0d\", i, total);\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    assert_eq!(sim.kernel().output(), ["i=3 total=4"]);
}

#[test]
fn do_while_runs_body_before_test_and_continue_checks_condition() {
    let src = "module top;\n\
      initial begin\n\
        int i = 0;\n\
        int total = 0;\n\
        do begin\n\
          i++;\n\
          if (i == 2) continue;\n\
          total += i;\n\
        end while (i < 3);\n\
        $display(\"i=%0d total=%0d\", i, total);\n\
      end\n\
    endmodule\n";
    let mut sim = run(src);
    assert_eq!(sim.kernel().output(), ["i=3 total=4"]);
}
