//! P2 throughput benchmark: the **same** free-running counter as the P1
//! `eevee-sched` benchmark, but now both the clock and the counter run as real
//! interpreted IR programs instead of hand-written Rust state machines.
//!
//! ```systemverilog
//! always #5            clk = ~clk;
//! always_ff @(posedge clk) c <= c + 1;
//! ```
//!
//! Comparing this number against the P1 hand-coded number is the honest
//! "interpreter tax": it tells us how much the IR dispatch loop costs over the
//! theoretical best. Run:
//!
//! ```text
//! cargo run --release -p eevee-ir --example counter_ir_bench [N]
//! ```

use std::rc::Rc;
use std::time::Instant;

use eevee_core::{LogicVec, SimTime};
use eevee_ir::{ExecBackend, Inst, Interp, Linkage, Program, ProgramBuilder};
use eevee_sched::{EdgeKind, NetId, Sim};

const HALF_PERIOD_FS: u64 = 5;

/// `always #5 clk = ~clk;` — the leading `#5` means the first toggle is at
/// t=5 (no spurious t=0 posedge), matching the P1 model.
fn build_clock(clk: NetId) -> Program {
    let mut b = ProgramBuilder::new("clkgen");
    let r = b.new_reg();
    let top = b.new_label();
    b.bind(top);
    b.emit(Inst::Delay { fs: HALF_PERIOD_FS });
    b.emit(Inst::NetRead { dst: r, net: clk });
    b.emit(Inst::Not { dst: r, a: r });
    b.emit(Inst::BlockingWrite { net: clk, src: r });
    b.jump(top);
    b.build()
}

/// `always_ff @(posedge clk) c <= c + 1;`
///
/// The constant `1` is loaded once in the prologue (before the loop); the
/// register persists across resumes, so the loop body is just wait/read/add/
/// nba. The leading `WaitEdge` means the first resume only arms — no increment
/// at t=0 — exactly the `always_ff` semantics.
fn build_counter(clk: NetId, c: NetId) -> Program {
    let mut b = ProgramBuilder::new("counter");
    let r_one = b.new_reg();
    let r_c = b.new_reg();
    let r_sum = b.new_reg();
    let k_one = b.konst_logic(LogicVec::from_u64(1, 32));

    // prologue (runs once)
    b.emit(Inst::LoadConst {
        dst: r_one,
        k: k_one,
    });

    let top = b.new_label();
    b.bind(top);
    b.emit(Inst::WaitEdge {
        net: clk,
        edge: EdgeKind::Posedge,
    });
    b.emit(Inst::NetRead { dst: r_c, net: c });
    b.emit(Inst::Add {
        dst: r_sum,
        a: r_c,
        b: r_one,
    });
    b.emit(Inst::NbaWrite { net: c, src: r_sum });
    b.jump(top);
    b.build()
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);

    let backend = Interp;
    let mut sim = Sim::with_default_timescale();
    let clk = sim.kernel().new_net("clk", LogicVec::from_u64(0, 1));
    let c = sim.kernel().new_net("c", LogicVec::from_u64(0, 32));

    let counter = Rc::new(build_counter(clk, c));
    let clock = Rc::new(build_clock(clk));
    sim.add_process(backend.instantiate(counter, Linkage::empty()));
    sim.add_process(backend.instantiate(clock, Linkage::empty()));

    let end = SimTime::from_fs((2 * n - 1) * HALF_PERIOD_FS);

    let t0 = Instant::now();
    sim.run_until(Some(end));
    let elapsed = t0.elapsed();

    let final_c = sim.kernel().net_value(c).to_u64();
    let stats = sim.kernel().stats();
    let secs = elapsed.as_secs_f64();
    let cps = n as f64 / secs;

    println!(
        "eevee-rs P2 counter benchmark (IR interpreter, backend = {})",
        backend.name()
    );
    println!("  cycles (posedges) : {n}");
    println!(
        "  final counter     : {final_c}  ({})",
        if final_c == n { "OK" } else { "MISMATCH" }
    );
    println!("  wall time         : {secs:.3} s");
    println!("  ----");
    println!("  cycles/sec        : {:.3} M ({:.0}/s)", cps / 1e6, cps);
    println!(
        "  process resumes   : {} ({:.1} M/s)",
        stats.resumes,
        stats.resumes as f64 / secs / 1e6
    );
    println!(
        "  net writes        : {} ({:.1} M/s)",
        stats.net_writes,
        stats.net_writes as f64 / secs / 1e6
    );

    assert_eq!(final_c, n, "IR counter did not reach N");
}
