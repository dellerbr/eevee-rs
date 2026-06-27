//! End-to-end: SystemVerilog **source text** → Verible parse → AST → elaborate
//! → IR → run on the event-driven kernel. This is the P2 milestone: a real
//! `.sv` design (not hand-built IR) producing correct behavior.

use eevee_core::SimTime;
use eevee_elab::elaborate;
use eevee_fe::parse_source;
use eevee_ir::Interp;

const COUNTER: &str = "module top;\n\
  logic clk = 0;\n\
  logic [31:0] c = 0;\n\
  always #5 clk = ~clk;\n\
  always_ff @(posedge clk) c <= c + 1;\n\
endmodule\n";

#[test]
fn counter_runs_from_sv_source() {
    let file = parse_source(COUNTER).expect("parse");
    let backend = Interp;
    let mut sim = elaborate(&file, &backend);

    let c = sim.kernel().find_net("c").expect("net c exists");
    let clk = sim.kernel().find_net("clk").expect("net clk exists");

    // Initial values from `= 0`.
    assert_eq!(sim.kernel().net_value(c).to_u64(), 0);
    assert_eq!(sim.kernel().net_value(clk).to_u64(), 0);

    // #5 in the default 1ns/1ps timescale = 5_000_000 fs; clock period =
    // 10_000_000 fs; N-th posedge at (2N-1)*5_000_000 fs.
    let n = 200u64;
    let half = 5_000_000u64;
    sim.run_until(Some(SimTime::from_fs((2 * n - 1) * half)));

    assert_eq!(
        sim.kernel().net_value(c).to_u64(),
        n,
        "counter reached N from SV source"
    );
    // After the N-th (odd) posedge the clock is high.
    assert_eq!(sim.kernel().net_value(clk).to_u64(), 1, "clock toggled");
}
