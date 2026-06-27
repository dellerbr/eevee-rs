//! String/format builtins used by the UVM report path: string concatenation
//! `{a, b, c}`, enum `.name()`, `$sformatf`, `$cast`, and `&&`/`||`.

use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;

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
