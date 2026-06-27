//! Reproduction of UVM's constructor pattern: a base class with an `extern`
//! constructor (body defined out of body) and a derived class whose `new`
//! chains `super.new(name)`.

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

fn out(sim: &Sim) -> Vec<String> {
    sim.kernel_ref().output().to_vec()
}

#[test]
fn extern_ctor_super_new_propagates_name() {
    let src = "package p;\n\
      class base;\n\
        string m_name;\n\
        extern function new(string name=\"\");\n\
        function string get_name();\n\
          return m_name;\n\
        endfunction\n\
      endclass\n\
      function base::new(string name=\"\");\n\
        m_name = name;\n\
      endfunction\n\
      class derived extends base;\n\
        function new(string name=\"d\");\n\
          super.new(name);\n\
        endfunction\n\
      endclass\n\
    endpackage\n\
    module top;\n\
      initial begin\n\
        derived o;\n\
        o = new(\"hello\");\n\
        $display(\"name=%s\", o.get_name());\n\
      end\n\
    endmodule\n";
    let lines = out(&run(src));
    assert_eq!(lines, vec!["name=hello".to_string()]);
}
