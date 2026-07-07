//! `fork`/`join`/`join_any`/`join_none` end-to-end tests, from real
//! SystemVerilog source through the front-end, codegen, and the scheduler's
//! `Wait::Fork` join-watch bookkeeping (LRM 9.3.2).

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

/// `join_none`: the parent does not block — it runs the statement right
/// after the fork immediately, while both branches (which each finish later,
/// at different times) keep running detached.
#[test]
fn join_none_parent_does_not_block() {
    let src = "module top;\n\
      initial begin\n\
        fork\n\
          begin #10; $display(\"child A\"); end\n\
          begin #5; $display(\"child B\"); end\n\
        join_none\n\
        $display(\"parent continues\");\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(
        sim.kernel_ref().output(),
        ["parent continues", "child B", "child A"],
        "parent must not wait for either branch; branches still complete in time order"
    );
}

/// `join`: the parent blocks until *every* branch has finished — the
/// statement after `join` must not run until the slowest branch does.
#[test]
fn join_waits_for_all_branches() {
    let src = "module top;\n\
      initial begin\n\
        fork\n\
          begin #10; $display(\"A done\"); end\n\
          begin #5; $display(\"B done\"); end\n\
        join\n\
        $display(\"after join\");\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(
        sim.kernel_ref().output(),
        ["B done", "A done", "after join"],
        "parent must resume only after the slower (later) branch finishes"
    );
}

/// `join_any`: the parent blocks until the *first* branch finishes, then
/// resumes while the remaining branch keeps running in the background.
#[test]
fn join_any_waits_for_first_branch() {
    let src = "module top;\n\
      initial begin\n\
        fork\n\
          begin #10; $display(\"A done\"); end\n\
          begin #5; $display(\"B done\"); end\n\
        join_any\n\
        $display(\"after any\");\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(
        sim.kernel_ref().output(),
        ["B done", "after any", "A done"],
        "parent resumes right after the first branch; the second finishes later"
    );
}

/// A `fork` branch that is an implicit-`this` method call inside a class
/// method: each branch must see the *same* object (shared mutable state,
/// like real concurrent SV processes), proving the interpreter's reg0/`this`
/// seeding at spawn time is wired correctly, not just bare statement blocks.
#[test]
fn fork_branches_share_this_object() {
    let src = "class Runner;\n\
      int count;\n\
      task step(); count = count + 1; endtask\n\
      task go();\n\
        fork\n\
          step();\n\
          step();\n\
        join\n\
        $display(\"count=%0d\", count);\n\
      endtask\n\
    endclass\n\
    module top;\n\
      initial begin\n\
        Runner r = new();\n\
        r.go();\n\
      end\n\
    endmodule\n";
    let sim = run(src);
    assert_eq!(sim.kernel_ref().output(), ["count=2"]);
}
