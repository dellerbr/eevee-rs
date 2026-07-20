//! Processes and the things they wait on.
//!
//! A *process* models an SV procedural thread (`initial`, `always`,
//! `always_ff`, a forked statement, a task call). The event-driven kernel does
//! **not** run a coroutine per process or poll conditions every delta — instead
//! a process is a resumable state machine: each call to [`Process::resume`] runs
//! the body up to the next timing control and returns a [`Wait`] describing what
//! should wake it. The kernel parks it on exactly that wakeup source (a future
//! time, a net edge, or a set of nets for `wait(cond)`), so a process is only
//! resumed when something it actually depends on changes.
//!
//! In P1 these state machines are written by hand (see the counter benchmark and
//! the integration tests). In P2 the procedural interpreter will implement
//! [`Process`] generically, saving/restoring its PC and operand stack across
//! `resume` calls — the same interface.

use crate::kernel::Kernel;

/// Handle to a process owned by the [`crate::Sim`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcId(pub usize);

/// Handle to a net owned by the [`Kernel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetId(pub usize);

/// Identity of an IEEE named `event` value.
pub type EventId = u64;

/// The kind of net activity a process is sensitive to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// `@(posedge net)` — a transition of the LSB toward 1.
    Posedge,
    /// `@(negedge net)` — a transition of the LSB toward 0.
    Negedge,
    /// `@(net)` / level sensitivity — any value change (case-inequality).
    AnyChange,
}

/// The completion discipline of a `fork` block (LRM 9.3.2), shared by the IR
/// layer (which lowers `fork`/`join`/`join_any`/`join_none` to this) and the
/// kernel (which implements the join-wait bookkeeping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkJoin {
    /// `join` — the parent resumes only after every branch finishes.
    All,
    /// `join_any` — the parent resumes after the first branch finishes.
    Any,
    /// `join_none` — the parent resumes immediately; branches run detached.
    None,
}

/// What a process is waiting for after a [`Process::resume`] call.
///
/// Returning a `Wait` parks the process; the kernel re-runs `resume` when the
/// wakeup fires. None of these variants cause polling — `Cond` is re-checked
/// only when one of its listed nets is written.
pub enum Wait {
    /// The process has finished; it will never be resumed again.
    Finished,
    /// Suspend for `fs` femtoseconds of simulation time (`#delay`).
    Delay(u64),
    /// Suspend until `net` shows `edge` (`@(posedge clk)` etc.).
    Edge(NetId, EdgeKind),
    /// Suspend until any listed `(net, edge)` fires (`@(a or posedge b)`).
    Sensitivity(Vec<(NetId, EdgeKind)>),
    /// `wait(cond)`: suspend until any of these nets changes, then re-`resume`
    /// (where the process re-evaluates the condition). The process is woken
    /// only on real writes to these nets — never by delta polling.
    Cond(Vec<NetId>),
    /// `wait(cond)` over class fields, statics, or collections. The process is
    /// rechecked after a runtime mutation notification, never delta-polled.
    RuntimeCond,
    /// Resume in the NBA region so the process can apply a pending procedural
    /// variable update before continuing past an expression event control.
    Nba,
    /// Suspend until the named event is triggered.
    NamedEvent(EventId),
    /// `fork branches join*`: spawn each of `children` as an independent
    /// concurrent process. The scheduler (not this process) owns the process
    /// table, so spawning happens one level up — [`crate::Sim`] intercepts
    /// this variant instead of parking it via the usual net-based waits. Per
    /// `join`, the parent process is re-queued once every child has returned
    /// `Wait::Finished` (`All`), once the first child has (`Any`), or
    /// immediately (`None`, branches run detached).
    Fork {
        children: Vec<Box<dyn Process>>,
        join: ForkJoin,
    },
}

/// A resumable SV process.
pub trait Process {
    /// Run the body up to the next timing control and report what should wake
    /// it next. The kernel guarantees `resume` is called once at startup and
    /// then once per fired wakeup.
    fn resume(&mut self, k: &mut Kernel) -> Wait;

    /// Optional human-readable label (debug/VCD/trace). Default empty.
    fn label(&self) -> &str {
        ""
    }
}
