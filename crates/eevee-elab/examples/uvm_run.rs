//! Attempt to actually run a tiny UVM testbench: build a real `uvm_object`
//! subclass, construct it, and print its name. Escalates the port from "UVM
//! compiles" toward "UVM runs".
//!
//! `cargo run -p eevee-elab --example uvm_run`

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

module top;
  initial begin
    `uvm_info("DEMO", "Hello from eevee UVM", UVM_LOW)
  end
endmodule
"#;

    let tmp = std::env::temp_dir().join("eevee_uvm_tb.sv");
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
        sim.run();
        sim.kernel_ref().output().to_vec()
    }));

    match result {
        Ok(out) => {
            println!("--- sim output ({} lines) ---", out.len());
            for line in &out {
                println!("{line}");
            }
        }
        Err(_) => println!("*** run panicked ***"),
    }
}
