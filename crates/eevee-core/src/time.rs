//! Simulation time and `timescale handling.
//!
//! Time is tracked in **femtoseconds** as a `u64` ([`SimTime`]), matching the
//! Python reference's choice. A `u64` of femtoseconds covers ~5.1 hours of
//! simulated 1 fs ticks, and far longer at realistic precisions — ample for any
//! DV run. All scheduling math is integer; there is no floating-point time.

use std::fmt;

/// SI time units expressible in a `timescale directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    Fs,
    Ps,
    Ns,
    Us,
    Ms,
    S,
}

impl TimeUnit {
    /// Femtoseconds per unit.
    #[inline]
    pub const fn fs(self) -> u64 {
        match self {
            TimeUnit::Fs => 1,
            TimeUnit::Ps => 1_000,
            TimeUnit::Ns => 1_000_000,
            TimeUnit::Us => 1_000_000_000,
            TimeUnit::Ms => 1_000_000_000_000,
            TimeUnit::S => 1_000_000_000_000_000,
        }
    }

    /// Parse a unit suffix (`fs`/`ps`/`ns`/`us`/`ms`/`s`).
    pub fn parse(s: &str) -> Option<TimeUnit> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fs" => Some(TimeUnit::Fs),
            "ps" => Some(TimeUnit::Ps),
            "ns" => Some(TimeUnit::Ns),
            "us" => Some(TimeUnit::Us),
            "ms" => Some(TimeUnit::Ms),
            "s" => Some(TimeUnit::S),
            _ => None,
        }
    }

    /// The suffix string.
    pub const fn suffix(self) -> &'static str {
        match self {
            TimeUnit::Fs => "fs",
            TimeUnit::Ps => "ps",
            TimeUnit::Ns => "ns",
            TimeUnit::Us => "us",
            TimeUnit::Ms => "ms",
            TimeUnit::S => "s",
        }
    }
}

/// A point in (or span of) simulation time, in femtoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SimTime(pub u64);

impl SimTime {
    /// Time zero.
    pub const ZERO: SimTime = SimTime(0);

    /// Construct from raw femtoseconds.
    #[inline]
    pub const fn from_fs(fs: u64) -> SimTime {
        SimTime(fs)
    }

    /// Construct from an integer count of a [`TimeUnit`].
    #[inline]
    pub const fn from_units(amount: u64, unit: TimeUnit) -> SimTime {
        SimTime(amount * unit.fs())
    }

    /// Raw femtoseconds.
    #[inline]
    pub const fn as_fs(self) -> u64 {
        self.0
    }

    /// Value in nanoseconds (lossy; for display only).
    #[inline]
    pub fn as_ns(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Saturating add of a femtosecond delta.
    #[inline]
    pub const fn saturating_add_fs(self, delta: u64) -> SimTime {
        SimTime(self.0.saturating_add(delta))
    }
}

impl std::ops::Add for SimTime {
    type Output = SimTime;
    #[inline]
    fn add(self, rhs: SimTime) -> SimTime {
        SimTime(self.0 + rhs.0)
    }
}

impl std::ops::Sub for SimTime {
    type Output = SimTime;
    #[inline]
    fn sub(self, rhs: SimTime) -> SimTime {
        SimTime(self.0 - rhs.0)
    }
}

impl fmt::Display for SimTime {
    /// Renders in the largest unit that keeps the value integral, e.g.
    /// `5000000 fs` prints as `5ns`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fs = self.0;
        for unit in [
            TimeUnit::S,
            TimeUnit::Ms,
            TimeUnit::Us,
            TimeUnit::Ns,
            TimeUnit::Ps,
        ] {
            let u = unit.fs();
            if fs != 0 && fs % u == 0 {
                return write!(f, "{}{}", fs / u, unit.suffix());
            }
        }
        write!(f, "{fs}fs")
    }
}

/// A `timescale: a time unit and a time precision, both in femtoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timescale {
    unit_fs: u64,
    prec_fs: u64,
}

impl Timescale {
    /// Construct from a unit and precision.
    pub fn new(unit: (u64, TimeUnit), precision: (u64, TimeUnit)) -> Timescale {
        Timescale {
            unit_fs: unit.0 * unit.1.fs(),
            prec_fs: precision.0 * precision.1.fs(),
        }
    }

    /// Parse a pair like `("1ns", "1ps")`.
    pub fn parse(unit: &str, precision: &str) -> Option<Timescale> {
        Some(Timescale {
            unit_fs: parse_time_spec(unit)?,
            prec_fs: parse_time_spec(precision)?,
        })
    }

    /// Femtoseconds of one time unit.
    #[inline]
    pub const fn unit_fs(self) -> u64 {
        self.unit_fs
    }

    /// Femtoseconds of the precision (the rounding quantum).
    #[inline]
    pub const fn prec_fs(self) -> u64 {
        self.prec_fs
    }

    /// Convert a `#delay` amount expressed in time *units* into simulation
    /// femtoseconds, rounded to the precision quantum (IEEE 1800-2017 §3.14.4).
    pub fn delay_to_fs(self, amount: f64) -> u64 {
        let raw = amount * self.unit_fs as f64;
        let q = self.prec_fs as f64;
        ((raw / q).round() as u64) * self.prec_fs
    }
}

impl Default for Timescale {
    /// `1ns / 1ps`, the de-facto default.
    fn default() -> Timescale {
        Timescale {
            unit_fs: TimeUnit::Ns.fs(),
            prec_fs: TimeUnit::Ps.fs(),
        }
    }
}

/// Parse a spec like `"1ns"`, `"10ps"`, `"100 fs"` into femtoseconds.
fn parse_time_spec(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| c.is_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok().or(if num.trim().is_empty() {
        Some(1.0)
    } else {
        None
    })?;
    let unit = TimeUnit::parse(unit)?;
    Some((num * unit.fs() as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_conversions() {
        assert_eq!(TimeUnit::Ns.fs(), 1_000_000);
        assert_eq!(SimTime::from_units(5, TimeUnit::Ns).as_fs(), 5_000_000);
        assert_eq!(SimTime::from_units(5, TimeUnit::Ns).as_ns(), 5.0);
    }

    #[test]
    fn time_arithmetic() {
        let a = SimTime::from_units(3, TimeUnit::Ns);
        let b = SimTime::from_units(2, TimeUnit::Ns);
        assert_eq!((a + b).as_fs(), 5_000_000);
        assert_eq!((a - b).as_fs(), 1_000_000);
        assert!(b < a);
    }

    #[test]
    fn time_display() {
        assert_eq!(SimTime::from_units(5, TimeUnit::Ns).to_string(), "5ns");
        assert_eq!(SimTime::from_fs(0).to_string(), "0fs");
        assert_eq!(SimTime::from_fs(1500).to_string(), "1500fs");
        assert_eq!(SimTime::from_units(2, TimeUnit::Us).to_string(), "2us");
    }

    #[test]
    fn timescale_parse_and_delay() {
        let ts = Timescale::parse("1ns", "1ps").unwrap();
        assert_eq!(ts.unit_fs(), 1_000_000);
        assert_eq!(ts.prec_fs(), 1_000);
        // #5 in 1ns units -> 5_000_000 fs
        assert_eq!(ts.delay_to_fs(5.0), 5_000_000);
        // #2.5 ns -> 2_500_000 fs
        assert_eq!(ts.delay_to_fs(2.5), 2_500_000);
    }

    #[test]
    fn timescale_rounds_to_precision() {
        let ts = Timescale::parse("1ns", "1ns").unwrap();
        // precision is 1ns, so #2.4ns rounds to 2ns
        assert_eq!(ts.delay_to_fs(2.4), 2_000_000);
        assert_eq!(ts.delay_to_fs(2.6), 3_000_000);
    }

    #[test]
    fn default_timescale() {
        let ts = Timescale::default();
        assert_eq!(ts.unit_fs(), 1_000_000);
        assert_eq!(ts.prec_fs(), 1_000);
    }
}
