//! IEEE 1800-2017 stratified event-queue regions (§4.4).
//!
//! Within a single simulation time slot, events are processed in a fixed order
//! of *regions*. The ordering is what makes blocking vs. non-blocking
//! assignments, `#0`, and sampling deterministic. This enum mirrors the Python
//! reference's `Region` (`core/scheduler.py`) — a practical 7-region subset of
//! the full LRM list, sufficient for RTL + UVM:
//!
//! ```text
//! Preponed  -> Active <-> Inactive -> NBA  -> Observed -> Reactive -> Postponed
//!                  ^___________________|  (loop until all three drain)
//! ```
//!
//! * **Preponed** — sample values before anything mutates them (`$past`, SVA).
//! * **Active** — blocking assignments, `$display`, RHS evaluation of NBAs,
//!   process resumes from edges/events.
//! * **Inactive** — `#0` (explicit zero-delay) events.
//! * **NBA** — apply non-blocking (`<=`) updates. May wake Active events,
//!   looping back.
//! * **Observed** — `program`/assertion and covergroup sampling.
//! * **Reactive** — `program` block re-active region (testbench reactive code).
//! * **Postponed** — read-only `$monitor`/`$strobe`, end-of-slot.
//!
//! The numeric order is the scheduling priority (lower = earlier), so the enum
//! can be used directly as a sort key in the time-wheel.

/// A stratified event-queue region. Ordered by scheduling priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Region {
    Preponed = 0,
    Active = 1,
    Inactive = 2,
    Nba = 3,
    Observed = 4,
    Reactive = 5,
    Postponed = 6,
}

impl Region {
    /// All regions in scheduling order.
    pub const ALL: [Region; 7] = [
        Region::Preponed,
        Region::Active,
        Region::Inactive,
        Region::Nba,
        Region::Observed,
        Region::Reactive,
        Region::Postponed,
    ];

    /// The region's scheduling priority (lower runs earlier in a time slot).
    #[inline]
    pub const fn priority(self) -> u8 {
        self as u8
    }

    /// The next region in the same time slot, or `None` after `Postponed`.
    pub const fn next(self) -> Option<Region> {
        match self {
            Region::Preponed => Some(Region::Active),
            Region::Active => Some(Region::Inactive),
            Region::Inactive => Some(Region::Nba),
            Region::Nba => Some(Region::Observed),
            Region::Observed => Some(Region::Reactive),
            Region::Reactive => Some(Region::Postponed),
            Region::Postponed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_by_priority() {
        assert!(Region::Preponed < Region::Active);
        assert!(Region::Active < Region::Inactive);
        assert!(Region::Inactive < Region::Nba);
        assert!(Region::Nba < Region::Observed);
        assert!(Region::Observed < Region::Reactive);
        assert!(Region::Reactive < Region::Postponed);
    }

    #[test]
    fn priorities_are_dense() {
        for (i, r) in Region::ALL.iter().enumerate() {
            assert_eq!(r.priority() as usize, i);
        }
    }

    #[test]
    fn next_chain() {
        let mut r = Region::Preponed;
        let mut count = 1;
        while let Some(n) = r.next() {
            r = n;
            count += 1;
        }
        assert_eq!(r, Region::Postponed);
        assert_eq!(count, 7);
    }
}
