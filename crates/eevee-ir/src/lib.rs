//! Register-based bytecode IR and interpreter for eevee.
//!
//! Elaborated SystemVerilog procedural blocks (`always`, `initial`, tasks,
//! functions, class methods) lower to the [`Program`] IR in [`inst`]. The
//! [`Interp`] backend runs that IR via [`IrProcess`], plugging into the P1
//! event-driven scheduler through the same `resume -> Wait` contract the
//! hand-written P1 processes used.
//!
//! Design goals, in priority order:
//! 1. **Fast.** Names are pre-resolved to register/slot indices and [`NetId`]s
//!    at elaboration; the hot loop is a dense `Copy` instruction stream with a
//!    jump-table `match` and an allocation-free narrow-`LogicVec` path.
//! 2. **Resumable without coroutines.** A process's whole state is `(pc, regs)`.
//! 3. **JIT-ready.** The IR is explicit and side-effect-typed; [`ExecBackend`]
//!    is the seam where a Cranelift JIT slots in beside the interpreter.
//!
//! [`NetId`]: eevee_sched::NetId

#![forbid(unsafe_code)]

pub mod inst;
pub mod interp;
pub mod value;

pub use inst::{
    ArgListId, ArgMode, ClassDef, ClassId, CodeAddr, CollOp, ConstId, DpiRegistry, FuncId, Inst,
    Label, Linkage, NetListId, Program, ProgramBuilder, Reg,
};
pub use interp::{ExecBackend, Interp, IrProcess};
pub use value::{AssocKey, ObjData, Value};
