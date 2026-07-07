//! Frontier probe: an actual `run_test("my_test")` testbench (a user test
//! class extending `uvm_test`, overriding `run_phase` to raise/drop an
//! objection). This is the next escalation past `uvm_run` (single `\`uvm_info`
//! call) — it exercises the factory, phasing FSM, and objection machinery.
//!
//! `cargo run -p eevee-elab --example uvm_run_test`
//!
//! Has a resume-count watchdog (not a wall-clock timeout) so a real infinite
//! loop in the interpreter prints a clear "watchdog tripped" message instead
//! of hanging the terminal forever.

use std::path::PathBuf;

fn main() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("eevee-elab lives at <repo>/crates/eevee-elab")
        .to_path_buf();
    let uvm_src = repo_root.join("uvm-core").join("src");
    let tb = r#"
`include "uvm_pkg.sv"
import uvm_pkg::*;

class my_test extends uvm_test;
  `uvm_component_utils(my_test)

  function new(string name, uvm_component parent);
    super.new(name, parent);
  endfunction

  task run_phase(uvm_phase phase);
    phase.raise_objection(this);
    `uvm_info("MYTEST", "running my_test", UVM_LOW)
    #10;
    `uvm_info("MYTEST", "done", UVM_LOW)
    phase.drop_objection(this);
  endtask
endclass

module top;
  initial run_test("my_test");
endmodule
"#;

    let tmp = std::env::temp_dir().join("eevee_uvm_run_test_tb.sv");
    std::fs::write(&tmp, tb).expect("write tb");

    let mut pp = eevee_fe::Preprocessor::new(vec![uvm_src.clone()]);
    let text = match pp.process_file(&tmp) {
        Ok(t) => t,
        Err(e) => {
            println!("preprocess error: {e}");
            return;
        }
    };
    println!("preprocessed testbench: {} lines", text.lines().count());

    let file = match eevee_fe::parse_source(&text) {
        Ok(f) => f,
        Err(e) => {
            println!("parse error: {e}");
            return;
        }
    };

    let backend = eevee_ir::Interp;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (mut sim, stats) = eevee_elab::elaborate_with_stats(&file, &backend);
        println!(
            "elaborated: {} classes, {}/{} callables compiled",
            stats.classes,
            stats.callables - stats.callables_stubbed,
            stats.callables
        );
        sim.kernel().set_echo(true);
        // Watchdog: real UVM run_test can spin forever if a phase-completion
        // signal is missed. run_until with a generous simulated-time limit
        // bounds it without needing a wall-clock timeout.
        sim.run_until(Some(eevee_core::SimTime(1_000_000_000)));
        sim.kernel_ref().output().to_vec()
    }));

    match result {
        Ok(lines) => {
            println!("--- sim output ({} lines) ---", lines.len());
            for l in lines {
                println!("{l}");
            }
        }
        Err(payload) => {
            let msg = panic_message(payload.as_ref());
            println!("--- PANIC ---\n{msg}");
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
