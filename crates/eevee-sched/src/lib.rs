//! Event-driven SystemVerilog simulation kernel.
//!
//! This crate replaces the Python reference's coroutine-per-process + busy-poll
//! scheduler with a true event-driven kernel:
//!
//! * [`Net`]s carry **sensitivity lists**; writing a net wakes only the parked
//!   processes whose edge filter matches (`net.rs`). No per-delta re-evaluation.
//! * Processes are resumable state machines ([`Process`]), parked on a precise
//!   [`Wait`] (a future time, a net edge, or the read-set of a `wait(cond)`).
//! * The [`Kernel`] runs the IEEE 1800-2017 Active/Inactive/NBA region cycle and
//!   a femtosecond time wheel ([`Sim::run`]).
//!
//! See the crate `examples/counter_bench.rs` for the P1 throughput benchmark.

#![forbid(unsafe_code)]

pub mod kernel;
pub mod net;
pub mod process;

pub use kernel::{Kernel, Sim, Stats};
pub use net::{DriveStrength, Net, NetResolution};
pub use process::{DriverId, EdgeKind, EventId, ForkJoin, NetId, ProcId, Process, Wait};
