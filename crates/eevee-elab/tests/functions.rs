//! Function call tests: parameters, return values, nested calls, the
//! `funcname = expr` return form, and **recursion** (which exercises the
//! interpreter's call stack — the machinery class methods will reuse).

use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;

fn run_get(src: &str, net: &str) -> u64 {
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);
    sim.kernel().set_echo(false);
    let n = sim
        .kernel()
        .find_net(net)
        .unwrap_or_else(|| panic!("missing net {net}"));
    sim.run();
    sim.kernel().net_value(n).to_u64()
}

#[test]
fn simple_function_return() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      function int add(int a, int b);\n\
        return a + b;\n\
      endfunction\n\
      initial begin r = add(3, 4); end\n\
    endmodule\n";
    assert_eq!(run_get(src, "r"), 7);
}

#[test]
fn nested_function_calls() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      function int add(int a, int b);\n\
        return a + b;\n\
      endfunction\n\
      function int double_add(int x);\n\
        return add(x, x);\n\
      endfunction\n\
      initial begin r = double_add(10); end\n\
    endmodule\n";
    assert_eq!(run_get(src, "r"), 20);
}

#[test]
fn recursive_factorial() {
    // fact(5) = 120 — proves the call stack handles recursion (each call is a
    // fresh frame; the shared function table is indexed, not Rc-cyclic).
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      function int fact(int n);\n\
        if (n == 0) fact = 1;\n\
        else fact = n * fact(n - 1);\n\
      endfunction\n\
      initial begin r = fact(5); end\n\
    endmodule\n";
    assert_eq!(run_get(src, "r"), 120);
}

#[test]
fn funcname_return_form() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      function int sq(int x);\n\
        sq = x * x;\n\
      endfunction\n\
      initial begin r = sq(9); end\n\
    endmodule\n";
    assert_eq!(run_get(src, "r"), 81);
}
