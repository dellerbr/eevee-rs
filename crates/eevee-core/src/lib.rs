//! Core data types for the eevee SystemVerilog simulator.
//!
//! This crate holds the three foundational pieces every other crate builds on:
//!
//! * [`logic`] — the 4-state ({0,1,x,z}) packed bit-vector ([`LogicVec`]),
//!   the simulator's core datum. Encoded as two words `{aval, bval}` exactly
//!   like the Python reference's `LogicValue {_val, _xz}`.
//! * [`time`] — simulation time in femtoseconds ([`SimTime`]) and the
//!   `timescale ([`Timescale`]) mapping.
//! * [`region`] — the IEEE 1800-2017 stratified event-queue [`Region`]s.
//!
//! These types are deliberately dependency-free and `#![forbid(unsafe_code)]`:
//! correctness of the core datum is paramount, and the hot paths are written
//! to be allocation-free for the common (<= 64-bit) case without `unsafe`.

#![forbid(unsafe_code)]

pub mod logic;
pub mod region;
pub mod time;

pub use logic::{Bit, LogicVec};
pub use region::Region;
pub use time::{SimTime, TimeUnit, Timescale};
