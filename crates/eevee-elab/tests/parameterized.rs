//! Parameterized-class (monomorphization) tests: each `C#(args)` becomes a
//! distinct specialized class with its own fields and static storage.

use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;
use eevee_sched::Sim;

fn run(src: &str) -> Sim {
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);
    sim.kernel().set_echo(false);
    sim.run();
    sim
}

fn net(sim: &Sim, name: &str) -> u64 {
    let n = sim
        .kernel_ref()
        .find_net(name)
        .unwrap_or_else(|| panic!("missing net {name}"));
    sim.kernel_ref().net_value(n).to_u64()
}

#[test]
fn parameterized_box_specialized_and_used() {
    // A `box#(T)` used inside a concrete class: monomorphization specializes
    // the local `box#(int)` and its methods run normally.
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class box #(type T = int);\n\
        T val;\n\
        function void set(T v);\n\
          val = v;\n\
        endfunction\n\
        function T get();\n\
          return val;\n\
        endfunction\n\
      endclass\n\
      class user;\n\
        function int run();\n\
          box #(int) b;\n\
          b = new();\n\
          b.set(7);\n\
          return b.get();\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        user u;\n\
        u = new();\n\
        r = u.run();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 7);
}

#[test]
fn distinct_specializations_have_independent_static_state() {
    // `counter#(1)` and `counter#(2)` are distinct classes -> distinct static
    // `total`, so bumping one does not affect the other.
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class counter #(int ID = 0);\n\
        static int total;\n\
        function void bump();\n\
          total = total + 1;\n\
        endfunction\n\
        function int get();\n\
          return total;\n\
        endfunction\n\
      endclass\n\
      class user;\n\
        function int run();\n\
          counter #(1) a;\n\
          counter #(2) b;\n\
          a = new();\n\
          b = new();\n\
          a.bump();\n\
          a.bump();\n\
          b.bump();\n\
          return (a.get() * 10) + b.get();\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        user u;\n\
        u = new();\n\
        r = u.run();\n\
      end\n\
    endmodule\n";
    // a bumped twice -> 2, b bumped once -> 1 (independent statics) -> 21.
    assert_eq!(net(&run(src), "r"), 21);
}
