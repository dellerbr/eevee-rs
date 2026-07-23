//! Nets and edge detection.
//!
//! A [`Net`] is a 4-state signal plus its **sensitivity list**: the processes
//! parked waiting for activity on it. This is the heart of the event-driven
//! model — when a net is written, the kernel wakes exactly the parked processes
//! whose edge filter matches, and no one else. There is no global "re-evaluate
//! every waiter each delta" step.

use eevee_core::{Bit, LogicVec};

use crate::process::{EdgeKind, ProcId};

/// A process parked on a net, with the edge it is waiting for.
pub(crate) struct Waiter {
    pub proc: ProcId,
    pub edge: EdgeKind,
    pub epoch: u64,
}

/// A 4-state net with an attached sensitivity list.
pub struct Net {
    pub(crate) value: LogicVec,
    pub(crate) driver_values: Vec<LogicVec>,
    pub(crate) waiters: Vec<Waiter>,
    pub(crate) name: String,
}

impl Net {
    pub(crate) fn new(name: impl Into<String>, value: LogicVec) -> Net {
        Net {
            value,
            driver_values: Vec::new(),
            waiters: Vec::new(),
            name: name.into(),
        }
    }

    /// Current value (read-only).
    pub fn value(&self) -> &LogicVec {
        &self.value
    }

    /// The net's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Number of currently registered sensitivity waiters.
    pub fn waiter_count(&self) -> usize {
        self.waiters.len()
    }
}

/// Which edges a transition `old -> new` produced. Computed on the LSB for
/// pos/neg edges (the conventional clock semantics) and on the whole vector for
/// `changed`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Edges {
    pub posedge: bool,
    pub negedge: bool,
    pub changed: bool,
}

impl Edges {
    #[inline]
    pub fn fires(&self, edge: EdgeKind) -> bool {
        match edge {
            EdgeKind::Posedge => self.posedge,
            EdgeKind::Negedge => self.negedge,
            EdgeKind::AnyChange => self.changed,
        }
    }
}

/// Detect edges for a transition from `old` to `new`.
///
/// Pos/neg edges follow IEEE 1800-2017 §9.4.2 on the LSB: a posedge is any move
/// toward 1 (`0->1`, `0->x`, `0->z`, `x->1`, `z->1`); a negedge is any move
/// toward 0. `changed` is full-vector case-inequality.
pub(crate) fn detect_edge(old: &LogicVec, new: &LogicVec) -> Edges {
    let changed = !old.eq_case(new);
    if !changed {
        return Edges {
            posedge: false,
            negedge: false,
            changed: false,
        };
    }
    let o = old.get_bit(0);
    let n = new.get_bit(0);
    Edges {
        posedge: is_posedge(o, n),
        negedge: is_negedge(o, n),
        changed,
    }
}

#[inline]
fn is_posedge(old: Bit, new: Bit) -> bool {
    // Any transition of the LSB toward 1.
    matches!(
        (old, new),
        (Bit::Zero, Bit::One | Bit::X | Bit::Z) | (Bit::X | Bit::Z, Bit::One)
    )
}

#[inline]
fn is_negedge(old: Bit, new: Bit) -> bool {
    // Any transition of the LSB toward 0.
    matches!(
        (old, new),
        (Bit::One, Bit::Zero | Bit::X | Bit::Z) | (Bit::X | Bit::Z, Bit::Zero)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b1(v: u64) -> LogicVec {
        LogicVec::from_u64(v, 1)
    }

    #[test]
    fn clock_edges() {
        let e = detect_edge(&b1(0), &b1(1));
        assert!(e.posedge && !e.negedge && e.changed);
        let e = detect_edge(&b1(1), &b1(0));
        assert!(!e.posedge && e.negedge && e.changed);
        let e = detect_edge(&b1(1), &b1(1));
        assert!(!e.posedge && !e.negedge && !e.changed);
    }

    #[test]
    fn x_transitions() {
        // 0 -> x is a posedge; x -> 1 is a posedge
        assert!(detect_edge(&b1(0), &LogicVec::x(1)).posedge);
        assert!(detect_edge(&LogicVec::x(1), &b1(1)).posedge);
        // 1 -> x is a negedge; x -> 0 is a negedge
        assert!(detect_edge(&b1(1), &LogicVec::x(1)).negedge);
        assert!(detect_edge(&LogicVec::x(1), &b1(0)).negedge);
    }

    #[test]
    fn multibit_change_no_edge() {
        // a vector change with LSB staying 0 -> changed but no pos/neg edge
        let e = detect_edge(&LogicVec::from_u64(0b00, 2), &LogicVec::from_u64(0b10, 2));
        assert!(e.changed && !e.posedge && !e.negedge);
    }
}
