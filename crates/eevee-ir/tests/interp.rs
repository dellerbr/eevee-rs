//! Integration tests for the IR interpreter.
//!
//! These prove the bytecode path reproduces the P1 scheduler semantics:
//! `always_ff` counting, event-driven `wait(cond)`, NBA old-value reads, and
//! the ALU opcodes — all driven through real [`Program`]s and the [`Interp`]
//! backend rather than hand-written Rust processes.

use std::rc::Rc;

use eevee_core::{LogicVec, SimTime};
use eevee_ir::{ExecBackend, Inst, Interp, Linkage, Program, ProgramBuilder};
use eevee_sched::{EdgeKind, NetId, Sim};

fn lv(v: u64, w: u32) -> LogicVec {
    LogicVec::from_u64(v, w)
}

// ---------------------------------------------------------------------------
// Counter via IR (mirrors the P1 scheduler counter test)
// ---------------------------------------------------------------------------

fn clock_prog(clk: NetId, half: u64) -> Program {
    let mut b = ProgramBuilder::new("clk");
    let r = b.new_reg();
    let top = b.new_label();
    b.bind(top);
    b.emit(Inst::Delay { fs: half });
    b.emit(Inst::NetRead { dst: r, net: clk });
    b.emit(Inst::Not { dst: r, a: r });
    b.emit(Inst::BlockingWrite { net: clk, src: r });
    b.jump(top);
    b.build()
}

fn counter_prog(clk: NetId, c: NetId) -> Program {
    let mut b = ProgramBuilder::new("counter");
    let r_one = b.new_reg();
    let r_c = b.new_reg();
    let r_sum = b.new_reg();
    let k1 = b.konst_logic(lv(1, 32));
    b.emit(Inst::LoadConst { dst: r_one, k: k1 });
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

#[test]
fn counter_counts_via_ir() {
    let n: u64 = 500;
    let half = 5u64;
    let backend = Interp;
    let mut sim = Sim::with_default_timescale();
    let clk = sim.kernel().new_net("clk", lv(0, 1));
    let c = sim.kernel().new_net("c", lv(0, 32));
    sim.add_process(backend.instantiate(Rc::new(counter_prog(clk, c)), Linkage::empty()));
    sim.add_process(backend.instantiate(Rc::new(clock_prog(clk, half)), Linkage::empty()));
    sim.run_until(Some(SimTime::from_fs((2 * n - 1) * half)));
    assert_eq!(sim.kernel().net_value(c).to_u64(), n);
}

// ---------------------------------------------------------------------------
// wait(cond) via IR — event-driven recheck, not polling
// ---------------------------------------------------------------------------

/// `wait(go == 1); done = 1;`
fn waiter_prog(go: NetId, done: NetId) -> Program {
    let mut b = ProgramBuilder::new("waiter");
    let r_one = b.new_reg();
    let r_go = b.new_reg();
    let r_t = b.new_reg();
    let k1 = b.konst_logic(lv(1, 1));
    let nl = b.netlist(&[go]);
    b.emit(Inst::LoadConst { dst: r_one, k: k1 });
    let recheck = b.new_label();
    let body = b.new_label();
    b.bind(recheck);
    b.emit(Inst::NetRead { dst: r_go, net: go });
    b.emit(Inst::Eq {
        dst: r_t,
        a: r_go,
        b: r_one,
    });
    b.branch_true(r_t, body);
    b.emit(Inst::WaitCond { nets: nl });
    b.jump(recheck);
    b.bind(body);
    b.emit(Inst::BlockingWrite {
        net: done,
        src: r_one,
    });
    b.emit(Inst::Finish);
    b.build()
}

/// `#delay go = 1;`
fn driver_prog(go: NetId, delay: u64) -> Program {
    let mut b = ProgramBuilder::new("driver");
    let r_one = b.new_reg();
    let k1 = b.konst_logic(lv(1, 1));
    b.emit(Inst::Delay { fs: delay });
    b.emit(Inst::LoadConst { dst: r_one, k: k1 });
    b.emit(Inst::BlockingWrite {
        net: go,
        src: r_one,
    });
    b.emit(Inst::Finish);
    b.build()
}

#[test]
fn wait_cond_via_ir() {
    let delay = 1000u64;
    let backend = Interp;
    let mut sim = Sim::with_default_timescale();
    let go = sim.kernel().new_net("go", lv(0, 1));
    let done = sim.kernel().new_net("done", lv(0, 1));
    sim.add_process(backend.instantiate(Rc::new(waiter_prog(go, done)), Linkage::empty()));
    sim.add_process(backend.instantiate(Rc::new(driver_prog(go, delay)), Linkage::empty()));
    sim.run();
    assert_eq!(
        sim.kernel().net_value(done).to_u64(),
        1,
        "waiter ran its body"
    );
    assert_eq!(
        sim.kernel().time(),
        SimTime::from_fs(delay),
        "woke at the write"
    );
}

// ---------------------------------------------------------------------------
// NBA reads old values (swap without a temp) via IR
// ---------------------------------------------------------------------------

fn swap_prog(a: NetId, b_net: NetId) -> Program {
    let mut b = ProgramBuilder::new("swap");
    let r_a = b.new_reg();
    let r_b = b.new_reg();
    b.emit(Inst::NetRead { dst: r_a, net: a });
    b.emit(Inst::NetRead {
        dst: r_b,
        net: b_net,
    });
    b.emit(Inst::NbaWrite { net: a, src: r_b });
    b.emit(Inst::NbaWrite {
        net: b_net,
        src: r_a,
    });
    b.emit(Inst::Finish);
    b.build()
}

#[test]
fn nba_swap_via_ir() {
    let backend = Interp;
    let mut sim = Sim::with_default_timescale();
    let a = sim.kernel().new_net("a", lv(1, 8));
    let b_net = sim.kernel().new_net("b", lv(2, 8));
    sim.add_process(backend.instantiate(Rc::new(swap_prog(a, b_net)), Linkage::empty()));
    sim.run();
    assert_eq!(sim.kernel().net_value(a).to_u64(), 2, "a got old b");
    assert_eq!(sim.kernel().net_value(b_net).to_u64(), 1, "b got old a");
}

// ---------------------------------------------------------------------------
// ALU opcodes plumb operands correctly
// ---------------------------------------------------------------------------

#[test]
fn alu_opcodes_via_ir() {
    let backend = Interp;
    let mut sim = Sim::with_default_timescale();
    let sum = sim.kernel().new_net("sum", lv(0, 8));
    let andn = sim.kernel().new_net("and", lv(0, 8));
    let xorn = sim.kernel().new_net("xor", lv(0, 8));

    let mut b = ProgramBuilder::new("alu");
    let r5 = b.new_reg();
    let r3 = b.new_reg();
    let rc = b.new_reg();
    let ra = b.new_reg();
    let rt = b.new_reg();
    let k5 = b.konst_logic(lv(5, 8));
    let k3 = b.konst_logic(lv(3, 8));
    let kc = b.konst_logic(lv(0xC, 8));
    let ka = b.konst_logic(lv(0xA, 8));
    b.emit(Inst::LoadConst { dst: r5, k: k5 });
    b.emit(Inst::LoadConst { dst: r3, k: k3 });
    b.emit(Inst::LoadConst { dst: rc, k: kc });
    b.emit(Inst::LoadConst { dst: ra, k: ka });
    b.emit(Inst::Add {
        dst: rt,
        a: r5,
        b: r3,
    });
    b.emit(Inst::BlockingWrite { net: sum, src: rt });
    b.emit(Inst::And {
        dst: rt,
        a: rc,
        b: ra,
    });
    b.emit(Inst::BlockingWrite { net: andn, src: rt });
    b.emit(Inst::Xor {
        dst: rt,
        a: rc,
        b: ra,
    });
    b.emit(Inst::BlockingWrite { net: xorn, src: rt });
    b.emit(Inst::Finish);
    sim.add_process(backend.instantiate(Rc::new(b.build()), Linkage::empty()));

    sim.run();
    assert_eq!(sim.kernel().net_value(sum).to_u64(), 8); // 5 + 3
    assert_eq!(sim.kernel().net_value(andn).to_u64(), 0x8); // 0xC & 0xA
    assert_eq!(sim.kernel().net_value(xorn).to_u64(), 0x6); // 0xC ^ 0xA
}
