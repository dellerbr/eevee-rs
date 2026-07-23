//! Integration tests for the event-driven kernel.
//!
//! These lock in the P1 semantics that the whole rewrite hinges on:
//! edge-sensitive wakeup, NBA vs. blocking ordering, `#delay` timing, and —
//! most importantly — that `wait(cond)` is woken by a *write*, never by polling.

use std::cell::Cell;
use std::rc::Rc;

use eevee_core::{Bit, LogicVec, SimTime};
use eevee_sched::{EdgeKind, Kernel, NetId, Process, Sim, Wait};

fn lv(v: u64, w: u32) -> LogicVec {
    LogicVec::from_u64(v, w)
}

#[test]
fn continuous_drivers_resolve_four_state_values() {
    let mut sim = Sim::with_default_timescale();
    let net = sim.kernel().new_net("resolved", LogicVec::z(1));
    let left = sim.kernel().new_driver(net);
    let right = sim.kernel().new_driver(net);

    sim.kernel().drive_net(left, lv(0, 1));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::Zero);

    sim.kernel().drive_net(right, lv(1, 1));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::X);

    sim.kernel().drive_net(left, lv(1, 1));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::One);

    sim.kernel().drive_net(right, LogicVec::z(1));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::One);

    sim.kernel().drive_net(left, LogicVec::x(1));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::X);
}

struct InertialPulseDriver {
    driver: eevee_sched::DriverId,
    phase: u8,
}

impl Process for InertialPulseDriver {
    fn resume(&mut self, kernel: &mut Kernel) -> Wait {
        match self.phase {
            0 => {
                self.phase = 1;
                kernel.schedule_drive(self.driver, lv(0, 1), 5);
                Wait::Delay(2)
            }
            1 => {
                self.phase = 2;
                kernel.schedule_drive(self.driver, lv(1, 1), 5);
                Wait::Delay(2)
            }
            _ => {
                kernel.schedule_drive(self.driver, lv(0, 1), 5);
                Wait::Finished
            }
        }
    }
}

#[test]
fn delayed_continuous_driver_rejects_short_pulse() {
    let mut sim = Sim::with_default_timescale();
    let net = sim.kernel().new_net("delayed", LogicVec::z(1));
    let driver = sim.kernel().new_driver(net);
    sim.kernel().drive_net(driver, lv(0, 1));
    sim.add_process(Box::new(InertialPulseDriver { driver, phase: 0 }));

    sim.run_until(Some(SimTime::from_fs(8)));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::Zero);

    sim.run_until(Some(SimTime::from_fs(9)));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::Zero);
    assert_eq!(
        sim.kernel().stats().time_advances,
        2,
        "canceled driver updates must not create phantom time slots"
    );
}

struct SameValueReevaluation {
    driver: eevee_sched::DriverId,
    phase: u8,
}

impl Process for SameValueReevaluation {
    fn resume(&mut self, kernel: &mut Kernel) -> Wait {
        kernel.schedule_drive(self.driver, lv(0, 1), 5);
        if self.phase == 0 {
            self.phase = 1;
            Wait::Delay(2)
        } else {
            Wait::Finished
        }
    }
}

#[test]
fn same_delayed_driver_value_does_not_postpone_update() {
    let mut sim = Sim::with_default_timescale();
    let net = sim.kernel().new_net("same_value", LogicVec::z(1));
    let driver = sim.kernel().new_driver(net);
    sim.add_process(Box::new(SameValueReevaluation { driver, phase: 0 }));

    sim.run_until(Some(SimTime::from_fs(5)));
    assert_eq!(sim.kernel().net_value(net).get_bit(0), Bit::Zero);
}

struct MultiNetWaiter {
    left: NetId,
    right: NetId,
    quiet: NetId,
    resumes: Rc<Cell<u64>>,
    armed: bool,
}

impl Process for MultiNetWaiter {
    fn resume(&mut self, _kernel: &mut Kernel) -> Wait {
        self.resumes.set(self.resumes.get() + 1);
        if self.armed {
            Wait::Finished
        } else {
            self.armed = true;
            Wait::Cond(vec![self.left, self.right, self.quiet])
        }
    }
}

struct UnrelatedThenBurst {
    unrelated: NetId,
    left: NetId,
    right: NetId,
    phase: u8,
}

impl Process for UnrelatedThenBurst {
    fn resume(&mut self, kernel: &mut Kernel) -> Wait {
        match self.phase {
            0 => {
                self.phase = 1;
                Wait::Delay(1)
            }
            1 => {
                self.phase = 2;
                kernel.write_net(self.unrelated, lv(1, 1));
                Wait::Delay(1)
            }
            _ => {
                kernel.write_net(self.left, lv(1, 1));
                kernel.write_net(self.right, lv(1, 1));
                Wait::Finished
            }
        }
    }
}

#[test]
fn multi_net_wait_ignores_unrelated_and_wakes_once() {
    let resumes = Rc::new(Cell::new(0));
    let mut sim = Sim::with_default_timescale();
    let left = sim.kernel().new_net("left", lv(0, 1));
    let right = sim.kernel().new_net("right", lv(0, 1));
    let quiet = sim.kernel().new_net("quiet", lv(0, 1));
    let unrelated = sim.kernel().new_net("unrelated", lv(0, 1));
    sim.add_process(Box::new(MultiNetWaiter {
        left,
        right,
        quiet,
        resumes: resumes.clone(),
        armed: false,
    }));
    sim.add_process(Box::new(UnrelatedThenBurst {
        unrelated,
        left,
        right,
        phase: 0,
    }));

    sim.run_until(Some(SimTime::from_fs(1)));
    assert_eq!(resumes.get(), 1, "unrelated write must not wake waiter");

    sim.run();
    assert_eq!(resumes.get(), 2, "two relevant writes must wake only once");
    assert_eq!(
        sim.kernel().net(quiet).waiter_count(),
        0,
        "firing one source must remove registrations from quiet siblings"
    );
    assert_eq!(sim.kernel().stats().wait_epoch_records, 0);
}

// ---------------------------------------------------------------------------
// always #5 clk = ~clk  +  always_ff @(posedge clk) c <= c + 1
// ---------------------------------------------------------------------------

struct Clock {
    clk: NetId,
    armed: bool,
    half: u64,
}
impl Process for Clock {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        if !self.armed {
            self.armed = true;
            return Wait::Delay(self.half);
        }
        let next = k.net_value(self.clk).bitnot();
        k.write_net(self.clk, next);
        Wait::Delay(self.half)
    }
}

struct Counter {
    clk: NetId,
    c: NetId,
    armed: bool,
}
impl Process for Counter {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        if self.armed {
            let next = k.net_value(self.c).add(&lv(1, 32));
            k.schedule_nba(self.c, next);
        } else {
            self.armed = true;
        }
        Wait::Edge(self.clk, EdgeKind::Posedge)
    }
}

#[test]
fn counter_counts_posedges() {
    let n: u64 = 1000;
    let half = 5u64;
    let mut sim = Sim::with_default_timescale();
    let clk = sim.kernel().new_net("clk", lv(0, 1));
    let c = sim.kernel().new_net("c", lv(0, 32));
    sim.add_process(Box::new(Counter {
        clk,
        c,
        armed: false,
    }));
    sim.add_process(Box::new(Clock {
        clk,
        armed: false,
        half,
    }));

    // N-th posedge at t = (2N-1)*half; final NBA settles in that slot.
    sim.run_until(Some(SimTime::from_fs((2 * n - 1) * half)));

    assert_eq!(sim.kernel().net_value(c).to_u64(), n);
    assert_eq!(sim.kernel().time(), SimTime::from_fs((2 * n - 1) * half));
}

// ---------------------------------------------------------------------------
// wait(cond) is woken by a write, NOT by polling
// ---------------------------------------------------------------------------

struct CondWaiter {
    go: NetId,
    resumes: Rc<Cell<u64>>,
    woke_at: Rc<Cell<u64>>,
    armed: bool,
}
impl Process for CondWaiter {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        self.resumes.set(self.resumes.get() + 1);
        self.armed = true;
        if k.net_value(self.go).is_true() {
            self.woke_at.set(k.time().as_fs());
            return Wait::Finished;
        }
        Wait::Cond(vec![self.go])
    }
}

struct DelayedDriver {
    go: NetId,
    delay: u64,
    phase: u8,
}
impl Process for DelayedDriver {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        if self.phase == 0 {
            self.phase = 1;
            return Wait::Delay(self.delay);
        }
        k.write_net(self.go, lv(1, 1));
        Wait::Finished
    }
}

#[test]
fn wait_cond_is_event_driven_not_polled() {
    let resumes = Rc::new(Cell::new(0u64));
    let woke_at = Rc::new(Cell::new(u64::MAX));
    let mut sim = Sim::with_default_timescale();
    let go = sim.kernel().new_net("go", lv(0, 1));
    sim.add_process(Box::new(CondWaiter {
        go,
        resumes: resumes.clone(),
        woke_at: woke_at.clone(),
        armed: false,
    }));
    sim.add_process(Box::new(DelayedDriver {
        go,
        delay: 1_000_000,
        phase: 0,
    }));

    sim.run();

    // The waiter is resumed exactly twice: once to arm at t=0, once when `go`
    // is actually written at t=1_000_000. A polling implementation would resume
    // it on every delta across the whole delay.
    assert_eq!(resumes.get(), 2, "wait(cond) must not poll");
    assert_eq!(woke_at.get(), 1_000_000, "woke at the write time");
}

#[test]
fn wait_never_satisfied_does_not_spin() {
    // A condition that never becomes true must simply leave the process parked;
    // the sim ends (no more events) without spinning.
    let resumes = Rc::new(Cell::new(0u64));
    let mut sim = Sim::with_default_timescale();
    let go = sim.kernel().new_net("go", lv(0, 1));
    sim.add_process(Box::new(CondWaiter {
        go,
        resumes: resumes.clone(),
        woke_at: Rc::new(Cell::new(0)),
        armed: false,
    }));
    sim.run();
    assert_eq!(resumes.get(), 1, "parked once, never polled");
}

// ---------------------------------------------------------------------------
// NBA reads old values and updates simultaneously (swap without temp)
// ---------------------------------------------------------------------------

struct NbaSwap {
    a: NetId,
    b: NetId,
    done: Rc<Cell<bool>>,
}
impl Process for NbaSwap {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        let a_old = k.net_value(self.a).clone();
        let b_old = k.net_value(self.b).clone();
        k.schedule_nba(self.a, b_old);
        k.schedule_nba(self.b, a_old);
        // In the SAME active region, the nets still hold their old values.
        assert_eq!(k.net_value(self.a).to_u64(), 1);
        assert_eq!(k.net_value(self.b).to_u64(), 2);
        self.done.set(true);
        Wait::Finished
    }
}

#[test]
fn nba_swaps_using_old_values() {
    let done = Rc::new(Cell::new(false));
    let mut sim = Sim::with_default_timescale();
    let a = sim.kernel().new_net("a", lv(1, 8));
    let b = sim.kernel().new_net("b", lv(2, 8));
    sim.add_process(Box::new(NbaSwap {
        a,
        b,
        done: done.clone(),
    }));
    sim.run();
    assert!(done.get());
    assert_eq!(sim.kernel().net_value(a).to_u64(), 2, "a got old b");
    assert_eq!(sim.kernel().net_value(b).to_u64(), 1, "b got old a");
}

// ---------------------------------------------------------------------------
// Blocking write is visible immediately; #delay advances time in order
// ---------------------------------------------------------------------------

struct BlockingThenDelay {
    a: NetId,
    log: Rc<Cell<u64>>,
    phase: u8,
}
impl Process for BlockingThenDelay {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        match self.phase {
            0 => {
                k.write_net(self.a, lv(5, 8));
                // Immediately visible (blocking).
                assert_eq!(k.net_value(self.a).to_u64(), 5);
                self.phase = 1;
                Wait::Delay(7)
            }
            _ => {
                // Woke at t=7.
                self.log.set(k.time().as_fs());
                Wait::Finished
            }
        }
    }
}

#[test]
fn blocking_visible_immediately_and_delay_advances_time() {
    let log = Rc::new(Cell::new(0u64));
    let mut sim = Sim::with_default_timescale();
    let a = sim.kernel().new_net("a", lv(0, 8));
    sim.add_process(Box::new(BlockingThenDelay {
        a,
        log: log.clone(),
        phase: 0,
    }));
    sim.run();
    assert_eq!(log.get(), 7);
    assert_eq!(sim.kernel().time(), SimTime::from_fs(7));
}

// ---------------------------------------------------------------------------
// posedge sensitivity ignores negedges
// ---------------------------------------------------------------------------

struct EdgeMonitor {
    sig: NetId,
    pos: Rc<Cell<u64>>,
    armed: bool,
}
impl Process for EdgeMonitor {
    fn resume(&mut self, _k: &mut Kernel) -> Wait {
        if self.armed {
            self.pos.set(self.pos.get() + 1);
        } else {
            self.armed = true;
        }
        Wait::Edge(self.sig, EdgeKind::Posedge)
    }
}

#[test]
fn posedge_waiter_ignores_negedges() {
    let pos = Rc::new(Cell::new(0u64));
    let half = 5u64;
    let toggles = 10u64; // 5 posedges, 5 negedges
    let mut sim = Sim::with_default_timescale();
    let clk = sim.kernel().new_net("clk", lv(0, 1));
    sim.add_process(Box::new(EdgeMonitor {
        sig: clk,
        pos: pos.clone(),
        armed: false,
    }));
    sim.add_process(Box::new(Clock {
        clk,
        armed: false,
        half,
    }));
    sim.run_until(Some(SimTime::from_fs(toggles * half)));
    assert_eq!(pos.get(), 5, "exactly the posedges, not the negedges");
}
