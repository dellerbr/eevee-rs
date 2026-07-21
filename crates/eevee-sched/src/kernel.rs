//! The simulation kernel and the [`Sim`] driver.
//!
//! # Region model
//!
//! Within one time slot the kernel cycles Active → Inactive → NBA until all
//! three drain, exactly as IEEE 1800-2017 §4 requires, then advances time to the
//! next scheduled event. (Preponed/Observed/Reactive/Postponed exist in
//! [`eevee_core::Region`] and will be wired in with sampling/program blocks in
//! later phases; P1 exercises Active/Inactive/NBA, which is what RTL needs.)
//!
//! # Why two structs ([`Kernel`] + [`Sim`])
//!
//! A process's [`Process::resume`] needs `&mut Kernel` (to read/write nets,
//! schedule delays and NBAs). If the processes lived *inside* the kernel we
//! could not borrow a process and the kernel simultaneously. So [`Sim`] owns the
//! process table and the kernel as sibling fields; the run loop destructures
//! `&mut self` into disjoint borrows of each. The same trick lets a net write
//! push to the active queue without allocating.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use eevee_core::{Bit, LogicVec, Region, SimTime, Timescale};

use crate::net::{detect_edge, Net, Waiter};
use crate::process::{DriverId, EdgeKind, EventId, ForkJoin, NetId, ProcId, Process, Wait};

#[derive(Clone, Copy)]
struct DriverTarget {
    net: NetId,
    slot: usize,
}

/// A parent process blocked on `join`/`join_any` for a set of forked children.
struct JoinWatch {
    parent: ProcId,
    /// Children not yet finished. `join_any` wakes the parent on the first
    /// removal; `join` waits until this is empty.
    remaining: Vec<ProcId>,
    any: bool,
}

/// A wakeup scheduled at a future time, ordered for the time wheel.
struct TimedEvent {
    time: SimTime,
    region: Region,
    seq: u64,
    proc: ProcId,
}

impl PartialEq for TimedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.region == other.region && self.seq == other.seq
    }
}
impl Eq for TimedEvent {}
impl Ord for TimedEvent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Earlier time first, then earlier region, then earlier seq (FIFO).
        self.time
            .cmp(&other.time)
            .then(self.region.cmp(&other.region))
            .then(self.seq.cmp(&other.seq))
    }
}
impl PartialOrd for TimedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Counters tracked for benchmarking and sanity checks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Number of [`Process::resume`] calls.
    pub resumes: u64,
    /// Number of net writes (blocking + NBA applies).
    pub net_writes: u64,
    /// Number of NBA updates applied.
    pub nba_applies: u64,
    /// Number of times simulation time advanced.
    pub time_advances: u64,
}

/// The simulation kernel: time, nets, the stratified queues, and the time wheel.
pub struct Kernel {
    time: SimTime,
    timescale: Timescale,
    nets: Vec<Net>,
    drivers: Vec<DriverTarget>,

    // Current-time-slot region queues of process wakeups.
    active: VecDeque<ProcId>,
    inactive: VecDeque<ProcId>,
    // Pending non-blocking updates, applied at the NBA region.
    nba: Vec<(NetId, LogicVec)>,
    nba_waiters: VecDeque<ProcId>,

    // Future events.
    timed: BinaryHeap<Reverse<TimedEvent>>,
    seq: u64,

    stop: bool,
    stats: Stats,

    // $display / $write output: captured for inspection and (optionally)
    // echoed to stdout.
    out: Vec<String>,
    echo: bool,

    // `fork ... join`/`join_any` parents currently blocked on their children.
    join_watches: Vec<JoinWatch>,

    // Processes waiting for a class/static/collection value to change.
    runtime_waiters: Vec<ProcId>,

    // Processes parked on IEEE named events.
    event_waiters: HashMap<EventId, Vec<ProcId>>,
}

impl Kernel {
    fn new(timescale: Timescale) -> Kernel {
        Kernel {
            time: SimTime::ZERO,
            timescale,
            nets: Vec::new(),
            drivers: Vec::new(),
            active: VecDeque::new(),
            inactive: VecDeque::new(),
            nba: Vec::new(),
            nba_waiters: VecDeque::new(),
            timed: BinaryHeap::new(),
            seq: 0,
            stop: false,
            stats: Stats::default(),
            out: Vec::new(),
            echo: true,
            join_watches: Vec::new(),
            runtime_waiters: Vec::new(),
            event_waiters: HashMap::new(),
        }
    }

    // ---- Net management -------------------------------------------------

    /// Create a net with an initial value, returning its handle.
    pub fn new_net(&mut self, name: impl Into<String>, initial: LogicVec) -> NetId {
        let id = NetId(self.nets.len());
        self.nets.push(Net::new(name, initial));
        id
    }

    /// Read a net's current value.
    #[inline]
    pub fn net_value(&self, net: NetId) -> &LogicVec {
        &self.nets[net.0].value
    }

    /// The net's metadata (name, waiters count, etc.).
    pub fn net(&self, net: NetId) -> &Net {
        &self.nets[net.0]
    }

    /// Find a net by name (linear scan; for elaboration/test wiring, not the
    /// hot path). Returns the first match.
    pub fn find_net(&self, name: &str) -> Option<NetId> {
        self.nets.iter().position(|n| n.name() == name).map(NetId)
    }

    /// Register a continuous driver on `net`. Until first driven its value is Z.
    pub fn new_driver(&mut self, net: NetId) -> DriverId {
        let width = self.nets[net.0].value.width();
        let slot = self.nets[net.0].driver_values.len();
        self.nets[net.0].driver_values.push(LogicVec::z(width));
        let id = DriverId(self.drivers.len());
        self.drivers.push(DriverTarget { net, slot });
        id
    }

    /// Update one continuous driver and write the net's resolved four-state value.
    pub fn drive_net(&mut self, driver: DriverId, value: LogicVec) {
        let target = self.drivers[driver.0];
        let width = self.nets[target.net.0].value.width();
        self.nets[target.net.0].driver_values[target.slot] = value.resize(width, false);
        let resolved = resolve_drivers(&self.nets[target.net.0].driver_values, width);
        self.write_net(target.net, resolved);
    }

    // ---- Display / output ----------------------------------------------

    /// Emit a `$display`/`$write` line: captured in the output log and, when
    /// echo is enabled (the default), printed to stdout.
    pub fn display(&mut self, line: String) {
        if self.echo {
            println!("{line}");
        }
        self.out.push(line);
    }

    /// All captured display output, in order.
    pub fn output(&self) -> &[String] {
        &self.out
    }

    /// Enable/disable echoing display output to stdout (tests disable it).
    pub fn set_echo(&mut self, echo: bool) {
        self.echo = echo;
    }

    /// Blocking write: update `net` **now** (Active region) and wake every
    /// parked process whose edge filter matches the transition.
    pub fn write_net(&mut self, net: NetId, val: LogicVec) {
        // Disjoint borrows of `nets` and `active` so we can wake processes
        // without allocating a scratch vector.
        let Kernel {
            nets,
            active,
            runtime_waiters,
            stats,
            ..
        } = self;
        let n = &mut nets[net.0];
        let old = std::mem::replace(&mut n.value, val);
        let edges = detect_edge(&old, &n.value);
        stats.net_writes += 1;
        if edges.changed {
            active.extend(runtime_waiters.drain(..));
        }
        if edges.changed && !n.waiters.is_empty() {
            let mut i = 0;
            while i < n.waiters.len() {
                if edges.fires(n.waiters[i].edge) {
                    let w = n.waiters.swap_remove(i);
                    active.push_back(w.proc);
                } else {
                    i += 1;
                }
            }
        }
    }

    /// Schedule a non-blocking update (`net <= val`), applied at the NBA region
    /// of the current time slot.
    #[inline]
    pub fn schedule_nba(&mut self, net: NetId, val: LogicVec) {
        self.nba.push((net, val));
    }

    fn drain_nba(&mut self) {
        // Take the batch; applying it may wake processes and even queue more
        // NBAs, which belong to the *next* NBA pass — standard semantics.
        let batch = std::mem::take(&mut self.nba);
        for (net, val) in batch {
            self.stats.nba_applies += 1;
            self.write_net(net, val);
        }
        self.active.extend(self.nba_waiters.drain(..));
    }

    // ---- Time / control -------------------------------------------------

    /// Current simulation time.
    #[inline]
    pub fn time(&self) -> SimTime {
        self.time
    }

    /// The active timescale.
    #[inline]
    pub fn timescale(&self) -> Timescale {
        self.timescale
    }

    /// Request the simulation stop after the current process settles.
    #[inline]
    pub fn request_stop(&mut self) {
        self.stop = true;
    }

    /// Wake processes parked on non-net `wait(cond)` state. Conditions are
    /// re-evaluated by their interpreter frames after being re-queued.
    pub fn notify_runtime_change(&mut self) {
        self.active.extend(self.runtime_waiters.drain(..));
    }

    /// Trigger a named event and wake every process currently waiting on it.
    pub fn trigger_event(&mut self, event: EventId) {
        if let Some(waiters) = self.event_waiters.remove(&event) {
            self.active.extend(waiters);
        }
    }

    /// Accumulated statistics.
    #[inline]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    // ---- Internal scheduling -------------------------------------------

    fn push_timed(&mut self, time: SimTime, region: Region, proc: ProcId) {
        self.seq += 1;
        self.timed.push(Reverse(TimedEvent {
            time,
            region,
            seq: self.seq,
            proc,
        }));
    }

    /// Park a process per the [`Wait`] it returned.
    fn arm(&mut self, pid: ProcId, wait: Wait) {
        match wait {
            Wait::Finished => {}
            Wait::Delay(fs) => {
                let t = self.time.saturating_add_fs(fs);
                self.push_timed(t, Region::Active, pid);
            }
            Wait::Edge(net, kind) => {
                self.nets[net.0].waiters.push(Waiter {
                    proc: pid,
                    edge: kind,
                });
            }
            Wait::Sensitivity(list) => {
                for (net, kind) in list {
                    self.nets[net.0].waiters.push(Waiter {
                        proc: pid,
                        edge: kind,
                    });
                }
            }
            Wait::Cond(nets) => {
                for net in nets {
                    self.nets[net.0].waiters.push(Waiter {
                        proc: pid,
                        edge: EdgeKind::AnyChange,
                    });
                }
            }
            Wait::RuntimeCond => self.runtime_waiters.push(pid),
            Wait::Nba => self.nba_waiters.push_back(pid),
            Wait::NamedEvent(event) => self.event_waiters.entry(event).or_default().push(pid),
            Wait::Fork { .. } => unreachable!(
                "Wait::Fork must be intercepted by Sim::drain_active (spawning \
                 needs the process table, which Kernel does not own) before \
                 reaching arm()"
            ),
        }
    }

    /// A process finished (`Wait::Finished`) — wake any `join`/`join_any`
    /// parent waiting on it. Cheap no-op when nothing is watching `pid`.
    fn notify_finished(&mut self, pid: ProcId) {
        let mut i = 0;
        while i < self.join_watches.len() {
            let had = self.join_watches[i].remaining.len();
            self.join_watches[i].remaining.retain(|&c| c != pid);
            let w = &self.join_watches[i];
            let satisfied = w.remaining.len() < had && (w.any || w.remaining.is_empty());
            if satisfied {
                let watch = self.join_watches.swap_remove(i);
                self.active.push_back(watch.parent);
            } else {
                i += 1;
            }
        }
    }

    /// Move every timed event at exactly `t` into its region's slot queue.
    fn dispatch_time_slot(&mut self, t: SimTime) {
        while let Some(Reverse(ev)) = self.timed.peek() {
            if ev.time != t {
                break;
            }
            let Reverse(ev) = self.timed.pop().unwrap();
            match ev.region {
                Region::Inactive => self.inactive.push_back(ev.proc),
                // Active and (for now) every other region resume in Active.
                _ => self.active.push_back(ev.proc),
            }
        }
    }
}

fn resolve_drivers(drivers: &[LogicVec], width: u32) -> LogicVec {
    let mut resolved = LogicVec::z(width);
    for bit in 0..width {
        let mut value = Bit::Z;
        for driver in drivers {
            value = match (value, driver.get_bit(bit)) {
                (current, Bit::Z) => current,
                (Bit::Z, driven) => driven,
                (Bit::X, _) | (_, Bit::X) => Bit::X,
                (Bit::Zero, Bit::One) | (Bit::One, Bit::Zero) => Bit::X,
                (current, _) => current,
            };
        }
        resolved.set_bit(bit, value);
    }
    resolved
}

/// The top-level simulation driver: owns the process table and the [`Kernel`].
pub struct Sim {
    kernel: Kernel,
    procs: Vec<Box<dyn Process>>,
}

impl Sim {
    /// Create a simulation with the given timescale.
    pub fn new(timescale: Timescale) -> Sim {
        Sim {
            kernel: Kernel::new(timescale),
            procs: Vec::new(),
        }
    }

    /// Create a simulation with the default `1ns/1ps` timescale.
    pub fn with_default_timescale() -> Sim {
        Sim::new(Timescale::default())
    }

    /// Mutable access to the kernel (to create nets before running, etc.).
    pub fn kernel(&mut self) -> &mut Kernel {
        &mut self.kernel
    }

    /// Read-only access to the kernel (to inspect nets / output after a run).
    pub fn kernel_ref(&self) -> &Kernel {
        &self.kernel
    }

    /// Add a process. It is queued for an initial `resume` at time 0 (where it
    /// will arm its first timing control), mirroring how all `initial`/`always`
    /// blocks start together and immediately hit their first event control.
    pub fn add_process(&mut self, p: Box<dyn Process>) -> ProcId {
        let id = ProcId(self.procs.len());
        self.procs.push(p);
        self.kernel.active.push_back(id);
        id
    }

    /// Run until no events remain or a stop is requested.
    pub fn run(&mut self) {
        self.run_until(None);
    }

    /// Run until `limit` (inclusive) or the simulation empties / stops.
    pub fn run_until(&mut self, limit: Option<SimTime>) {
        loop {
            // Settle the current time slot: Active <-> Inactive <-> NBA.
            loop {
                if !self.kernel.active.is_empty() {
                    self.drain_active();
                    if self.kernel.stop {
                        return;
                    }
                    continue;
                }
                if !self.kernel.inactive.is_empty() {
                    // Inactive (#0) promotes to Active.
                    while let Some(p) = self.kernel.inactive.pop_front() {
                        self.kernel.active.push_back(p);
                    }
                    continue;
                }
                if !self.kernel.nba.is_empty() || !self.kernel.nba_waiters.is_empty() {
                    self.kernel.drain_nba();
                    continue;
                }
                break;
            }

            // Advance time to the next scheduled event.
            let next_t = match self.kernel.timed.peek() {
                Some(Reverse(ev)) => ev.time,
                None => return, // nothing left to do
            };
            if let Some(limit) = limit {
                if next_t > limit {
                    self.kernel.time = limit;
                    return;
                }
            }
            self.kernel.time = next_t;
            self.kernel.stats.time_advances += 1;
            self.kernel.dispatch_time_slot(next_t);
        }
    }

    /// Drain the Active queue, resuming each process and arming its next wait.
    /// Processes woken *during* this drain (by net writes) are appended and run
    /// in the same pass — correct Active-region behavior.
    fn drain_active(&mut self) {
        while let Some(pid) = self.kernel.active.pop_front() {
            // Disjoint borrow: resume the process with &mut Kernel.
            let Sim { kernel, procs } = self;
            kernel.stats.resumes += 1;
            let wait = procs[pid.0].resume(kernel);
            match wait {
                // Spawning needs `procs` (which Kernel doesn't own), so this
                // variant is handled here instead of via `Kernel::arm`.
                Wait::Fork { children, join } => self.spawn_fork(pid, children, join),
                Wait::Finished => {
                    // No-op park, then wake any join/join_any parent on `pid`.
                    self.kernel.arm(pid, Wait::Finished);
                    self.kernel.notify_finished(pid);
                }
                other => self.kernel.arm(pid, other),
            }
            if self.kernel.stop {
                return;
            }
        }
    }

    /// Handle a `Wait::Fork`: add each child to the process table (queued for
    /// its first resume, same as [`Sim::add_process`]), then either re-queue
    /// `parent` immediately (`join_none`) or park it on a [`JoinWatch`] until
    /// the children satisfy `join`/`join_any`.
    fn spawn_fork(&mut self, parent: ProcId, children: Vec<Box<dyn Process>>, join: ForkJoin) {
        let mut ids = Vec::with_capacity(children.len());
        for c in children {
            let id = ProcId(self.procs.len());
            self.procs.push(c);
            self.kernel.active.push_back(id);
            ids.push(id);
        }
        if matches!(join, ForkJoin::None) || ids.is_empty() {
            self.kernel.active.push_back(parent);
        } else {
            self.kernel.join_watches.push(JoinWatch {
                parent,
                remaining: ids,
                any: matches!(join, ForkJoin::Any),
            });
        }
    }
}
