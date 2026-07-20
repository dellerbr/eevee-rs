//! Runtime values held in interpreter registers / slots.
//!
//! # Speed & the JIT seam
//!
//! For now a slot is a single [`Value`] enum. The hot path (4-state RTL) only
//! ever touches the [`Value::Logic`] variant, and that variant is the cheap one
//! ([`LogicVec`] is inline for widths <= 64). Tagged dispatch on the variant is
//! branch-predictable and was chosen for v0 simplicity.
//!
//! Two known speed levers, deferred until the perf log says they're needed
//! (measure, don't guess):
//! * **Typed slot banks** — separate `Vec<LogicVec>` / `Vec<f64>` / `Vec<Handle>`
//!   arrays indexed by a typed operand, so the hot loop has zero tag checks and
//!   the register file is cache-dense. This is also what a JIT wants (each slot
//!   has a static type → it maps to registers/stack directly).
//! * **Narrow-`LogicVec` shrink** — box the rare wide representation so a slot is
//!   ~24 bytes instead of ~40.
//!
//! The IR (see [`crate::inst`]) is already typed enough that both changes are
//! local to this module + the interpreter; they do not reshape the IR.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use eevee_core::LogicVec;

/// A heap-allocated class instance: its class id plus its field values (by slot
/// index). SV class handles are references, so a [`Value::Obj`] holds an
/// `Rc<RefCell<…>>` — cloning a handle shares the object (correct aliasing), and
/// methods mutate fields through the `RefCell`.
#[derive(Debug)]
pub struct ObjData {
    pub class: u32,
    pub fields: Vec<Value>,
}

/// Key of an associative array. SV assoc arrays are keyed by integers or
/// strings; we keep them ordered (`BTreeMap`) so `first`/`next`/`last`/`prev`
/// iterate deterministically.
#[derive(Clone, Debug)]
pub enum AssocKey {
    Int(i64),
    Str(Rc<str>),
    Obj(Rc<RefCell<ObjData>>),
    Null,
}

impl AssocKey {
    fn rank(&self) -> u8 {
        match self {
            AssocKey::Int(_) => 0,
            AssocKey::Str(_) => 1,
            AssocKey::Obj(_) => 2,
            AssocKey::Null => 3,
        }
    }
}

impl PartialEq for AssocKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (AssocKey::Int(left), AssocKey::Int(right)) => left == right,
            (AssocKey::Str(left), AssocKey::Str(right)) => left == right,
            (AssocKey::Obj(left), AssocKey::Obj(right)) => Rc::ptr_eq(left, right),
            (AssocKey::Null, AssocKey::Null) => true,
            _ => false,
        }
    }
}

impl Eq for AssocKey {}

impl PartialOrd for AssocKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AssocKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (AssocKey::Int(left), AssocKey::Int(right)) => left.cmp(right),
            (AssocKey::Str(left), AssocKey::Str(right)) => left.cmp(right),
            (AssocKey::Obj(left), AssocKey::Obj(right)) => {
                (Rc::as_ptr(left) as usize).cmp(&(Rc::as_ptr(right) as usize))
            }
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl Hash for AssocKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            AssocKey::Int(value) => value.hash(state),
            AssocKey::Str(value) => value.hash(state),
            AssocKey::Obj(value) => (Rc::as_ptr(value) as usize).hash(state),
            AssocKey::Null => {}
        }
    }
}

/// A value living in an interpreter register or a design slot.
#[derive(Clone, Debug, Default)]
pub enum Value {
    /// 4-state packed vector — the overwhelmingly common case.
    Logic(LogicVec),
    /// IEEE-754 double (`real`/`shortreal`).
    Real(f64),
    /// SV `string`.
    Str(Rc<str>),
    /// A class instance handle (shared, mutable).
    Obj(Rc<RefCell<ObjData>>),
    /// A queue / dynamic array / unpacked array (shared, mutable). All three
    /// are list-backed in SV semantics.
    Queue(Rc<RefCell<Vec<Value>>>),
    /// An associative array (shared, mutable).
    Assoc(Rc<RefCell<BTreeMap<AssocKey, Value>>>),
    /// Null handle / unset slot.
    #[default]
    Null,
}

impl Value {
    /// Borrow as a [`LogicVec`]. A `Null` slot reads as numeric zero — this is
    /// the SV default for an uninitialized / missing associative-array element
    /// of an integral type (`int m[key]; ... m[absent]++`). Other non-logic
    /// kinds (handles, strings, collections) are genuine IR type errors.
    #[inline]
    pub fn as_logic(&self) -> &LogicVec {
        use std::sync::OnceLock;
        static ZERO: OnceLock<LogicVec> = OnceLock::new();
        match self {
            Value::Logic(v) => v,
            Value::Null => ZERO.get_or_init(|| LogicVec::zero(32)),
            other => panic!("IR type error: expected logic value, found {other:?}"),
        }
    }

    /// Consume into a [`LogicVec`]. Same `Null`-as-zero rule as
    /// [`as_logic`](Self::as_logic).
    #[inline]
    pub fn into_logic(self) -> LogicVec {
        match self {
            Value::Logic(v) => v,
            Value::Null => LogicVec::zero(32),
            other => panic!("IR type error: expected logic value, found {other:?}"),
        }
    }

    /// A fresh, empty queue / dynamic array.
    #[inline]
    pub fn new_queue() -> Value {
        Value::Queue(Rc::new(RefCell::new(Vec::new())))
    }

    /// A fresh, empty associative array.
    #[inline]
    pub fn new_assoc() -> Value {
        Value::Assoc(Rc::new(RefCell::new(BTreeMap::new())))
    }
}

impl From<LogicVec> for Value {
    #[inline]
    fn from(v: LogicVec) -> Self {
        Value::Logic(v)
    }
}
