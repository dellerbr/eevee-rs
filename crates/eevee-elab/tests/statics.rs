//! Static class field tests: a `static` field is shared storage across all
//! instances of a class (and its subclasses), exercising `StaticGet`/
//! `StaticSet` and `new` into a static handle.

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
fn static_field_shared_across_instances() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class svc;\n\
        static int count;\n\
        function void bump();\n\
          count = count + 1;\n\
        endfunction\n\
        function int get_count();\n\
          return count;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        svc a;\n\
        svc b;\n\
        a = new();\n\
        b = new();\n\
        a.bump();\n\
        b.bump();\n\
        a.bump();\n\
        r = b.get_count();\n\
      end\n\
    endmodule\n";
    // 3 bumps across a and b, read via b's view of the shared static -> 3.
    assert_eq!(net(&run(src), "r"), 3);
}

#[test]
fn static_singleton_via_static_method() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class svc;\n\
        static int hits;\n\
        static svc inst;\n\
        function void touch();\n\
          hits = hits + 1;\n\
        endfunction\n\
        function int get_hits();\n\
          return hits;\n\
        endfunction\n\
        static function svc get();\n\
          inst = new();\n\
          return inst;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        svc x;\n\
        x = svc::get();\n\
        x.touch();\n\
        x.touch();\n\
        r = x.get_hits();\n\
      end\n\
    endmodule\n";
    // get() builds the singleton; two touches on the shared static `hits` -> 2.
    assert_eq!(net(&run(src), "r"), 2);
}
