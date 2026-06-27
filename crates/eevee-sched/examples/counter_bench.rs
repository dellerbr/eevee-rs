//! P1 throughput benchmark: a free-running counter.
//!
//! Models the canonical RTL fragment
//!
//! ```systemverilog
//! logic        clk = 0;
//! logic [31:0] c   = 0;
//! always #5            clk <= ~clk;          // clock generator
//! always_ff @(posedge clk) c <= c + 1;       // counter
//! ```
//!
//! and runs it for N posedges, reporting **cycles/sec** — the perf trend number
//! every later phase is measured against. Run with:
//!
//! ```text
//! cargo run --release -p eevee-sched --example counter_bench [N]
//! ```
//!
//! The point of this benchmark is to exercise the real event machinery — timed
//! events (the clock), edge-sensitive wakeup (the counter parks on `posedge
//! clk` and is woken only by the clock write, never by polling), blocking net
//! updates, and the NBA region — not a synthetic loop.

use std::time::Instant;

use eevee_core::{LogicVec, SimTime};
use eevee_sched::{EdgeKind, Kernel, NetId, Process, Sim, Wait};

const HALF_PERIOD_FS: u64 = 5;

/// `always #HALF clk <= ~clk;` — but modeled faithfully so the first edge is at
/// t = HALF (the block arms its delay at t=0 without toggling), avoiding a
/// spurious t=0 posedge before the counter has armed.
struct Clock {
    clk: NetId,
    armed: bool,
}

impl Process for Clock {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        if !self.armed {
            // t=0: just arm the first half-period delay.
            self.armed = true;
            return Wait::Delay(HALF_PERIOD_FS);
        }
        // Toggle and schedule the next half-period.
        let next = k.net_value(self.clk).bitnot();
        k.write_net(self.clk, next);
        Wait::Delay(HALF_PERIOD_FS)
    }

    fn label(&self) -> &str {
        "clkgen"
    }
}

/// `always_ff @(posedge clk) c <= c + 1;`
struct Counter {
    clk: NetId,
    c: NetId,
    one: LogicVec,
    armed: bool,
}

impl Process for Counter {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        if self.armed {
            // A posedge fired: NBA c <= c + 1.
            let next = k.net_value(self.c).add(&self.one);
            k.schedule_nba(self.c, next);
        } else {
            self.armed = true;
        }
        Wait::Edge(self.clk, EdgeKind::Posedge)
    }

    fn label(&self) -> &str {
        "counter"
    }
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);

    let mut sim = Sim::with_default_timescale();
    let clk = sim.kernel().new_net("clk", LogicVec::from_u64(0, 1));
    let c = sim.kernel().new_net("c", LogicVec::from_u64(0, 32));

    sim.add_process(Box::new(Counter {
        clk,
        c,
        one: LogicVec::from_u64(1, 32),
        armed: false,
    }));
    sim.add_process(Box::new(Clock { clk, armed: false }));

    // The k-th posedge (1-indexed) occurs at t = (2k-1)*HALF. Running to that
    // exact time produces precisely N posedges, and the final NBA settles, so
    // the counter ends at N.
    let end = SimTime::from_fs((2 * n - 1) * HALF_PERIOD_FS);

    let t0 = Instant::now();
    sim.run_until(Some(end));
    let elapsed = t0.elapsed();

    let final_c = sim.kernel().net_value(c).to_u64();
    let stats = sim.kernel().stats();
    let secs = elapsed.as_secs_f64();
    let cps = n as f64 / secs;

    println!("eevee-rs P1 counter benchmark");
    println!("  cycles (posedges) : {n}");
    println!(
        "  final counter     : {final_c}  ({})",
        if final_c == n { "OK" } else { "MISMATCH" }
    );
    println!("  wall time         : {:.3} s", secs);
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
    println!("  nba applies       : {}", stats.nba_applies);
    println!("  time advances     : {}", stats.time_advances);

    assert_eq!(
        final_c, n,
        "counter did not reach N — benchmark is not modeling correctly"
    );
}
