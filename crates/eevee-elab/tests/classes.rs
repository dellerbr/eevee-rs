//! Class tests: fields, constructor (`new`), methods (static dispatch via the
//! call stack with an implicit `this`), field access, method arguments, and
//! object aliasing through shared handles.

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
fn counter_class_new_methods_fields() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Counter;\n\
        int count;\n\
        function new();\n\
          count = 0;\n\
        endfunction\n\
        function void incr();\n\
          count = count + 1;\n\
        endfunction\n\
        function int get();\n\
          return count;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Counter c;\n\
        c = new();\n\
        c.incr();\n\
        c.incr();\n\
        r = c.get();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 2);
}

#[test]
fn class_method_args_and_display() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Acc;\n\
        int sum;\n\
        function new();\n\
          sum = 0;\n\
        endfunction\n\
        function void add(int x);\n\
          sum = sum + x;\n\
        endfunction\n\
        function int total();\n\
          return sum;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Acc a;\n\
        a = new();\n\
        a.add(10);\n\
        a.add(32);\n\
        r = a.total();\n\
        $display(\"sum=%0d\", a.total());\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(net(&sim, "r"), 42);
    assert_eq!(sim.kernel_ref().output(), ["sum=42"]);
}

#[test]
fn object_handles_alias_same_instance() {
    // b = a copies the handle, not the object, so mutating through b is visible
    // through a.
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Box;\n\
        int v;\n\
        function new();\n\
          v = 0;\n\
        endfunction\n\
        function void set(int x);\n\
          v = x;\n\
        endfunction\n\
        function int get();\n\
          return v;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Box a;\n\
        Box b;\n\
        a = new();\n\
        b = a;\n\
        b.set(99);\n\
        r = a.get();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 99);
}

#[test]
fn inheritance_super_and_virtual_dispatch() {
    // d.val() dispatches virtually to Derived::val even through a Base handle;
    // Derived::new calls super.new (Base sets x=10), then x += 5 -> 15;
    // Derived::val returns x + 100 = 115.
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Base;\n\
        int x;\n\
        function new();\n\
          x = 10;\n\
        endfunction\n\
        virtual function int val();\n\
          return x;\n\
        endfunction\n\
      endclass\n\
      class Derived extends Base;\n\
        function new();\n\
          super.new();\n\
          x = x + 5;\n\
        endfunction\n\
        virtual function int val();\n\
          return x + 100;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Base b;\n\
        Derived d;\n\
        d = new();\n\
        b = d;\n\
        r = b.val();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 115);
}

#[test]
fn base_handle_calls_base_when_not_overridden_instance() {
    // A genuine Base instance dispatches to Base::val.
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Base;\n\
        int x;\n\
        function new();\n\
          x = 7;\n\
        endfunction\n\
        virtual function int val();\n\
          return x;\n\
        endfunction\n\
      endclass\n\
      class Derived extends Base;\n\
        virtual function int val();\n\
          return x + 100;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Base b;\n\
        b = new();\n\
        r = b.val();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 7);
}

#[test]
fn string_field_param_and_display() {
    // The UVM component-naming pattern: a string field set from a constructor
    // argument and printed with %s.
    let src = "module top;\n\
      class Named;\n\
        string name;\n\
        function new(string n);\n\
          name = n;\n\
        endfunction\n\
        function void show();\n\
          $display(\"name=%s\", name);\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Named obj;\n\
        obj = new(\"alpha\");\n\
        obj.show();\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(sim.kernel_ref().output(), ["name=alpha"]);
}

#[test]
fn unqualified_class_method_shadows_global_function() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      function int pick();\n\
        return 1;\n\
      endfunction\n\
      class Picker;\n\
        function int pick();\n\
          return 42;\n\
        endfunction\n\
        function int run();\n\
          return pick();\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Picker p = new();\n\
        r = p.run();\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 42);
}

#[test]
fn virtual_output_class_handle_is_copied_to_caller() {
    let src = "module top;\n\
      logic [31:0] r = 0;\n\
      class Item;\n\
        int value;\n\
      endclass\n\
      class Holder;\n\
        Item stored;\n\
        virtual function bit try_get(output Item value);\n\
          value = stored;\n\
          return 1;\n\
        endfunction\n\
      endclass\n\
      initial begin\n\
        Item item = new();\n\
        Item result;\n\
        Holder holder = new();\n\
        item.value = 42;\n\
        holder.stored = item;\n\
        if (holder.try_get(result)) r = result.value;\n\
      end\n\
    endmodule\n";
    assert_eq!(net(&run(src), "r"), 42);
}
