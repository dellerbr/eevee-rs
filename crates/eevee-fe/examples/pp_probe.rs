//! Probe: preprocess the real UVM library and try to parse the result with
//! Verible. Reports how far the front-end gets toward loading UVM.
//!
//! `cargo run -p eevee-fe --example pp_probe`

use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let uvm_src = PathBuf::from(r"C:\Users\dellerbr\Simmy\uvm-core\src");
    let pkg = uvm_src.join("uvm_pkg.sv");

    let t0 = Instant::now();
    let mut pp = eevee_fe::Preprocessor::new(vec![uvm_src.clone()]);
    let text = match pp.process_file(&pkg) {
        Ok(t) => t,
        Err(e) => {
            println!("preprocess error: {e}");
            return;
        }
    };
    let pp_secs = t0.elapsed().as_secs_f64();
    println!("preprocessed uvm_pkg.sv:");
    println!("  lines : {}", text.lines().count());
    println!("  chars : {}", text.len());
    println!("  time  : {pp_secs:.2}s");

    // How many UVM classes appear in the expanded text?
    let class_count = text.matches("class ").count();
    let endclass = text.matches("endclass").count();
    println!("  'class ' occurrences  : {class_count}");
    println!("  'endclass' occurrences: {endclass}");

    // Any unexpanded `uvm_ macro references left?
    let leftover = text.matches("`uvm_").count();
    println!("  leftover `uvm_ refs   : {leftover}");

    let t1 = Instant::now();
    match eevee_fe::parse_source(&text) {
        Ok(file) => {
            println!(
                "VERIBLE PARSE + LOWER: OK in {:.2}s",
                t1.elapsed().as_secs_f64()
            );
            println!("  top-level items lowered: {}", file.items.len());
            let mut classes = 0;
            let mut funcs = 0;
            for it in &file.items {
                let items = match it {
                    eevee_ast::Item::Package(p) => &p.items,
                    eevee_ast::Item::Module(m) => &m.items,
                    eevee_ast::Item::Class(_) => {
                        classes += 1;
                        continue;
                    }
                    eevee_ast::Item::Func(_) => {
                        funcs += 1;
                        continue;
                    }
                };
                for mi in items {
                    match mi {
                        eevee_ast::ModuleItem::Class(_) => classes += 1,
                        eevee_ast::ModuleItem::Func(_) => funcs += 1,
                        _ => {}
                    }
                }
            }
            println!("  classes lowered: {classes}");
            println!("  package-level functions lowered: {funcs}");
        }
        Err(e) => {
            let msg = format!("{e}");
            let head: String = msg.chars().take(600).collect();
            println!("VERIBLE PARSE ERROR (first 600 chars):\n{head}");
        }
    }
}
