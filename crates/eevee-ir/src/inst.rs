//! The register-based IR: instructions, programs, and a small builder.
//!
//! # Why this shape
//!
//! * **Register-based** (operands are register indices, results name a `dst`):
//!   fewer dispatches per SV expression than a stack machine, and it maps
//!   cleanly onto a JIT's value model later.
//! * **`Inst` is `Copy` and fixed-size.** Variable-length payloads (a
//!   `wait`'s net read-set, later a `$display` arg list) live in side tables
//!   (`netlists`, `consts`) referenced by index, so the code stream stays a
//!   dense, cache-friendly array — and a JIT can lower it instruction-by-
//!   instruction without chasing pointers.
//! * **Names are already resolved.** Registers are slot indices and nets are
//!   [`NetId`]s, both fixed at elaboration time — there is no per-access name
//!   resolution in the hot loop (the central Python perf fix).
//!
//! Timing instructions (`Delay`, `WaitEdge`, `WaitCond`) are the process
//! suspension points: the interpreter returns the matching
//! [`eevee_sched::Wait`] with the PC saved just past them.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use eevee_core::LogicVec;
use eevee_sched::{DriverId, EdgeKind, ForkJoin, NetId};

use crate::value::Value;

/// Register (slot) index within a process frame.
pub type Reg = u32;
/// Index into [`Program::consts`].
pub type ConstId = u32;
/// Index into [`Program::netlists`].
pub type NetListId = u32;
/// A code address (index into [`Program::code`]).
pub type CodeAddr = u32;
/// Index into [`Program::arglists`].
pub type ArgListId = u32;
/// Index into a process's function table (a callable function/task).
pub type FuncId = u32;
/// Index into the design's class table.
pub type ClassId = u32;
/// Index into [`Program::fork_groups`].
pub type ForkGroupId = u32;

/// Runtime passing mode for one callable argument register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgMode {
    Input,
    Output,
    Inout,
    Ref,
}

impl ArgMode {
    #[inline]
    pub fn copies_out(self) -> bool {
        matches!(self, Self::Output | Self::Inout | Self::Ref)
    }
}

/// A single IR instruction. `Copy` and small by construction.
#[derive(Clone, Copy, Debug)]
pub enum Inst {
    /// `dst = consts[k]`.
    LoadConst { dst: Reg, k: ConstId },
    /// `dst = src`.
    Mov { dst: Reg, src: Reg },
    /// `dst = src` at an SV assignment boundary (arrays copy by value).
    Assign { dst: Reg, src: Reg },
    /// Queue `dst <= src` for the procedural-variable NBA region.
    NbaAssign { dst: Reg, src: Reg },

    /// `dst = <value of net>` (read the net's current value).
    NetRead { dst: Reg, net: NetId },

    // --- 4-state ALU (operands and result are logic) ---
    /// `dst = ~a`.
    Not { dst: Reg, a: Reg },
    /// `dst = a + b`.
    Add { dst: Reg, a: Reg, b: Reg },
    /// `dst = a - b`.
    Sub { dst: Reg, a: Reg, b: Reg },
    /// `dst = a * b` (low `width` bits).
    Mul { dst: Reg, a: Reg, b: Reg },
    /// `dst = a & b`.
    And { dst: Reg, a: Reg, b: Reg },
    /// `dst = a | b`.
    Or { dst: Reg, a: Reg, b: Reg },
    /// `dst = a ^ b`.
    Xor { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a == b)` — 1-bit logical equality (x if either operand has x/z).
    Eq { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a != b)` — 1-bit logical inequality.
    Neq { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a < b)` — 1-bit unsigned less-than.
    Lt { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a <= b)` — 1-bit unsigned less-or-equal.
    Le { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a > b)` — 1-bit unsigned greater-than.
    Gt { dst: Reg, a: Reg, b: Reg },
    /// `dst = (a >= b)` — 1-bit unsigned greater-or-equal.
    Ge { dst: Reg, a: Reg, b: Reg },
    /// `dst = a << b` (logical shift left by `b`'s value).
    Shl { dst: Reg, a: Reg, b: Reg },
    /// `dst = a >> b` (logical shift right by `b`'s value).
    Shr { dst: Reg, a: Reg, b: Reg },
    /// `dst = a && b` — 1-bit logical AND (both operands reduced to truthy).
    LogAnd { dst: Reg, a: Reg, b: Reg },
    /// `dst = a || b` — 1-bit logical OR (either operand truthy).
    LogOr { dst: Reg, a: Reg, b: Reg },
    /// `dst = !a` — 1-bit logical negation.
    LogNot { dst: Reg, a: Reg },
    /// `dst = -a` — two's-complement negation.
    Neg { dst: Reg, a: Reg },
    /// `dst = &a` — reduction AND (1 bit).
    ReduceAnd { dst: Reg, a: Reg },
    /// `dst = |a` — reduction OR (1 bit).
    ReduceOr { dst: Reg, a: Reg },
    /// `dst = ^a` — reduction XOR (1 bit).
    ReduceXor { dst: Reg, a: Reg },

    // --- net updates ---
    /// Blocking assign: write the net **now** (Active region).
    BlockingWrite { net: NetId, src: Reg },
    /// Non-blocking assign: schedule the net update for the NBA region.
    NbaWrite { net: NetId, src: Reg },
    /// Update one continuous driver; the scheduler resolves all drivers.
    DriveNet { driver: DriverId, src: Reg },
    /// Schedule an inertial continuous-driver update after `delay_fs`.
    ScheduleDrive {
        driver: DriverId,
        src: Reg,
        delay_fs: u64,
    },

    // --- timing controls = process suspension points ---
    /// `#fs` — suspend for `fs` femtoseconds.
    Delay { fs: u64 },
    /// `@(edge net)` — suspend until the edge fires.
    WaitEdge { net: NetId, edge: EdgeKind },
    /// `wait(cond)` body: suspend until any net in `netlists[nets]` changes.
    /// The producing code re-evaluates the condition after the wakeup (a
    /// backward branch), so this is event-driven, never polled.
    WaitCond { nets: NetListId },
    /// Suspend until runtime state changes, then re-evaluate a previously
    /// emitted `wait(cond)` condition. Used for class fields, statics, and
    /// collections whose mutation sources are not kernel nets.
    WaitRuntime,
    /// `@(value)` for a non-net expression.
    WaitChange { value: Reg },
    /// Suspend until the named-event value in `event` is triggered.
    WaitEvent { event: Reg },

    // --- control flow ---
    /// Unconditional jump.
    Jump { target: CodeAddr },
    /// Jump if `cond` is **not** truthy (SV `is_true`).
    BranchFalse { cond: Reg, target: CodeAddr },
    /// Jump if `cond` is truthy.
    BranchTrue { cond: Reg, target: CodeAddr },
    /// Jump when the caller supplied formal register `arg`. This lets a
    /// callable evaluate a default expression only for an omitted argument;
    /// an explicitly supplied `null` remains distinguishable from omission.
    BranchArgProvided { arg: Reg, target: CodeAddr },

    /// `$display(fmt, args...)`: format `consts[fmt]` (a string) with the
    /// register values in `arglists[args]` and emit a line to the kernel.
    Display { fmt: ConstId, args: ArgListId },
    /// Call an imported DPI-C host function named by `consts[name]`.
    DpiCall {
        dst: Reg,
        name: ConstId,
        args: ArgListId,
    },

    /// Call function `func` (an index into the process's function table) with
    /// the register values in `arglists[args]`, placing the returned value
    /// into `ret`. The callee's formals are its registers `0..n_args`.
    Call {
        func: FuncId,
        args: ArgListId,
        ret: Reg,
    },
    /// Virtual method call: resolve the `FuncId` from `obj`'s runtime class
    /// vtable at `vslot`, then call it (arg 0 is `obj`/`this`).
    CallVirtual {
        obj: Reg,
        vslot: u32,
        args: ArgListId,
        ret: Reg,
    },
    /// Return a value from the current function/task frame to the caller.
    Return { value: Reg },
    /// Return with no value (void function / task).
    ReturnVoid,

    /// Allocate a class instance of `class` (fields default-initialized) and
    /// store the handle in `dst`.
    New { dst: Reg, class: ClassId },
    /// Allocate a fresh IEEE named-event identity.
    NewEvent { dst: Reg },
    /// Trigger the named event held in `event`.
    TriggerEvent { event: Reg },
    /// `dst = $cast(<target class>, src)`: report whether `src` is null or its
    /// runtime class derives from `class`. Destination assignment is emitted
    /// separately so a failed cast leaves the original handle unchanged.
    ClassCast { dst: Reg, src: Reg, class: ClassId },
    /// `dst = obj.fields[slot]` (object field read).
    GetField { dst: Reg, obj: Reg, slot: u32 },
    /// `obj.fields[slot] = src` (object field write).
    SetField { obj: Reg, slot: u32, src: Reg },

    /// `dst = ` the value of static field `id` (shared class storage).
    StaticGet { dst: Reg, id: u32 },
    /// `static[id] = src` (write a static class field).
    StaticSet { id: u32, src: Reg },

    /// `dst = ` a fresh empty queue / dynamic array.
    NewQueue { dst: Reg },
    /// `dst = ` a fresh empty associative array.
    NewAssoc { dst: Reg },
    /// `dst = base[idx]` — element read of a queue/array (int index) or an
    /// associative array (int/string key), or a packed bit-select.
    IndexGet { dst: Reg, base: Reg, idx: Reg },
    /// `dst = base[left:right]` — packed part-select.
    PartSelect {
        dst: Reg,
        base: Reg,
        left: Reg,
        right: Reg,
    },
    /// `base[idx] = src` — element write (auto-grows a queue/array).
    IndexSet { base: Reg, idx: Reg, src: Reg },
    /// A built-in queue/array/assoc method call (`push_back`, `size`,
    /// `exists`, ...). `dst` receives the result (or `Null`/0 for void ones).
    CollMethod {
        dst: Reg,
        base: Reg,
        op: CollOp,
        args: ArgListId,
    },

    /// `dst = {a, b, c}` — packed concatenation unless an operand is a string.
    Concat { dst: Reg, args: ArgListId },
    /// `dst = ` the current simulation time as a 64-bit value (`$time`/
    /// `$realtime`).
    SimTime { dst: Reg },
    /// `dst = ` a formatted string (`$sformatf`/`$swrite`/`itoa`): format
    /// `consts[fmt]` with `arglists[args]`, producing a `Str`.
    Format {
        dst: Reg,
        fmt: ConstId,
        args: ArgListId,
    },
    /// `dst = ` the enum member name for `src`'s value, via the value->name
    /// table `enum_tables[table]`; falls back to the decimal value.
    EnumName { dst: Reg, src: Reg, table: u32 },

    /// `dst = src.len()` — number of characters in the string `src`.
    StringLen { dst: Reg, src: Reg },
    /// `dst = src.substr(lo, hi)` — characters from index `lo` to `hi` (inclusive).
    StringSub {
        dst: Reg,
        src: Reg,
        lo: Reg,
        hi: Reg,
    },
    /// `dst = src[idx]` — byte value (8-bit) of the character at index `idx`.
    StringIndex { dst: Reg, src: Reg, idx: Reg },
    /// `dst = src.toupper()` — uppercase copy of the string.
    StringToUpper { dst: Reg, src: Reg },
    /// `dst = src.tolower()` — lowercase copy of the string.
    StringToLower { dst: Reg, src: Reg },
    /// `dst = src.atoi()` — integer value parsed from the string.
    StringAtoi { dst: Reg, src: Reg },

    /// `fork ... join/join_any/join_none` (LRM 9.3.2): spawn each program in
    /// `fork_groups[group]` (indices into `Program::forks`) as an independent
    /// concurrent process. Each spawned process's register 0 is seeded with
    /// the *current* frame's register 0 (`this`, by the same reg0-is-this
    /// convention `gen_callable` uses) when `forks_want_this[i]` is set, so a
    /// forked method call sees the same object the parent method did. Per
    /// `join`, execution of the current frame parks here until the children
    /// satisfy `join`'s completion rule (see [`eevee_sched::Wait::Fork`]).
    Fork { group: ForkGroupId, join: ForkJoin },

    /// End the process (`Wait::Finished`).
    Finish,
}

/// A built-in queue / dynamic-array / associative-array method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollOp {
    PushBack,
    PushFront,
    PopBack,
    PopFront,
    Size,
    Insert,
    Delete,
    Exists,
    Num,
    First,
    Last,
    Next,
    Prev,
}

/// A compiled procedural block: a code stream plus its side tables. Shared
/// (`Rc`) across all instances of the same `always`/`initial` block; per-
/// instance mutable state (PC, registers) lives in the interpreter.
#[derive(Clone, Debug, Default)]
pub struct Program {
    /// The instruction stream.
    pub code: Vec<Inst>,
    /// Constant pool (logic/real/string literals).
    pub consts: Vec<Value>,
    /// Net read-sets for `WaitCond` (and future multi-signal sensitivity).
    pub netlists: Vec<Box<[NetId]>>,
    /// Argument register lists for `Display` (and future calls).
    pub arglists: Vec<Box<[Reg]>>,
    /// Programs compiled for `fork` branches, referenced by `fork_groups`.
    pub forks: Vec<Rc<Program>>,
    /// Per-branch lexical captures as `(child register, enclosing register)`.
    /// Values are sampled when the fork executes; object and collection
    /// handles retain their shared identity in the child process.
    pub fork_captures: Vec<Box<[(Reg, Reg)]>>,
    /// Groups of branch indices (into `forks`), one group per `fork`
    /// statement, referenced by `Inst::Fork::group`.
    pub fork_groups: Vec<Box<[u32]>>,
    /// Number of registers (frame size).
    pub n_regs: u32,
    /// Passing mode of each leading formal register, including an implicit
    /// input `this` at index 0 for methods.
    pub arg_modes: Box<[ArgMode]>,
    /// Human-readable label (debug/trace/VCD).
    pub label: String,
}

/// A class definition: its name, the default value of each field (used to
/// initialize a fresh instance on [`Inst::New`]), and its virtual-method table
/// (vslot -> `FuncId`, resolved by [`Inst::CallVirtual`]).
#[derive(Debug)]
pub struct ClassDef {
    pub name: String,
    /// Immediate base class, used for runtime assignment compatibility and
    /// dynamic `$cast` checks.
    pub base: Option<ClassId>,
    pub field_defaults: Box<[Value]>,
    pub vtable: Box<[FuncId]>,
    /// Collection-typed fields `(slot, is_assoc)` — initialized to a *fresh*
    /// queue/assoc on every [`Inst::New`] so instances never share storage.
    pub coll_fields: Box<[(u32, bool)]>,
    /// Event-typed field slots, initialized to a fresh identity per instance.
    pub event_fields: Box<[u32]>,
    /// Struct-typed fields `(slot, struct_class_id)` — an unpacked
    /// `typedef struct {...}` field (modeled as a no-method class, see
    /// `ClassId`/`is_struct` on the elaborator side) is fully default-
    /// constructed on every [`Inst::New`] via a *fresh* recursive
    /// [`ClassId`] instance, rather than staying a null handle awaiting an
    /// explicit `new()` the way a real class-typed field does.
    pub struct_fields: Box<[(u32, ClassId)]>,
}

type DpiFunction = dyn Fn(&mut [Value]) -> Value;

/// Host bindings for imported DPI-C functions.
#[derive(Default)]
pub struct DpiRegistry {
    functions: HashMap<String, Rc<DpiFunction>>,
}

impl DpiRegistry {
    pub fn register(
        &mut self,
        name: impl Into<String>,
        function: impl Fn(&mut [Value]) -> Value + 'static,
    ) {
        self.functions.insert(name.into(), Rc::new(function));
    }

    pub fn call(&self, name: &str, args: &mut [Value]) -> Value {
        self.functions
            .get(name)
            .unwrap_or_else(|| panic!("unbound DPI-C function '{name}'"))(args)
    }

    /// Simulator services used by the Accellera UVM DPI wrapper.
    pub fn simulator_defaults() -> DpiRegistry {
        let mut registry = DpiRegistry::default();
        let argv: Rc<[String]> = std::env::args().collect::<Vec<_>>().into();
        let arg_index = Rc::new(Cell::new(0usize));
        registry.register("uvm_dpi_get_next_arg_c", move |args| {
            let reset = args.first().is_some_and(|arg| arg.as_logic().is_true());
            if reset {
                arg_index.set(0);
            }
            let current = arg_index.get();
            let value = argv.get(current).map(String::as_str).unwrap_or("");
            if current < argv.len() {
                arg_index.set(current + 1);
            }
            Value::Str(Rc::from(value))
        });
        registry.register("uvm_dpi_get_tool_name_c", |_| Value::Str(Rc::from("Eevee")));
        registry.register("uvm_dpi_get_tool_version_c", |_| {
            Value::Str(Rc::from(env!("CARGO_PKG_VERSION")))
        });
        registry
    }
}

/// The design's shared linkage tables: every callable function/task and every
/// class. Built once by the elaborator and shared (`Rc`) by all processes;
/// [`Inst::Call`] indexes `funcs` and [`Inst::New`] indexes `classes`.
#[derive(Default)]
pub struct Linkage {
    pub funcs: Vec<Rc<Program>>,
    pub classes: Vec<ClassDef>,
    /// Storage for `static` class fields (shared across all instances and
    /// processes); indexed by the static-field id baked into the IR.
    pub statics: Vec<RefCell<Value>>,
    /// Enum value->name tables, indexed by the id baked into [`Inst::EnumName`].
    pub enum_tables: Vec<std::collections::HashMap<i64, Rc<str>>>,
    /// Host functions available to imported DPI-C callables.
    pub dpi: DpiRegistry,
}

impl Linkage {
    /// An empty linkage (for processes that call nothing and allocate nothing).
    pub fn empty() -> Rc<Linkage> {
        Rc::new(Linkage::default())
    }
}

/// An unbound label handle returned by [`ProgramBuilder::new_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(u32);

/// A tiny assembler for hand-writing or code-generating [`Program`]s.
///
/// The elaborator's procedural-block code generator will drive this same API;
/// the P2 tests and benchmark drive it directly.
pub struct ProgramBuilder {
    code: Vec<Inst>,
    consts: Vec<Value>,
    netlists: Vec<Box<[NetId]>>,
    arglists: Vec<Box<[Reg]>>,
    forks: Vec<Rc<Program>>,
    fork_captures: Vec<Box<[(Reg, Reg)]>>,
    fork_groups: Vec<Box<[u32]>>,
    n_regs: u32,
    arg_modes: Vec<ArgMode>,
    /// label id -> bound code address (`u32::MAX` while unbound).
    labels: Vec<u32>,
    /// (code index of a jump, label id it targets) to fix up at `build`.
    patches: Vec<(usize, u32)>,
    name: String,
}

impl ProgramBuilder {
    /// Start a new program with a debug label.
    pub fn new(name: impl Into<String>) -> ProgramBuilder {
        ProgramBuilder {
            code: Vec::new(),
            consts: Vec::new(),
            netlists: Vec::new(),
            arglists: Vec::new(),
            forks: Vec::new(),
            fork_captures: Vec::new(),
            fork_groups: Vec::new(),
            n_regs: 0,
            arg_modes: Vec::new(),
            labels: Vec::new(),
            patches: Vec::new(),
            name: name.into(),
        }
    }

    /// Allocate a fresh register.
    pub fn new_reg(&mut self) -> Reg {
        let r = self.n_regs;
        self.n_regs += 1;
        r
    }

    /// Record the passing modes of the leading argument registers.
    pub fn set_arg_modes(&mut self, modes: &[ArgMode]) {
        self.arg_modes.clear();
        self.arg_modes.extend_from_slice(modes);
    }

    /// Intern a constant value, returning its id.
    pub fn konst(&mut self, v: Value) -> ConstId {
        let id = self.consts.len() as u32;
        self.consts.push(v);
        id
    }

    /// Intern a logic constant (convenience).
    pub fn konst_logic(&mut self, v: LogicVec) -> ConstId {
        self.konst(Value::Logic(v))
    }

    /// Intern a net read-set (for `WaitCond`).
    pub fn netlist(&mut self, nets: &[NetId]) -> NetListId {
        let id = self.netlists.len() as u32;
        self.netlists.push(nets.to_vec().into_boxed_slice());
        id
    }

    /// Intern an argument register list (for `Display`).
    pub fn arglist(&mut self, regs: &[Reg]) -> ArgListId {
        let id = self.arglists.len() as u32;
        self.arglists.push(regs.to_vec().into_boxed_slice());
        id
    }

    /// Register a compiled `fork` branch program, returning its index into
    /// `forks` (used to build a [`Self::fork_group`]). Each capture maps a
    /// child register to its source register in the enclosing program.
    pub fn add_fork_branch(&mut self, prog: Rc<Program>, captures: &[(Reg, Reg)]) -> u32 {
        let id = self.forks.len() as u32;
        self.forks.push(prog);
        self.fork_captures
            .push(captures.to_vec().into_boxed_slice());
        id
    }

    /// Group a `fork` statement's branch indices (from [`Self::add_fork_branch`])
    /// for use as `Inst::Fork::group`.
    pub fn fork_group(&mut self, branches: &[u32]) -> ForkGroupId {
        let id = self.fork_groups.len() as u32;
        self.fork_groups.push(branches.to_vec().into_boxed_slice());
        id
    }

    /// Create an unbound label.
    pub fn new_label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(u32::MAX);
        Label(id)
    }

    /// Bind a label to the current code position.
    pub fn bind(&mut self, l: Label) {
        self.labels[l.0 as usize] = self.code.len() as u32;
    }

    /// Emit a straight-line instruction (no label operand).
    pub fn emit(&mut self, inst: Inst) {
        debug_assert!(
            !matches!(
                inst,
                Inst::Jump { .. }
                    | Inst::BranchFalse { .. }
                    | Inst::BranchTrue { .. }
                    | Inst::BranchArgProvided { .. }
            ),
            "use jump/branch_false/branch_true for control flow so labels are patched"
        );
        self.code.push(inst);
    }

    /// Emit an unconditional jump to `l`.
    pub fn jump(&mut self, l: Label) {
        self.patches.push((self.code.len(), l.0));
        self.code.push(Inst::Jump { target: 0 });
    }

    /// Emit a conditional jump taken when `cond` is falsey.
    pub fn branch_false(&mut self, cond: Reg, l: Label) {
        self.patches.push((self.code.len(), l.0));
        self.code.push(Inst::BranchFalse { cond, target: 0 });
    }

    /// Emit a conditional jump taken when `cond` is truthy.
    pub fn branch_true(&mut self, cond: Reg, l: Label) {
        self.patches.push((self.code.len(), l.0));
        self.code.push(Inst::BranchTrue { cond, target: 0 });
    }

    /// Jump to `l` if the caller supplied the formal register `arg`.
    pub fn branch_arg_provided(&mut self, arg: Reg, l: Label) {
        self.patches.push((self.code.len(), l.0));
        self.code.push(Inst::BranchArgProvided { arg, target: 0 });
    }

    /// Resolve labels and produce the immutable [`Program`].
    pub fn build(mut self) -> Program {
        for (ci, lid) in &self.patches {
            let target = self.labels[*lid as usize];
            assert!(
                target != u32::MAX,
                "unbound label {lid} referenced at code[{ci}]"
            );
            match &mut self.code[*ci] {
                Inst::Jump { target: t }
                | Inst::BranchFalse { target: t, .. }
                | Inst::BranchTrue { target: t, .. }
                | Inst::BranchArgProvided { target: t, .. } => *t = target,
                _ => unreachable!("patch points only at jump/branch instructions"),
            }
        }
        Program {
            code: self.code,
            consts: self.consts,
            netlists: self.netlists,
            arglists: self.arglists,
            forks: self.forks,
            fork_captures: self.fork_captures,
            fork_groups: self.fork_groups,
            n_regs: self.n_regs,
            arg_modes: self.arg_modes.into_boxed_slice(),
            label: self.name,
        }
    }
}
