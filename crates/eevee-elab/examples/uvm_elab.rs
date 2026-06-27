//! Probe: preprocess + parse + globally elaborate the real UVM library and
//! report how many classes/callables we can lay out and compile today.
//!
//! `cargo run -p eevee-elab --example uvm_elab`

use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let uvm_src = PathBuf::from(r"C:\Users\dellerbr\Simmy\uvm-core\src");
    let pkg = uvm_src.join("uvm_pkg.sv");

    let t0 = Instant::now();
    let file = match eevee_fe::parse_file(&pkg, vec![uvm_src.clone()]) {
        Ok(f) => f,
        Err(e) => {
            println!("front-end error: {e}");
            return;
        }
    };
    println!("parsed UVM front-end in {:.2}s", t0.elapsed().as_secs_f64());

    let t1 = Instant::now();
    let backend = eevee_ir::Interp;
    let (_sim, stats) = eevee_elab::elaborate_with_stats(&file, &backend);
    println!("global elaboration in {:.2}s", t1.elapsed().as_secs_f64());
    println!("  classes laid out      : {}", stats.classes);
    println!("  callables seen        : {}", stats.callables);
    println!("  callables stubbed     : {}", stats.callables_stubbed);
    let compiled = stats.callables.saturating_sub(stats.callables_stubbed);
    let pct = if stats.callables > 0 {
        100.0 * compiled as f64 / stats.callables as f64
    } else {
        0.0
    };
    println!("  callables compiled    : {compiled} ({pct:.1}%)");
    println!("  top reasons callables are still stubbed:");
    for (reason, count) in stats.stub_reasons.iter().take(20) {
        println!("    {count:5}  {reason}");
    }
}
