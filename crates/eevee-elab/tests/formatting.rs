//! String/format builtins used by the UVM report path: string concatenation
//! `{a, b, c}`, enum `.name()`, `$sformatf`, `$cast`, and `&&`/`||`.

use eevee_core::LogicVec;
use eevee_elab::{elaborate, elaborate_with_dpi};
use eevee_fe::parse_source;
use eevee_ir::{DpiRegistry, Interp, Value};

fn run(src: &str) -> Vec<String> {
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);
    sim.kernel().set_echo(false);
    sim.run();
    sim.kernel_ref().output().to_vec()
}

#[test]
fn string_concat_and_sformatf() {
    let src = "module top;\n\
      initial begin\n\
        string a;\n\
        string b;\n\
        a = \"Hello\";\n\
        b = {a, \", \", \"world\"};\n\
        $display(\"%s\", b);\n\
        $display(\"%s\", $sformatf(\"n=%0d\", 42));\n\
      end\n\
    endmodule\n";
    assert_eq!(run(src), ["Hello, world", "n=42"]);
}

#[test]
fn enum_name_method() {
    // Explicit values (like UVM's severities/verbosities) resolve to names.
    let src = "package p;\n\
      typedef enum { LOW = 100, MED = 200 } verb_e;\n\
      endpackage\n\
      module top;\n\
      import p::*;\n\
        initial begin\n\
          verb_e v;\n\
          v = MED;\n\
          $display(\"%s=%0d\", v.name(), v);\n\
        end\n\
      endmodule\n";
    assert_eq!(run(src), ["MED=200"]);
}

#[test]
fn logical_and_or_short_paths() {
    // `&&`/`||` must reduce to 1-bit logic, not arithmetic.
    let src = "module top;\n\
      initial begin\n\
        int a;\n\
        int b;\n\
        a = 200;\n\
        b = 100;\n\
        if ((a >= b) && (b != 0)) $display(\"yes\");\n\
        else $display(\"no\");\n\
      end\n\
    endmodule\n";
    assert_eq!(run(src), ["yes"]);
}

#[test]
fn cast_assigns_value() {
    let src = "module top;\n\
      initial begin\n\
        int x;\n\
        int y;\n\
        y = 7;\n\
        if ($cast(x, y)) $display(\"x=%0d\", x);\n\
      end\n\
    endmodule\n";
    assert_eq!(run(src), ["x=7"]);
}

#[test]
fn dpi_command_line_iteration_reaches_empty_sentinel() {
    let src = "module top;\n\
      import \"DPI-C\" function string uvm_dpi_get_next_arg_c(int init);\n\
      initial begin\n\
        string arg;\n\
        int count = 0;\n\
        do begin\n\
          arg = uvm_dpi_get_next_arg_c(count == 0);\n\
          if (arg != \"\") count++;\n\
        end while (arg != \"\");\n\
        $display(\"argc=%0d\", count);\n\
      end\n\
    endmodule\n";
    let output = run(src);
    let count: usize = output[0]
        .strip_prefix("argc=")
        .expect("argc output")
        .parse()
        .expect("numeric argc");
    assert!(count >= 1);
}

#[test]
fn dpi_default_tool_identity_bindings() {
    let src = "module top;\n\
      import \"DPI-C\" function string uvm_dpi_get_tool_name_c();\n\
      import \"DPI-C\" function string uvm_dpi_get_tool_version_c();\n\
      initial $display(\"%s %s\", uvm_dpi_get_tool_name_c(), uvm_dpi_get_tool_version_c());\n\
    endmodule\n";
    let output = run(src);
    assert!(output[0].starts_with("Eevee "));
    assert!(output[0].len() > "Eevee ".len());
}

#[test]
fn custom_dpi_binding_returns_and_copies_out() {
    let src = "module top;\n\
      import \"DPI-C\" function int mutate_c(inout int value);\n\
      initial begin\n\
        int value = 4;\n\
        int result = mutate_c(value);\n\
        $display(\"value=%0d result=%0d\", value, result);\n\
      end\n\
    endmodule\n";
    let mut dpi = DpiRegistry::default();
    dpi.register("mutate_c", |args| {
        let input = args[0].as_logic().to_u64();
        args[0] = Value::Logic(LogicVec::from_u64(9, 32));
        Value::Logic(LogicVec::from_u64(input + 1, 32))
    });
    let file = parse_source(src).expect("parse");
    let backend = Interp;
    let mut sim = elaborate_with_dpi(&file, &backend, dpi);
    sim.kernel().set_echo(false);
    sim.run();
    assert_eq!(sim.kernel_ref().output(), ["value=9 result=5"]);
}
