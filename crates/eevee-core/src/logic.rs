//! 4-state packed logic vectors.
//!
//! # Encoding
//!
//! Every bit is one of four states `{0, 1, x, z}`. A [`LogicVec`] stores a
//! `width`-bit value as two bit-planes, `aval` and `bval`, matching the
//! Python reference (`LogicValue {_val, _xz}`) bit-for-bit:
//!
//! | state | `aval` | `bval` |
//! |-------|--------|--------|
//! | `0`   |   0    |   0    |
//! | `1`   |   1    |   0    |
//! | `x`   |   0    |   1    |
//! | `z`   |   1    |   1    |
//!
//! So `bval == 0` for the whole vector iff it is fully known (no x/z), which
//! makes [`LogicVec::is_known`] a single comparison — the property the hot
//! paths lean on. `aval & !bval` isolates the known-1 bits, which is the basis
//! for SV truthiness (`if (expr)`).
//!
//! # Storage
//!
//! The common case in real RTL (and the P1 counter benchmark) is a width of at
//! most 64 bits. That case is stored **inline** in two `u64`s — no allocation,
//! no indirection — so a 32-bit `c <= c + 1` is a handful of integer ops. Wider
//! vectors spill to a heap-allocated limb array. The invariant in both reprs:
//! bits at or above `width` are always 0 in both planes.

use std::fmt;

/// A single 4-state bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bit {
    Zero,
    One,
    X,
    Z,
}

impl Bit {
    /// `(aval, bval)` plane bits for this state.
    #[inline]
    pub const fn planes(self) -> (u64, u64) {
        match self {
            Bit::Zero => (0, 0),
            Bit::One => (1, 0),
            Bit::X => (0, 1),
            Bit::Z => (1, 1),
        }
    }

    /// Decode a `(aval, bval)` plane pair (low bit of each) into a [`Bit`].
    #[inline]
    pub const fn from_planes(aval: u64, bval: u64) -> Bit {
        match (aval & 1, bval & 1) {
            (0, 0) => Bit::Zero,
            (1, 0) => Bit::One,
            (0, 1) => Bit::X,
            _ => Bit::Z,
        }
    }

    /// The display character for this bit (`0`, `1`, `x`, `z`).
    #[inline]
    pub const fn to_char(self) -> char {
        match self {
            Bit::Zero => '0',
            Bit::One => '1',
            Bit::X => 'x',
            Bit::Z => 'z',
        }
    }

    /// True for `x` or `z`.
    #[inline]
    pub const fn is_unknown(self) -> bool {
        matches!(self, Bit::X | Bit::Z)
    }
}

/// Internal storage: inline for `width <= 64`, heap limbs otherwise.
#[derive(Clone)]
enum Repr {
    /// `width <= 64`. High bits (>= width) are 0.
    Small { aval: u64, bval: u64 },
    /// `width > 64`. Little-endian limbs; high bits of the top limb are 0.
    Wide { aval: Box<[u64]>, bval: Box<[u64]> },
}

/// A `width`-bit 4-state logic vector. See the module docs for the encoding.
#[derive(Clone)]
pub struct LogicVec {
    width: u32,
    repr: Repr,
}

/// Mask of the low `width` bits of a single `u64` (`width` in `1..=64`).
#[inline]
const fn word_mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

#[inline]
fn limbs_for(width: u32) -> usize {
    (width as usize).div_ceil(64)
}

impl LogicVec {
    // ---------------------------------------------------------------------
    // Constructors
    // ---------------------------------------------------------------------

    /// All-zero vector of `width` bits.
    #[inline]
    pub fn zero(width: u32) -> LogicVec {
        assert!(width >= 1, "LogicVec width must be >= 1");
        if width <= 64 {
            LogicVec {
                width,
                repr: Repr::Small { aval: 0, bval: 0 },
            }
        } else {
            let n = limbs_for(width);
            LogicVec {
                width,
                repr: Repr::Wide {
                    aval: vec![0u64; n].into_boxed_slice(),
                    bval: vec![0u64; n].into_boxed_slice(),
                },
            }
        }
    }

    /// All-ones (every bit `1`) vector of `width` bits.
    pub fn ones(width: u32) -> LogicVec {
        let mut v = LogicVec::zero(width);
        v.fill_planes(true, false);
        v
    }

    /// All-`x` vector of `width` bits.
    pub fn x(width: u32) -> LogicVec {
        let mut v = LogicVec::zero(width);
        v.fill_planes(false, true);
        v
    }

    /// All-`z` vector of `width` bits.
    pub fn z(width: u32) -> LogicVec {
        let mut v = LogicVec::zero(width);
        v.fill_planes(true, true);
        v
    }

    /// Build from a `u64` value, zero-extended/truncated to `width` bits.
    /// The result is fully known (no x/z).
    pub fn from_u64(val: u64, width: u32) -> LogicVec {
        assert!(width >= 1, "LogicVec width must be >= 1");
        if width <= 64 {
            LogicVec {
                width,
                repr: Repr::Small {
                    aval: val & word_mask(width),
                    bval: 0,
                },
            }
        } else {
            let n = limbs_for(width);
            let mut aval = vec![0u64; n];
            aval[0] = val;
            LogicVec {
                width,
                repr: Repr::Wide {
                    aval: aval.into_boxed_slice(),
                    bval: vec![0u64; n].into_boxed_slice(),
                },
            }
        }
    }

    /// Build a fully-known vector from a signed `i64`, sign-extended to `width`.
    pub fn from_i64(val: i64, width: u32) -> LogicVec {
        if width <= 64 {
            LogicVec {
                width,
                repr: Repr::Small {
                    aval: (val as u64) & word_mask(width),
                    bval: 0,
                },
            }
        } else {
            let n = limbs_for(width);
            let fill = if val < 0 { u64::MAX } else { 0 };
            let mut aval = vec![fill; n];
            aval[0] = val as u64;
            let mut v = LogicVec {
                width,
                repr: Repr::Wide {
                    aval: aval.into_boxed_slice(),
                    bval: vec![0u64; n].into_boxed_slice(),
                },
            };
            v.normalize_top();
            v
        }
    }

    /// Build from explicit `(aval, bval)` low words (for `width <= 64`).
    pub fn from_planes_small(aval: u64, bval: u64, width: u32) -> LogicVec {
        assert!(
            (1..=64).contains(&width),
            "from_planes_small requires width in 1..=64"
        );
        let m = word_mask(width);
        LogicVec {
            width,
            repr: Repr::Small {
                aval: aval & m,
                bval: bval & m,
            },
        }
    }

    /// Build from a big-endian sequence of [`Bit`]s (index 0 = MSB), the order
    /// `4'b10xz` is written in source.
    pub fn from_bits_msb_first(bits: &[Bit]) -> LogicVec {
        let width = bits.len().max(1) as u32;
        let mut v = LogicVec::zero(width);
        for (i, b) in bits.iter().rev().enumerate() {
            v.set_bit(i as u32, *b);
        }
        v
    }

    /// Parse a SystemVerilog sized literal like `4'b10x1`, `8'hF0`, `32'd42`,
    /// `1'bz`. Underscores are ignored. Returns `None` on malformed input.
    ///
    /// This is a focused parser for the literal forms the runtime needs; the
    /// full lexer lives in the front-end. Bases `b`, `o`, `h`, `d` supported.
    pub fn parse_sized(s: &str) -> Option<LogicVec> {
        let s: String = s.chars().filter(|c| *c != '_').collect();
        let (width_str, rest) = s.split_once('\'')?;
        let width: u32 = width_str.trim().parse().ok()?;
        if width == 0 {
            return None;
        }
        let rest = rest.trim();
        let mut chars = rest.chars();
        let mut base = chars.next()?.to_ascii_lowercase();
        if base == 's' {
            // signed marker, e.g. 8'shFF
            base = chars.next()?.to_ascii_lowercase();
        }
        let digits: String = chars.collect();
        let bits_per = match base {
            'b' => 1,
            'o' => 3,
            'h' => 4,
            'd' => {
                let val: u64 = digits.trim().parse().ok()?;
                return Some(LogicVec::from_u64(val, width));
            }
            _ => return None,
        };
        let mut v = LogicVec::zero(width);
        let mut pos: u32 = 0;
        for ch in digits.chars().rev() {
            let c = ch.to_ascii_lowercase();
            for k in 0..bits_per {
                if pos >= width {
                    break;
                }
                let bit = match c {
                    'x' => Bit::X,
                    'z' | '?' => Bit::Z,
                    _ => {
                        let d = c.to_digit(16)?;
                        if (d >> k) & 1 == 1 {
                            Bit::One
                        } else {
                            Bit::Zero
                        }
                    }
                };
                v.set_bit(pos, bit);
                pos += 1;
            }
        }
        Some(v)
    }

    // ---------------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------------

    #[inline]
    fn fill_planes(&mut self, aval_one: bool, bval_one: bool) {
        let width = self.width;
        match &mut self.repr {
            Repr::Small { aval, bval } => {
                let m = word_mask(width);
                *aval = if aval_one { m } else { 0 };
                *bval = if bval_one { m } else { 0 };
            }
            Repr::Wide { aval, bval } => {
                let fa = if aval_one { u64::MAX } else { 0 };
                let fb = if bval_one { u64::MAX } else { 0 };
                for w in aval.iter_mut() {
                    *w = fa;
                }
                for w in bval.iter_mut() {
                    *w = fb;
                }
                mask_top(width, aval, bval);
            }
        }
    }

    /// Zero the bits above `width` in the top limb of a Wide repr.
    fn normalize_top(&mut self) {
        let width = self.width;
        if let Repr::Wide { aval, bval } = &mut self.repr {
            mask_top(width, aval, bval);
        }
    }

    // ---------------------------------------------------------------------
    // Accessors
    // ---------------------------------------------------------------------

    /// Number of bits.
    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// `(aval, bval)` low words if this vector is stored inline (`width <= 64`).
    #[inline]
    fn small(&self) -> Option<(u64, u64)> {
        match &self.repr {
            Repr::Small { aval, bval } => Some((*aval, *bval)),
            Repr::Wide { .. } => None,
        }
    }

    /// True if the vector has no `x` or `z` bits.
    #[inline]
    pub fn is_known(&self) -> bool {
        match &self.repr {
            Repr::Small { bval, .. } => *bval == 0,
            Repr::Wide { bval, .. } => bval.iter().all(|w| *w == 0),
        }
    }

    /// True if any bit is `x` or `z`.
    #[inline]
    pub fn has_unknown(&self) -> bool {
        !self.is_known()
    }

    /// SV truthiness: true iff at least one bit is a *known* 1
    /// (`(aval & !bval) != 0`). x/z and all-zero are false. This is the
    /// `if (expr)` / `wait (expr)` condition rule.
    #[inline]
    pub fn is_true(&self) -> bool {
        match &self.repr {
            Repr::Small { aval, bval } => (aval & !bval) != 0,
            Repr::Wide { aval, bval } => aval.iter().zip(bval.iter()).any(|(a, b)| (a & !b) != 0),
        }
    }

    /// SystemVerilog conditional selection, including bitwise branch merging
    /// when the condition is indeterminate.
    pub fn conditional(
        condition: &LogicVec,
        when_true: &LogicVec,
        when_false: &LogicVec,
    ) -> LogicVec {
        let width = when_true.width().max(when_false.width());
        if condition.is_true() {
            return when_true.resize(width, false);
        }
        if condition.is_known() {
            return when_false.resize(width, false);
        }
        let when_true = when_true.resize(width, false);
        let when_false = when_false.resize(width, false);
        let mut merged = LogicVec::x(width);
        for bit in 0..width {
            let true_bit = when_true.get_bit(bit);
            if true_bit == when_false.get_bit(bit) {
                merged.set_bit(bit, true_bit);
            }
        }
        merged
    }

    /// Read a single bit.
    pub fn get_bit(&self, idx: u32) -> Bit {
        if idx >= self.width {
            return Bit::Zero;
        }
        match &self.repr {
            Repr::Small { aval, bval } => Bit::from_planes(aval >> idx, bval >> idx),
            Repr::Wide { aval, bval } => {
                let limb = (idx / 64) as usize;
                let off = idx % 64;
                Bit::from_planes(aval[limb] >> off, bval[limb] >> off)
            }
        }
    }

    /// Write a single bit.
    pub fn set_bit(&mut self, idx: u32, bit: Bit) {
        if idx >= self.width {
            return;
        }
        let (ba, bb) = bit.planes();
        match &mut self.repr {
            Repr::Small { aval, bval } => {
                let m = 1u64 << idx;
                *aval = (*aval & !m) | (ba << idx);
                *bval = (*bval & !m) | (bb << idx);
            }
            Repr::Wide { aval, bval } => {
                let limb = (idx / 64) as usize;
                let off = idx % 64;
                let m = 1u64 << off;
                aval[limb] = (aval[limb] & !m) | (ba << off);
                bval[limb] = (bval[limb] & !m) | (bb << off);
            }
        }
    }

    /// Value as an unsigned `u64`, with x/z bits read as 0. Only meaningful for
    /// `width <= 64`; higher bits are ignored.
    #[inline]
    pub fn to_u64(&self) -> u64 {
        match &self.repr {
            Repr::Small { aval, bval } => aval & !bval,
            Repr::Wide { aval, bval } => aval[0] & !bval[0],
        }
    }

    /// Value as `u64` only if fully known, else `None`.
    #[inline]
    pub fn try_to_u64(&self) -> Option<u64> {
        if self.is_known() && self.width <= 64 {
            self.small().map(|(a, _)| a)
        } else if self.is_known() {
            // Wide but value fits in low limb with all higher limbs zero.
            if let Repr::Wide { aval, .. } = &self.repr {
                if aval[1..].iter().all(|w| *w == 0) {
                    return Some(aval[0]);
                }
            }
            None
        } else {
            None
        }
    }

    /// Sign-extended value as `i64` (x/z bits read as 0). For `width <= 64`.
    pub fn to_i64(&self) -> i64 {
        let raw = self.to_u64() & word_mask(self.width);
        if self.width < 64 {
            let sign = 1u64 << (self.width - 1);
            if raw & sign != 0 {
                (raw | !word_mask(self.width)) as i64
            } else {
                raw as i64
            }
        } else {
            raw as i64
        }
    }

    // ---------------------------------------------------------------------
    // Resize
    // ---------------------------------------------------------------------

    /// Resize to `new_width`, truncating or zero/sign-extending as needed.
    pub fn resize(&self, new_width: u32, signed: bool) -> LogicVec {
        if new_width == self.width {
            return self.clone();
        }
        let mut out = LogicVec::zero(new_width);
        let copy = new_width.min(self.width);
        for i in 0..copy {
            out.set_bit(i, self.get_bit(i));
        }
        if new_width > self.width {
            let ext = if signed {
                self.get_bit(self.width - 1)
            } else {
                Bit::Zero
            };
            // Sign/zero extension; SV extends x/z by the (x/z) sign bit too.
            let ext = match ext {
                Bit::One if signed => Bit::One,
                Bit::X if signed => Bit::X,
                Bit::Z if signed => Bit::X, // z sign-extends as x
                _ => Bit::Zero,
            };
            if ext != Bit::Zero {
                for i in self.width..new_width {
                    out.set_bit(i, ext);
                }
            }
        }
        out
    }

    // ---------------------------------------------------------------------
    // Equality
    // ---------------------------------------------------------------------

    /// Case equality (`===`): bit-exact including x/z, widths must match.
    pub fn eq_case(&self, other: &LogicVec) -> bool {
        if self.width != other.width {
            return false;
        }
        match (&self.repr, &other.repr) {
            (Repr::Small { aval: a1, bval: b1 }, Repr::Small { aval: a2, bval: b2 }) => {
                a1 == a2 && b1 == b2
            }
            _ => (0..self.width).all(|i| self.get_bit(i) == other.get_bit(i)),
        }
    }

    /// Logical equality (`==`): returns a 1-bit result that is `x` if either
    /// operand has any x/z bit, else `1`/`0`.
    pub fn eq_logical(&self, other: &LogicVec) -> LogicVec {
        if self.has_unknown() || other.has_unknown() {
            return LogicVec::x(1);
        }
        let w = self.width.max(other.width);
        let a = self.resize(w, false);
        let b = other.resize(w, false);
        let eq = a.eq_case(&b);
        LogicVec::from_u64(eq as u64, 1)
    }

    /// Logical inequality (`!=`): the 1-bit complement of [`eq_logical`].
    pub fn ne_logical(&self, other: &LogicVec) -> LogicVec {
        self.eq_logical(other).lognot()
    }

    /// Unsigned magnitude comparison. `None` if either operand has any x/z bit.
    fn ucmp(&self, other: &LogicVec) -> Option<core::cmp::Ordering> {
        use core::cmp::Ordering;
        if self.has_unknown() || other.has_unknown() {
            return None;
        }
        let w = self.width.max(other.width);
        let a = self.resize(w, false);
        let b = other.resize(w, false);
        for i in (0..w).rev() {
            let ai = matches!(a.get_bit(i), Bit::One);
            let bi = matches!(b.get_bit(i), Bit::One);
            if ai != bi {
                return Some(if bi {
                    Ordering::Less
                } else {
                    Ordering::Greater
                });
            }
        }
        Some(Ordering::Equal)
    }

    fn rel(&self, other: &LogicVec, f: impl Fn(core::cmp::Ordering) -> bool) -> LogicVec {
        match self.ucmp(other) {
            Some(o) => LogicVec::from_u64(f(o) as u64, 1),
            None => LogicVec::x(1),
        }
    }

    /// Unsigned `<` (1-bit, x if either operand has x/z).
    pub fn ult(&self, other: &LogicVec) -> LogicVec {
        self.rel(other, |c| c == core::cmp::Ordering::Less)
    }
    /// Unsigned `<=`.
    pub fn ule(&self, other: &LogicVec) -> LogicVec {
        self.rel(other, |c| c != core::cmp::Ordering::Greater)
    }
    /// Unsigned `>`.
    pub fn ugt(&self, other: &LogicVec) -> LogicVec {
        self.rel(other, |c| c == core::cmp::Ordering::Greater)
    }
    /// Unsigned `>=`.
    pub fn uge(&self, other: &LogicVec) -> LogicVec {
        self.rel(other, |c| c != core::cmp::Ordering::Less)
    }

    /// Logical negation (`!`): 1-bit, `x` if the operand is unknown with no
    /// known-1 bit.
    pub fn lognot(&self) -> LogicVec {
        match self.reduce_or() {
            Bit::One => LogicVec::from_u64(0, 1),
            Bit::Zero => LogicVec::from_u64(1, 1),
            _ => LogicVec::x(1),
        }
    }

    /// Two's-complement negation (`-a`), computed as `0 - a`.
    pub fn neg(&self) -> LogicVec {
        LogicVec::zero(self.width).sub(self)
    }

    /// A 1-bit vector from a single [`Bit`].
    pub fn from_bit(b: Bit) -> LogicVec {
        match b {
            Bit::Zero => LogicVec::from_u64(0, 1),
            Bit::One => LogicVec::from_u64(1, 1),
            Bit::X | Bit::Z => LogicVec::x(1),
        }
    }

    /// A vector of `width` copies of one four-state bit.
    pub fn filled(bit: Bit, width: u32) -> LogicVec {
        match bit {
            Bit::Zero => LogicVec::zero(width),
            Bit::One => LogicVec::ones(width),
            Bit::X => LogicVec::x(width),
            Bit::Z => LogicVec::z(width),
        }
    }

    // ---------------------------------------------------------------------
    // Bitwise (4-state, per-bit) — computed word-parallel
    // ---------------------------------------------------------------------

    /// Bitwise NOT (`~`): `~0=1`, `~1=0`, `~x=x`, `~z=x`.
    pub fn bitnot(&self) -> LogicVec {
        self.map_word(|a, b| {
            // result 1 where a is logic-0 (a==0,b==0); x where b set.
            let r_a = !a & !b;
            let r_b = b;
            (r_a, r_b)
        })
    }

    /// Bitwise AND (`&`).
    pub fn bitand(&self, rhs: &LogicVec) -> LogicVec {
        self.zip_word(rhs, |aa, ab, ba, bb| {
            let a0 = !aa & !ab;
            let b0 = !ba & !bb;
            let a1 = aa & !ab;
            let b1 = ba & !bb;
            let is0 = a0 | b0;
            let is1 = a1 & b1;
            let isx = !is0 & !is1;
            (is1, isx)
        })
    }

    /// Bitwise OR (`|`).
    pub fn bitor(&self, rhs: &LogicVec) -> LogicVec {
        self.zip_word(rhs, |aa, ab, ba, bb| {
            let a0 = !aa & !ab;
            let b0 = !ba & !bb;
            let a1 = aa & !ab;
            let b1 = ba & !bb;
            let is1 = a1 | b1;
            let is0 = a0 & b0;
            let isx = !is1 & !is0;
            (is1, isx)
        })
    }

    /// Bitwise XOR (`^`). Any unknown input bit yields `x`.
    pub fn bitxor(&self, rhs: &LogicVec) -> LogicVec {
        self.zip_word(rhs, |aa, ab, ba, bb| {
            let unknown = ab | bb;
            let r_a = (aa ^ ba) & !unknown;
            (r_a, unknown)
        })
    }

    // ---------------------------------------------------------------------
    // Reductions (for `if (vec)`, `|vec`, `&vec`, `^vec`)
    // ---------------------------------------------------------------------

    /// Reduction OR (`|vec`): 1 if any known-1 bit, x if any unknown and no
    /// known-1, else 0.
    pub fn reduce_or(&self) -> Bit {
        if self.is_true() {
            Bit::One
        } else if self.has_unknown() {
            Bit::X
        } else {
            Bit::Zero
        }
    }

    /// Reduction AND (`&vec`): 1 if all bits known-1, 0 if any known-0,
    /// else x.
    pub fn reduce_and(&self) -> Bit {
        let mut saw_unknown = false;
        for i in 0..self.width {
            match self.get_bit(i) {
                Bit::Zero => return Bit::Zero,
                Bit::X | Bit::Z => saw_unknown = true,
                Bit::One => {}
            }
        }
        if saw_unknown {
            Bit::X
        } else {
            Bit::One
        }
    }

    /// Reduction XOR (`^vec`): x if any unknown, else parity of 1 bits.
    pub fn reduce_xor(&self) -> Bit {
        if self.has_unknown() {
            return Bit::X;
        }
        let mut parity = 0u32;
        match &self.repr {
            Repr::Small { aval, .. } => parity = aval.count_ones(),
            Repr::Wide { aval, .. } => {
                for w in aval.iter() {
                    parity = parity.wrapping_add(w.count_ones());
                }
            }
        }
        if parity & 1 == 1 {
            Bit::One
        } else {
            Bit::Zero
        }
    }

    // ---------------------------------------------------------------------
    // Arithmetic
    // ---------------------------------------------------------------------

    /// Addition. Per IEEE 1800, if **either** operand has any x/z bit the whole
    /// result is `x`. Result width is `max(self.width, rhs.width)`, truncating
    /// any carry-out beyond it.
    pub fn add(&self, rhs: &LogicVec) -> LogicVec {
        let w = self.width.max(rhs.width);
        if self.has_unknown() || rhs.has_unknown() {
            return LogicVec::x(w);
        }
        if w <= 64 {
            let a = self.to_u64();
            let b = rhs.to_u64();
            return LogicVec::from_u64(a.wrapping_add(b), w);
        }
        self.wide_addsub(rhs, false, w)
    }

    /// Subtraction. Same x/z propagation rule as [`add`](Self::add).
    pub fn sub(&self, rhs: &LogicVec) -> LogicVec {
        let w = self.width.max(rhs.width);
        if self.has_unknown() || rhs.has_unknown() {
            return LogicVec::x(w);
        }
        if w <= 64 {
            let a = self.to_u64();
            let b = rhs.to_u64();
            return LogicVec::from_u64(a.wrapping_sub(b), w);
        }
        self.wide_addsub(rhs, true, w)
    }

    /// Multiplication (low `w` bits). x/z propagation as in [`add`](Self::add).
    /// Currently narrow-only (`w <= 64`); wider widths are deferred until a
    /// test needs them (tracked in the port roadmap).
    pub fn mul(&self, rhs: &LogicVec) -> LogicVec {
        let w = self.width.max(rhs.width);
        if self.has_unknown() || rhs.has_unknown() {
            return LogicVec::x(w);
        }
        assert!(w <= 64, "wide multiply (width {w}) not yet implemented");
        LogicVec::from_u64(self.to_u64().wrapping_mul(rhs.to_u64()), w)
    }

    fn wide_addsub(&self, rhs: &LogicVec, sub: bool, w: u32) -> LogicVec {
        let n = limbs_for(w);
        let mut out = vec![0u64; n];
        let mut carry: u128 = if sub { 1 } else { 0 };
        for (i, slot) in out.iter_mut().enumerate() {
            let a = self.limb_aval(i) as u128;
            let b = if sub {
                (!rhs.limb_aval(i)) as u128
            } else {
                rhs.limb_aval(i) as u128
            };
            let s = a + b + carry;
            *slot = s as u64;
            carry = s >> 64;
        }
        let mut v = LogicVec {
            width: w,
            repr: Repr::Wide {
                aval: out.into_boxed_slice(),
                bval: vec![0u64; n].into_boxed_slice(),
            },
        };
        v.normalize_top();
        v
    }

    #[inline]
    fn limb_aval(&self, i: usize) -> u64 {
        match &self.repr {
            Repr::Small { aval, .. } => {
                if i == 0 {
                    *aval
                } else {
                    0
                }
            }
            Repr::Wide { aval, .. } => aval.get(i).copied().unwrap_or(0),
        }
    }

    // ---------------------------------------------------------------------
    // Shifts (narrow fast path; wide deferred)
    // ---------------------------------------------------------------------

    /// Logical left shift by `amt` bits, keeping `self.width`.
    pub fn shl(&self, amt: u32) -> LogicVec {
        if let Some((a, b)) = self.small() {
            let w = self.width;
            if amt >= w {
                return LogicVec::zero(w);
            }
            let m = word_mask(w);
            return LogicVec::from_planes_small((a << amt) & m, (b << amt) & m, w);
        }
        // Wide: bit-by-bit (correctness over speed; wide shifts are rare).
        let mut out = LogicVec::zero(self.width);
        for i in (0..self.width).rev() {
            if i >= amt {
                out.set_bit(i, self.get_bit(i - amt));
            }
        }
        out
    }

    /// Logical right shift by `amt` bits (zero fill).
    pub fn shr(&self, amt: u32) -> LogicVec {
        if let Some((a, b)) = self.small() {
            let w = self.width;
            if amt >= w {
                return LogicVec::zero(w);
            }
            return LogicVec::from_planes_small(a >> amt, b >> amt, w);
        }
        let mut out = LogicVec::zero(self.width);
        for i in 0..self.width {
            if i + amt < self.width {
                out.set_bit(i, self.get_bit(i + amt));
            }
        }
        out
    }

    // ---------------------------------------------------------------------
    // Slicing / concat
    // ---------------------------------------------------------------------

    /// Part-select `[hi:lo]` (inclusive), returning a `hi-lo+1`-bit vector.
    pub fn slice(&self, hi: u32, lo: u32) -> LogicVec {
        assert!(hi >= lo, "slice hi < lo");
        let w = hi - lo + 1;
        let mut out = LogicVec::zero(w);
        for i in 0..w {
            out.set_bit(i, self.get_bit(lo + i));
        }
        out
    }

    /// Concatenate `{self, rhs}` — `self` occupies the high bits.
    pub fn concat(&self, rhs: &LogicVec) -> LogicVec {
        let w = self.width + rhs.width;
        let mut out = LogicVec::zero(w);
        for i in 0..rhs.width {
            out.set_bit(i, rhs.get_bit(i));
        }
        for i in 0..self.width {
            out.set_bit(rhs.width + i, self.get_bit(i));
        }
        out
    }

    // ---------------------------------------------------------------------
    // Word-parallel helpers for per-bit ops
    // ---------------------------------------------------------------------

    fn map_word<F: Fn(u64, u64) -> (u64, u64)>(&self, f: F) -> LogicVec {
        match &self.repr {
            Repr::Small { aval, bval } => {
                let (ra, rb) = f(*aval, *bval);
                let m = word_mask(self.width);
                LogicVec {
                    width: self.width,
                    repr: Repr::Small {
                        aval: ra & m,
                        bval: rb & m,
                    },
                }
            }
            Repr::Wide { aval, bval } => {
                let n = aval.len();
                let mut ra = vec![0u64; n];
                let mut rb = vec![0u64; n];
                for i in 0..n {
                    let (a, b) = f(aval[i], bval[i]);
                    ra[i] = a;
                    rb[i] = b;
                }
                let mut v = LogicVec {
                    width: self.width,
                    repr: Repr::Wide {
                        aval: ra.into_boxed_slice(),
                        bval: rb.into_boxed_slice(),
                    },
                };
                v.normalize_top();
                v
            }
        }
    }

    fn zip_word<F: Fn(u64, u64, u64, u64) -> (u64, u64)>(&self, rhs: &LogicVec, f: F) -> LogicVec {
        let w = self.width.max(rhs.width);
        match (&self.repr, &rhs.repr) {
            (Repr::Small { aval: a1, bval: b1 }, Repr::Small { aval: a2, bval: b2 }) if w <= 64 => {
                let (ra, rb) = f(*a1, *b1, *a2, *b2);
                let m = word_mask(w);
                LogicVec {
                    width: w,
                    repr: Repr::Small {
                        aval: ra & m,
                        bval: rb & m,
                    },
                }
            }
            _ => {
                let n = limbs_for(w);
                let mut ra = vec![0u64; n];
                let mut rb = vec![0u64; n];
                for i in 0..n {
                    let (a, b) = f(
                        self.limb_aval(i),
                        self.limb_bval(i),
                        rhs.limb_aval(i),
                        rhs.limb_bval(i),
                    );
                    ra[i] = a;
                    rb[i] = b;
                }
                let mut v = LogicVec {
                    width: w,
                    repr: Repr::Wide {
                        aval: ra.into_boxed_slice(),
                        bval: rb.into_boxed_slice(),
                    },
                };
                v.normalize_top();
                v
            }
        }
    }

    #[inline]
    fn limb_bval(&self, i: usize) -> u64 {
        match &self.repr {
            Repr::Small { bval, .. } => {
                if i == 0 {
                    *bval
                } else {
                    0
                }
            }
            Repr::Wide { bval, .. } => bval.get(i).copied().unwrap_or(0),
        }
    }
}

/// Zero out the bits above `width` in the top limb of a limb array pair.
fn mask_top(width: u32, aval: &mut [u64], bval: &mut [u64]) {
    let top_bits = width % 64;
    if top_bits != 0 {
        let m = (1u64 << top_bits) - 1;
        if let Some(last) = aval.last_mut() {
            *last &= m;
        }
        if let Some(last) = bval.last_mut() {
            *last &= m;
        }
    }
}

impl PartialEq for LogicVec {
    /// `==` on [`LogicVec`] is **case** (bit-exact) equality, so x/z compare
    /// equal to themselves. Use [`eq_logical`](LogicVec::eq_logical) for the SV
    /// `==` operator semantics.
    fn eq(&self, other: &Self) -> bool {
        self.eq_case(other)
    }
}
impl Eq for LogicVec {}

impl fmt::Display for LogicVec {
    /// Renders as `width'b<bits>` (MSB first), e.g. `4'b10x1`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}'b", self.width)?;
        for i in (0..self.width).rev() {
            write!(f, "{}", self.get_bit(i).to_char())?;
        }
        Ok(())
    }
}

impl fmt::Debug for LogicVec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicVec({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_selects_and_merges_four_state_values() {
        let when_true = LogicVec::parse_sized("4'b10z1").unwrap();
        let when_false = LogicVec::parse_sized("4'b10z0").unwrap();
        assert_eq!(
            LogicVec::conditional(&LogicVec::from_u64(1, 1), &when_true, &when_false),
            when_true
        );
        assert_eq!(
            LogicVec::conditional(&LogicVec::zero(1), &when_true, &when_false),
            when_false
        );
        let merged = LogicVec::conditional(&LogicVec::x(1), &when_true, &when_false);
        assert_eq!(merged.get_bit(0), Bit::X);
        assert_eq!(merged.get_bit(1), Bit::Z);
        assert_eq!(merged.get_bit(2), Bit::Zero);
        assert_eq!(merged.get_bit(3), Bit::One);

        let equal_unknown = LogicVec::conditional(
            &LogicVec::z(2),
            &LogicVec::parse_sized("2'bxz").unwrap(),
            &LogicVec::parse_sized("2'bxz").unwrap(),
        );
        assert_eq!(equal_unknown.get_bit(0), Bit::Z);
        assert_eq!(equal_unknown.get_bit(1), Bit::X);

        let unequal_width = LogicVec::conditional(
            &LogicVec::x(1),
            &LogicVec::from_u64(1, 1),
            &LogicVec::from_u64(0b1010, 4),
        );
        assert_eq!(unequal_width.width(), 4);
        assert_eq!(unequal_width.get_bit(0), Bit::X);
        assert_eq!(unequal_width.get_bit(1), Bit::X);
        assert_eq!(unequal_width.get_bit(2), Bit::Zero);
        assert_eq!(unequal_width.get_bit(3), Bit::X);

        let partly_unknown_true = LogicVec::parse_sized("2'bx1").unwrap();
        assert_eq!(
            LogicVec::conditional(&partly_unknown_true, &when_true, &when_false),
            when_true
        );
    }

    #[test]
    fn logicvec_stays_small() {
        // The core datum is cloned/stored on every net touch and lives in every
        // interpreter register, so its size directly drives memory traffic and
        // cache behavior. Guard against accidental bloat. Known further lever:
        // boxing the rare `Wide` variant would roughly halve this (deferred —
        // profiling shows clone traffic is not the current bottleneck).
        let sz = std::mem::size_of::<LogicVec>();
        assert!(sz <= 48, "LogicVec grew to {sz} bytes (was <= 48)");
    }

    #[test]
    fn zero_and_known() {
        let v = LogicVec::zero(8);
        assert_eq!(v.width(), 8);
        assert!(v.is_known());
        assert!(!v.is_true());
        assert_eq!(v.to_u64(), 0);
    }

    #[test]
    fn from_u64_round_trip() {
        let v = LogicVec::from_u64(0xABCD, 16);
        assert_eq!(v.to_u64(), 0xABCD);
        assert_eq!(v.try_to_u64(), Some(0xABCD));
        // truncation
        let t = LogicVec::from_u64(0x1_2345, 16);
        assert_eq!(t.to_u64(), 0x2345);
    }

    #[test]
    fn x_z_states() {
        let x = LogicVec::x(4);
        assert!(x.has_unknown());
        assert!(!x.is_true());
        assert_eq!(x.try_to_u64(), None);
        assert_eq!(x.get_bit(0), Bit::X);
        let z = LogicVec::z(4);
        assert_eq!(z.get_bit(3), Bit::Z);
        assert!(z.has_unknown());
    }

    #[test]
    fn parse_sized_literals() {
        assert_eq!(LogicVec::parse_sized("4'b1010").unwrap().to_u64(), 0b1010);
        assert_eq!(LogicVec::parse_sized("8'hF0").unwrap().to_u64(), 0xF0);
        assert_eq!(LogicVec::parse_sized("32'd42").unwrap().to_u64(), 42);
        assert_eq!(LogicVec::parse_sized("6'o52").unwrap().to_u64(), 0o52);
        let v = LogicVec::parse_sized("4'b10x1").unwrap();
        assert_eq!(v.get_bit(0), Bit::One);
        assert_eq!(v.get_bit(1), Bit::X);
        assert_eq!(v.get_bit(2), Bit::Zero);
        assert_eq!(v.get_bit(3), Bit::One);
        assert!(v.has_unknown());
        let z = LogicVec::parse_sized("1'bz").unwrap();
        assert_eq!(z.get_bit(0), Bit::Z);
        assert_eq!(
            LogicVec::parse_sized("8'b_1010_0101").unwrap().to_u64(),
            0xA5
        );
    }

    #[test]
    fn display_format() {
        assert_eq!(
            LogicVec::parse_sized("4'b10x1").unwrap().to_string(),
            "4'b10x1"
        );
        assert_eq!(LogicVec::from_u64(0xA, 4).to_string(), "4'b1010");
        assert_eq!(LogicVec::z(2).to_string(), "2'bzz");
    }

    #[test]
    fn bit_set_get() {
        let mut v = LogicVec::zero(8);
        v.set_bit(3, Bit::One);
        v.set_bit(5, Bit::X);
        assert_eq!(v.get_bit(3), Bit::One);
        assert_eq!(v.get_bit(5), Bit::X);
        assert_eq!(v.get_bit(0), Bit::Zero);
        assert!(v.has_unknown());
    }

    #[test]
    fn arithmetic_known() {
        let a = LogicVec::from_u64(40, 32);
        let b = LogicVec::from_u64(2, 32);
        assert_eq!(a.add(&b).to_u64(), 42);
        assert_eq!(a.sub(&b).to_u64(), 38);
        assert_eq!(a.mul(&b).to_u64(), 80);
    }

    #[test]
    fn arithmetic_truncates() {
        let a = LogicVec::from_u64(0xFFFF_FFFF, 32);
        let one = LogicVec::from_u64(1, 32);
        // 0xFFFFFFFF + 1 wraps to 0 in 32 bits
        assert_eq!(a.add(&one).to_u64(), 0);
    }

    #[test]
    fn arithmetic_x_propagates() {
        let a = LogicVec::x(8);
        let b = LogicVec::from_u64(1, 8);
        assert!(a.add(&b).has_unknown());
        assert!(b.add(&a).has_unknown());
    }

    #[test]
    fn bitwise_4state() {
        let a = LogicVec::parse_sized("4'b1100").unwrap();
        let b = LogicVec::parse_sized("4'b1010").unwrap();
        assert_eq!(a.bitand(&b), LogicVec::parse_sized("4'b1000").unwrap());
        assert_eq!(a.bitor(&b), LogicVec::parse_sized("4'b1110").unwrap());
        assert_eq!(a.bitxor(&b), LogicVec::parse_sized("4'b0110").unwrap());
        assert_eq!(a.bitnot(), LogicVec::parse_sized("4'b0011").unwrap());
    }

    #[test]
    fn bitwise_x_rules() {
        // 1 & x = x ; 0 & x = 0
        let one = LogicVec::parse_sized("1'b1").unwrap();
        let zero = LogicVec::parse_sized("1'b0").unwrap();
        let x = LogicVec::x(1);
        assert_eq!(one.bitand(&x).get_bit(0), Bit::X);
        assert_eq!(zero.bitand(&x).get_bit(0), Bit::Zero);
        // 1 | x = 1 ; 0 | x = x
        assert_eq!(one.bitor(&x).get_bit(0), Bit::One);
        assert_eq!(zero.bitor(&x).get_bit(0), Bit::X);
        // ~x = x
        assert_eq!(x.bitnot().get_bit(0), Bit::X);
        // anything xor x = x
        assert_eq!(one.bitxor(&x).get_bit(0), Bit::X);
    }

    #[test]
    fn reductions() {
        assert_eq!(
            LogicVec::parse_sized("4'b0010").unwrap().reduce_or(),
            Bit::One
        );
        assert_eq!(LogicVec::zero(4).reduce_or(), Bit::Zero);
        assert_eq!(LogicVec::x(4).reduce_or(), Bit::X);
        assert_eq!(LogicVec::ones(4).reduce_and(), Bit::One);
        assert_eq!(
            LogicVec::parse_sized("4'b1110").unwrap().reduce_and(),
            Bit::Zero
        );
        assert_eq!(
            LogicVec::parse_sized("4'b1011").unwrap().reduce_xor(),
            Bit::One
        );
        assert_eq!(
            LogicVec::parse_sized("4'b1010").unwrap().reduce_xor(),
            Bit::Zero
        );
    }

    #[test]
    fn equality_case_vs_logical() {
        let a = LogicVec::from_u64(5, 8);
        let b = LogicVec::from_u64(5, 8);
        assert!(a.eq_case(&b));
        assert_eq!(a.eq_logical(&b).get_bit(0), Bit::One);
        let x1 = LogicVec::x(8);
        let x2 = LogicVec::x(8);
        assert!(x1.eq_case(&x2)); // === : x===x is true
        assert_eq!(x1.eq_logical(&x2).get_bit(0), Bit::X); // == : x==x is x
    }

    #[test]
    fn shifts() {
        let v = LogicVec::from_u64(0b0011, 4);
        assert_eq!(v.shl(1).to_u64(), 0b0110);
        assert_eq!(v.shl(4).to_u64(), 0);
        assert_eq!(v.shr(1).to_u64(), 0b0001);
        let top = LogicVec::from_u64(0b1000, 4);
        assert_eq!(top.shl(1).to_u64(), 0); // carry out dropped
    }

    #[test]
    fn slice_and_concat() {
        let v = LogicVec::from_u64(0xAB, 8);
        assert_eq!(v.slice(7, 4).to_u64(), 0xA);
        assert_eq!(v.slice(3, 0).to_u64(), 0xB);
        let hi = LogicVec::from_u64(0xA, 4);
        let lo = LogicVec::from_u64(0xB, 4);
        assert_eq!(hi.concat(&lo).to_u64(), 0xAB);
    }

    #[test]
    fn resize_zero_and_sign_extend() {
        let v = LogicVec::from_u64(0xF, 4);
        assert_eq!(v.resize(8, false).to_u64(), 0x0F);
        assert_eq!(v.resize(8, true).to_u64(), 0xFF); // sign extend (top bit 1)
        let small = LogicVec::from_u64(0x3, 4);
        assert_eq!(small.resize(8, true).to_u64(), 0x03); // top bit 0
        assert_eq!(v.resize(2, false).to_u64(), 0x3); // truncate
    }

    #[test]
    fn signed_to_i64() {
        let v = LogicVec::from_i64(-1, 8);
        assert_eq!(v.to_u64(), 0xFF);
        assert_eq!(v.to_i64(), -1);
        let p = LogicVec::from_i64(5, 8);
        assert_eq!(p.to_i64(), 5);
    }

    // ----- Wide (> 64 bit) coverage -----

    #[test]
    fn wide_construct_and_bits() {
        let mut v = LogicVec::zero(128);
        assert_eq!(v.width(), 128);
        assert!(v.is_known());
        v.set_bit(100, Bit::One);
        v.set_bit(64, Bit::X);
        assert_eq!(v.get_bit(100), Bit::One);
        assert_eq!(v.get_bit(64), Bit::X);
        assert!(v.has_unknown());
        assert!(v.is_true()); // bit 100 is a known 1
    }

    #[test]
    fn wide_ones_and_mask_top() {
        let v = LogicVec::ones(100);
        // every bit 0..100 is 1, nothing above
        assert_eq!(v.get_bit(99), Bit::One);
        assert_eq!(v.get_bit(100), Bit::Zero);
        assert!(v.reduce_and() == Bit::One);
    }

    #[test]
    fn wide_bitwise() {
        let a = LogicVec::ones(96);
        let b = LogicVec::zero(96);
        assert_eq!(a.bitand(&b), LogicVec::zero(96));
        assert_eq!(a.bitor(&b), LogicVec::ones(96));
        assert_eq!(a.bitnot(), LogicVec::zero(96));
    }

    #[test]
    fn wide_add_carry_across_limb() {
        // 0xFFFF...F (64 ones) + 1 in a 96-bit vector -> bit 64 set.
        let mut a = LogicVec::zero(96);
        for i in 0..64 {
            a.set_bit(i, Bit::One);
        }
        let one = LogicVec::from_u64(1, 96);
        let s = a.add(&one);
        assert_eq!(s.get_bit(64), Bit::One);
        for i in 0..64 {
            assert_eq!(s.get_bit(i), Bit::Zero);
        }
    }

    #[test]
    fn width_64_boundary() {
        let v = LogicVec::from_u64(u64::MAX, 64);
        assert_eq!(v.to_u64(), u64::MAX);
        assert!(v.is_known());
        let s = v.add(&LogicVec::from_u64(1, 64));
        assert_eq!(s.to_u64(), 0); // wraps
    }
}
