//! The IR interpreter and the backend seam.
//!
//! [`IrProcess`] is the [`eevee_sched::Process`] implementation that runs a
//! [`Program`]. Its entire suspendable state is `(pc, regs)` — there is no
//! coroutine and no OS thread. Hitting a timing instruction returns the
//! corresponding [`Wait`] with the PC parked just past it; the kernel calls
//! [`IrProcess::resume`] again when the wakeup fires and execution continues.
//! This is the same `resume -> Wait` contract the P1 hand-written processes
//! used, so the interpreter drops into the existing scheduler with no changes.
//!
//! # The JIT seam
//!
//! [`ExecBackend::instantiate`] turns an `Rc<Program>` into a `Box<dyn
//! Process>`. [`Interp`] wraps it in an [`IrProcess`]. A future `JitBackend`
//! implements the same trait by compiling the program to native code (e.g. via
//! Cranelift — pure Rust, no external linker) and returning a native-backed
//! process that honors the identical `resume -> Wait` contract. Nothing else in
//! the simulator needs to know which backend produced a process.

use std::cell::RefCell;
use std::rc::Rc;

use eevee_core::LogicVec;
use eevee_sched::{ForkJoin, Kernel, Process, Wait};

use crate::inst::{CollOp, Inst, Linkage, Program, Reg};
use crate::value::{AssocKey, ObjData, Value};
/// One activation record: a program, a PC into it, and a register frame. A
/// process is a *stack* of these (the bottom is the `always`/`initial` block;
/// each function/task call pushes one). The whole stack is the process's
/// suspendable state, so a task may suspend (`#`/`@`/`wait`) mid-call with the
/// entire call chain preserved across `resume`.
struct Frame {
    prog: Rc<Program>,
    pc: usize,
    regs: Vec<Value>,
    /// Register in the *caller's* frame to receive this call's return value.
    ret_dst: Reg,
    /// Copy-out mappings owned by this frame: `(formal, caller actual)`.
    copy_out: Vec<(usize, Reg)>,
    /// Number of leading formal registers supplied by the caller.
    provided_args: usize,
    /// Procedural-variable updates queued by this activation for NBA.
    pending_nba: Vec<(Reg, Value)>,
}

type ForkChild = (Rc<Program>, Vec<(Reg, Value)>);

/// What running one frame to its next transition produced.
enum Step {
    /// Suspend the whole process on this wait.
    Wait(Wait),
    /// Push a new frame: call `func` with `vals` as its leading registers,
    /// returning into the caller's `ret_dst`.
    Call {
        func: u32,
        vals: Vec<Value>,
        actuals: Vec<Reg>,
        ret_dst: Reg,
    },
    /// Pop the current frame, delivering `Some(value)` to the caller's
    /// `ret_dst` (or nothing for a void return).
    Return(Option<Value>),
    /// `fork`: spawn branch programs with their captured register values as
    /// independent processes, per `join`.
    Fork {
        children: Vec<ForkChild>,
        join: ForkJoin,
    },
}

/// A process that executes IR via the interpreter, with a call stack.
///
/// The currently-executing frame is held in **inline fields** so the common
/// case (a process running its body with no active call — e.g. all RTL) runs
/// directly on `prog`/`pc`/`regs`, exactly as fast as a single-frame
/// interpreter. Only when a function/task is called does a caller frame spill
/// to `callers`.
pub struct IrProcess {
    prog: Rc<Program>,
    pc: usize,
    regs: Vec<Value>,
    /// Where the current frame's return value goes in its caller's registers.
    ret_dst: Reg,
    /// Output/inout/ref formals copied back when the current frame returns.
    copy_out: Vec<(usize, Reg)>,
    /// Number of leading formal registers supplied by the caller.
    provided_args: usize,
    /// Procedural-variable updates queued by the current activation for NBA.
    pending_nba: Vec<(Reg, Value)>,
    /// Apply `pending_nba` before executing the next instruction.
    apply_pending_nba: bool,
    /// Suspended caller frames — empty unless inside a call.
    callers: Vec<Frame>,
    /// The design's shared linkage (functions + classes), indexed by
    /// [`Inst::Call`] / [`Inst::New`].
    linkage: Rc<Linkage>,
}

impl IrProcess {
    /// Create a runnable instance of `prog` (the bottom frame) with access to
    /// the shared `linkage` (functions + classes).
    pub fn new(prog: Rc<Program>, linkage: Rc<Linkage>) -> IrProcess {
        let regs = vec![Value::Null; prog.n_regs as usize];
        IrProcess {
            prog,
            pc: 0,
            regs,
            ret_dst: 0,
            copy_out: Vec::new(),
            provided_args: 0,
            pending_nba: Vec::new(),
            apply_pending_nba: false,
            callers: Vec::new(),
            linkage,
        }
    }

    /// The core resume loop. Wrapped by [`Process::resume`], which catches any
    /// runtime fault and prints an SV-level call stack ([`Self::sv_backtrace`])
    /// before letting the panic propagate.
    fn run(&mut self, k: &mut Kernel) -> Wait {
        if std::mem::take(&mut self.apply_pending_nba) {
            for (dst, value) in self.pending_nba.drain(..) {
                self.regs[dst as usize] = value;
            }
            k.notify_runtime_change();
        }
        loop {
            match run_frame(
                &self.prog,
                &mut self.pc,
                &mut self.regs,
                self.provided_args,
                &mut self.pending_nba,
                &self.linkage,
                k,
            ) {
                Step::Wait(w) => {
                    if matches!(w, Wait::Nba) {
                        self.apply_pending_nba = true;
                    }
                    return w;
                }
                Step::Call {
                    func,
                    vals,
                    actuals,
                    ret_dst,
                } => {
                    let callee = self.linkage.funcs[func as usize].clone();
                    let provided_args = vals.len();
                    if trace_on() {
                        let a0 = vals.first().map(vsum).unwrap_or_default();
                        eprintln!(
                            "{:>w$}> {} (arg0={a0})",
                            "",
                            callee.label,
                            w = self.callers.len() * 2
                        );
                    }
                    let mut regs = vec![Value::Null; callee.n_regs as usize];
                    let mut copy_out = Vec::new();
                    for (i, v) in vals.into_iter().enumerate() {
                        if i < regs.len() {
                            let mode = callee
                                .arg_modes
                                .get(i)
                                .copied()
                                .unwrap_or(crate::inst::ArgMode::Input);
                            if mode != crate::inst::ArgMode::Output {
                                regs[i] = v;
                            }
                            if mode.copies_out() {
                                copy_out.push((i, actuals[i]));
                            }
                        }
                    }
                    // Save the current frame and switch to the callee.
                    let prev = Frame {
                        prog: std::mem::replace(&mut self.prog, callee),
                        pc: std::mem::replace(&mut self.pc, 0),
                        regs: std::mem::replace(&mut self.regs, regs),
                        ret_dst: std::mem::replace(&mut self.ret_dst, ret_dst),
                        copy_out: std::mem::replace(&mut self.copy_out, copy_out),
                        provided_args: std::mem::replace(&mut self.provided_args, provided_args),
                        pending_nba: std::mem::take(&mut self.pending_nba),
                    };
                    self.callers.push(prev);
                }
                Step::Return(val) => {
                    if trace_on() {
                        let rv = val.as_ref().map(vsum).unwrap_or_else(|| "void".into());
                        eprintln!(
                            "{:>w$}< {} -> {rv}",
                            "",
                            self.prog.label,
                            w = self.callers.len().saturating_sub(1) * 2
                        );
                    }
                    match self.callers.pop() {
                        Some(mut caller) => {
                            for &(formal, actual) in &self.copy_out {
                                caller.regs[actual as usize] = self.regs[formal].clone();
                            }
                            if let Some(v) = val {
                                caller.regs[self.ret_dst as usize] = v;
                            }
                            self.prog = caller.prog;
                            self.pc = caller.pc;
                            self.regs = caller.regs;
                            self.ret_dst = caller.ret_dst;
                            self.copy_out = caller.copy_out;
                            self.provided_args = caller.provided_args;
                            self.pending_nba = caller.pending_nba;
                        }
                        // Returned past the bottom frame — the process is done.
                        None => return Wait::Finished,
                    }
                }
                Step::Fork { children, join } => {
                    let children: Vec<Box<dyn Process>> = children
                        .into_iter()
                        .map(|(prog, captures)| {
                            let mut p = IrProcess::new(prog, self.linkage.clone());
                            for (child_reg, value) in captures {
                                p.regs[child_reg as usize] = value;
                            }
                            Box::new(p) as Box<dyn Process>
                        })
                        .collect();
                    return Wait::Fork { children, join };
                }
            }
        }
    }

    /// Format the SystemVerilog call stack at the current execution point,
    /// innermost frame first. Each entry is the frame's program label (the
    /// function/task/`initial` name) and its instruction pointer. `run_frame`
    /// advances the IP past an instruction before executing it, so the reported
    /// IP is the faulting/call instruction itself.
    fn sv_backtrace(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::from("SystemVerilog call stack (innermost first):\n");
        let _ = writeln!(
            s,
            "  #0 {} (ip {})",
            self.prog.label,
            self.pc.saturating_sub(1)
        );
        for (i, f) in self.callers.iter().rev().enumerate() {
            let _ = writeln!(
                s,
                "  #{} {} (ip {})",
                i + 1,
                f.prog.label,
                f.pc.saturating_sub(1)
            );
        }
        s
    }
}

impl Process for IrProcess {
    fn resume(&mut self, k: &mut Kernel) -> Wait {
        // Run the body under a catch so a runtime fault (null handle, OOB, ...)
        // is annotated with an SV-level call stack before the panic propagates.
        // The unwind landing pad is effectively free on the happy path.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(k))) {
            Ok(w) => w,
            Err(payload) => {
                eprint!("{}", self.sv_backtrace());
                std::panic::resume_unwind(payload);
            }
        }
    }

    fn label(&self) -> &str {
        self.callers
            .first()
            .map(|f| f.prog.label.as_str())
            .unwrap_or(&self.prog.label)
    }
}

/// Whether execution tracing is enabled (env `EEVEE_TRACE`), read once.
fn trace_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("EEVEE_TRACE").is_ok())
}

fn cast_trace_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("EEVEE_TRACE_CAST").is_ok())
}

/// One-line summary of a value for traces.
fn vsum(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Obj(rc) => format!("obj#cls{}", rc.borrow().class),
        Value::Logic(l) => l.to_u64().to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Real(r) => r.to_string(),
        Value::Queue(rc) => format!("queue[{}]", rc.borrow().len()),
        Value::Assoc(rc) => format!("assoc[{}]", rc.borrow().len()),
        Value::Event(id) => format!("event#{id}"),
    }
}

/// Construct a fresh instance of `class`: collection-typed fields get fresh
/// empty storage (no aliasing across instances), and struct-typed fields (an
/// unpacked `typedef struct {...}` field, modeled as a no-method class — see
/// `ClassId`/`is_struct` on the elaborator side) get a fresh, fully
/// default-constructed sub-object of their own — recursively, so a struct
/// nested inside a struct is handled too — rather than staying a null handle
/// awaiting an explicit `new()` the way a real class-typed field does.
fn new_instance(class: crate::inst::ClassId, linkage: &Linkage) -> Value {
    let def = &linkage.classes[class as usize];
    let mut fields = def.field_defaults.to_vec();
    for &(slot, is_assoc) in def.coll_fields.iter() {
        fields[slot as usize] = if is_assoc {
            Value::new_assoc()
        } else {
            Value::new_queue()
        };
    }
    for &(slot, struct_cid) in def.struct_fields.iter() {
        fields[slot as usize] = new_instance(struct_cid, linkage);
    }
    for &slot in def.event_fields.iter() {
        fields[slot as usize] = Value::new_event();
    }
    Value::Obj(Rc::new(RefCell::new(ObjData { class, fields })))
}

fn class_is_a(
    mut actual: crate::inst::ClassId,
    target: crate::inst::ClassId,
    linkage: &Linkage,
) -> bool {
    for _ in 0..linkage.classes.len() {
        if actual == target {
            return true;
        }
        let Some(base) = linkage.classes[actual as usize].base else {
            return false;
        };
        actual = base;
    }
    false
}

/// Run one frame from `*pc` until it transitions (suspends, calls, or returns),
/// updating `*pc` and `regs` in place.
fn run_frame(
    prog: &Program,
    pc: &mut usize,
    regs: &mut [Value],
    provided_args: usize,
    pending_nba: &mut Vec<(Reg, Value)>,
    linkage: &Linkage,
    k: &mut Kernel,
) -> Step {
    let code = &prog.code;
    loop {
        // `Inst` is `Copy`, so this lifts the instruction out without holding a
        // borrow on `code` across the body (lets us freely touch `k`).
        let inst = code[*pc];
        *pc += 1;
        match inst {
            Inst::LoadConst { dst, k: ci } => {
                regs[dst as usize] = prog.consts[ci as usize].clone();
            }
            Inst::Mov { dst, src } => {
                regs[dst as usize] = regs[src as usize].clone();
            }
            Inst::Resize {
                dst,
                src,
                width,
                signed,
            } => {
                let value = regs[src as usize].as_logic().resize(width, signed);
                regs[dst as usize] = Value::Logic(value);
            }
            Inst::Assign { dst, src } => {
                regs[dst as usize] = regs[src as usize].assignment_copy();
            }
            Inst::NbaAssign { dst, src } => {
                let value = regs[src as usize].assignment_copy();
                if let Some((_, pending)) = pending_nba
                    .iter_mut()
                    .find(|(pending_dst, _)| *pending_dst == dst)
                {
                    *pending = value;
                } else {
                    pending_nba.push((dst, value));
                }
            }
            Inst::NetRead { dst, net } => {
                regs[dst as usize] = Value::Logic(k.net_value(net).clone());
            }
            Inst::Not { dst, a } => {
                let v = regs[a as usize].as_logic().bitnot();
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Add { dst, a, b } => {
                let v = regs[a as usize].as_logic().add(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Sub { dst, a, b } => {
                let v = regs[a as usize].as_logic().sub(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Mul { dst, a, b } => {
                let v = regs[a as usize].as_logic().mul(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::And { dst, a, b } => {
                let v = regs[a as usize]
                    .as_logic()
                    .bitand(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Or { dst, a, b } => {
                let v = regs[a as usize]
                    .as_logic()
                    .bitor(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Xor { dst, a, b } => {
                let v = regs[a as usize]
                    .as_logic()
                    .bitxor(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Eq { dst, a, b } => {
                let v = eval_eq(&regs[a as usize], &regs[b as usize]);
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Neq { dst, a, b } => {
                let v = eval_eq(&regs[a as usize], &regs[b as usize]).lognot();
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Lt { dst, a, b } => {
                let v = regs[a as usize].as_logic().ult(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Le { dst, a, b } => {
                let v = regs[a as usize].as_logic().ule(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Gt { dst, a, b } => {
                let v = regs[a as usize].as_logic().ugt(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Ge { dst, a, b } => {
                let v = regs[a as usize].as_logic().uge(regs[b as usize].as_logic());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Shl { dst, a, b } => {
                let amt = regs[b as usize].as_logic().to_u64() as u32;
                let v = regs[a as usize].as_logic().shl(amt);
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Shr { dst, a, b } => {
                let amt = regs[b as usize].as_logic().to_u64() as u32;
                let v = regs[a as usize].as_logic().shr(amt);
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::LogAnd { dst, a, b } => {
                let v = value_truthy(&regs[a as usize]) && value_truthy(&regs[b as usize]);
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(v as u64, 1));
            }
            Inst::LogOr { dst, a, b } => {
                let v = value_truthy(&regs[a as usize]) || value_truthy(&regs[b as usize]);
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(v as u64, 1));
            }
            Inst::Select {
                dst,
                condition,
                when_true,
                when_false,
            } => {
                let condition = regs[condition as usize].as_logic();
                regs[dst as usize] = match (&regs[when_true as usize], &regs[when_false as usize]) {
                    (Value::Logic(when_true), Value::Logic(when_false)) => {
                        Value::Logic(LogicVec::conditional(condition, when_true, when_false))
                    }
                    (when_true, when_false) if values_identical(when_true, when_false) => {
                        when_true.clone()
                    }
                    _ => Value::Null,
                };
            }
            Inst::IsKnown { dst, a } => {
                let known = match &regs[a as usize] {
                    Value::Logic(value) => value.is_known(),
                    _ => true,
                };
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(known as u64, 1));
            }
            Inst::LogNot { dst, a } => {
                let v = regs[a as usize].as_logic().lognot();
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::Neg { dst, a } => {
                let v = regs[a as usize].as_logic().neg();
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::ReduceAnd { dst, a } => {
                let v = LogicVec::from_bit(regs[a as usize].as_logic().reduce_and());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::ReduceOr { dst, a } => {
                let v = LogicVec::from_bit(regs[a as usize].as_logic().reduce_or());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::ReduceXor { dst, a } => {
                let v = LogicVec::from_bit(regs[a as usize].as_logic().reduce_xor());
                regs[dst as usize] = Value::Logic(v);
            }
            Inst::BlockingWrite { net, src } => {
                let v = regs[src as usize].as_logic().clone();
                k.write_net(net, v);
            }
            Inst::NbaWrite { net, src } => {
                let v = regs[src as usize].as_logic().clone();
                k.schedule_nba(net, v);
            }
            Inst::DriveNet { driver, src } => {
                let value = regs[src as usize].as_logic().clone();
                k.drive_net(driver, value);
            }
            Inst::ScheduleDrive {
                driver,
                src,
                delay_fs,
            } => {
                let value = regs[src as usize].as_logic().clone();
                k.schedule_drive(driver, value, delay_fs);
            }
            Inst::ScheduleDriveTransitions {
                driver,
                src,
                delays,
            } => {
                let value = regs[src as usize].as_logic().clone();
                k.schedule_drive_transitions(driver, value, delays);
            }
            Inst::Delay { fs } => return Step::Wait(Wait::Delay(fs)),
            Inst::WaitEdge { net, edge } => return Step::Wait(Wait::Edge(net, edge)),
            Inst::WaitCond { nets } => {
                return Step::Wait(Wait::Cond(prog.netlists[nets as usize].to_vec()));
            }
            Inst::WaitRuntime => return Step::Wait(Wait::RuntimeCond),
            Inst::WaitChange { value } => {
                if pending_nba.iter().any(|(dst, _)| *dst == value) {
                    return Step::Wait(Wait::Nba);
                }
                return Step::Wait(Wait::RuntimeCond);
            }
            Inst::WaitEvent { event } => match regs[event as usize] {
                Value::Event(id) => return Step::Wait(Wait::NamedEvent(id)),
                ref value => panic!("event control on non-event value {value:?}"),
            },
            Inst::Jump { target } => *pc = target as usize,
            Inst::BranchFalse { cond, target } => {
                let c = &regs[cond as usize];
                if trace_on() {
                    eprintln!("      branch-if-false cond={} [{}]", vsum(c), prog.label);
                }
                if !value_truthy(c) {
                    *pc = target as usize;
                }
            }
            Inst::BranchTrue { cond, target } => {
                if value_truthy(&regs[cond as usize]) {
                    *pc = target as usize;
                }
            }
            Inst::BranchArgProvided { arg, target } => {
                if (arg as usize) < provided_args {
                    *pc = target as usize;
                }
            }
            Inst::Display { fmt, args } => {
                let f = match &prog.consts[fmt as usize] {
                    Value::Str(s) => s.clone(),
                    _ => std::rc::Rc::from(""),
                };
                let line = format_display(&f, &prog.arglists[args as usize], regs);
                k.display(line);
            }
            Inst::DpiCall { dst, name, args } => {
                let name = match &prog.consts[name as usize] {
                    Value::Str(name) => name.clone(),
                    value => panic!("DPI symbol name is not a string: {value:?}"),
                };
                let arg_regs = &prog.arglists[args as usize];
                let mut values: Vec<Value> = arg_regs
                    .iter()
                    .map(|arg| regs[*arg as usize].clone())
                    .collect();
                let result = linkage.dpi.call(&name, &mut values);
                for (arg, value) in arg_regs.iter().zip(values) {
                    regs[*arg as usize] = value;
                }
                regs[dst as usize] = result;
            }
            Inst::Call { func, args, ret } => {
                let arglist = &prog.arglists[args as usize];
                let vals: Vec<Value> = arglist.iter().map(|r| regs[*r as usize].clone()).collect();
                return Step::Call {
                    func,
                    vals,
                    actuals: arglist.to_vec(),
                    ret_dst: ret,
                };
            }
            Inst::CallVirtual {
                obj,
                vslot,
                args,
                ret,
            } => {
                // Resolve the implementation from the object's runtime class.
                let func = match &regs[obj as usize] {
                    Value::Obj(rc) => {
                        let cls = rc.borrow().class as usize;
                        linkage.classes[cls].vtable[vslot as usize]
                    }
                    _ => panic!(
                        "virtual method call (vslot {vslot}) on a null handle in '{}'",
                        prog.label
                    ),
                };
                let arglist = &prog.arglists[args as usize];
                let vals: Vec<Value> = arglist.iter().map(|r| regs[*r as usize].clone()).collect();
                return Step::Call {
                    func,
                    vals,
                    actuals: arglist.to_vec(),
                    ret_dst: ret,
                };
            }
            Inst::Return { value } => return Step::Return(Some(regs[value as usize].clone())),
            Inst::ReturnVoid => return Step::Return(None),
            Inst::New { dst, class } => {
                regs[dst as usize] = new_instance(class, linkage);
            }
            Inst::NewEvent { dst } => {
                regs[dst as usize] = Value::new_event();
            }
            Inst::TriggerEvent { event } => match regs[event as usize] {
                Value::Event(id) => k.trigger_event(id),
                ref value => panic!("event trigger on non-event value {value:?}"),
            },
            Inst::ClassCast { dst, src, class } => {
                let actual = match &regs[src as usize] {
                    Value::Obj(obj) => Some(obj.borrow().class),
                    _ => None,
                };
                let succeeds = match actual {
                    None => matches!(regs[src as usize], Value::Null),
                    Some(actual) => class_is_a(actual, class, linkage),
                };
                if cast_trace_on() {
                    let actual_name = actual
                        .map(|id| linkage.classes[id as usize].name.as_str())
                        .unwrap_or("<non-object>");
                    let fields = match &regs[src as usize] {
                        Value::Obj(obj) => obj
                            .borrow()
                            .fields
                            .iter()
                            .map(vsum)
                            .collect::<Vec<_>>()
                            .join(", "),
                        _ => String::new(),
                    };
                    eprintln!(
                        "class cast {actual_name} -> {}: {succeeds}; fields=[{fields}]",
                        linkage.classes[class as usize].name,
                    );
                }
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(succeeds as u64, 1));
            }
            Inst::GetField { dst, obj, slot } => {
                let v = match &regs[obj as usize] {
                    Value::Obj(rc) => rc.borrow().fields[slot as usize].clone(),
                    _ => Value::Null,
                };
                regs[dst as usize] = v;
            }
            Inst::SetField { obj, slot, src } => {
                let v = regs[src as usize].assignment_copy();
                if let Value::Obj(rc) = &regs[obj as usize] {
                    rc.borrow_mut().fields[slot as usize] = v;
                    k.notify_runtime_change();
                }
            }
            Inst::StaticGet { dst, id } => {
                let v = linkage.statics[id as usize].borrow().clone();
                if trace_on() {
                    eprintln!("      sget static#{id} -> {} [{}]", vsum(&v), prog.label);
                }
                regs[dst as usize] = v;
            }
            Inst::StaticSet { id, src } => {
                let v = regs[src as usize].assignment_copy();
                if trace_on() {
                    eprintln!("      sset static#{id} <- {} [{}]", vsum(&v), prog.label);
                }
                *linkage.statics[id as usize].borrow_mut() = v;
                k.notify_runtime_change();
            }
            Inst::NewQueue { dst } => {
                regs[dst as usize] = Value::new_queue();
            }
            Inst::NewAssoc { dst } => {
                regs[dst as usize] = Value::new_assoc();
            }
            Inst::IndexGet { dst, base, idx } => {
                let v = match &regs[base as usize] {
                    Value::Logic(value) => {
                        let i = regs[idx as usize].as_logic().to_u64() as u32;
                        Value::Logic(LogicVec::from_bit(value.get_bit(i)))
                    }
                    Value::Queue(rc) => {
                        let i = regs[idx as usize].as_logic().to_u64() as usize;
                        rc.borrow().get(i).cloned().unwrap_or(Value::Null)
                    }
                    Value::Assoc(rc) => {
                        let key = value_to_key(&regs[idx as usize]);
                        rc.borrow().get(&key).cloned().unwrap_or(Value::Null)
                    }
                    _ => Value::Null,
                };
                regs[dst as usize] = v;
            }
            Inst::PartSelect {
                dst,
                base,
                left,
                right,
            } => {
                let value = regs[base as usize].as_logic();
                let left = regs[left as usize].as_logic().to_u64() as u32;
                let right = regs[right as usize].as_logic().to_u64() as u32;
                let selected = if left >= right {
                    value.slice(left, right)
                } else {
                    let width = right - left + 1;
                    let mut selected = LogicVec::zero(width);
                    for offset in 0..width {
                        selected.set_bit(offset, value.get_bit(right - offset));
                    }
                    selected
                };
                regs[dst as usize] = Value::Logic(selected);
            }
            Inst::IndexSet { base, idx, src } => {
                let v = regs[src as usize].assignment_copy();
                let key = value_to_key(&regs[idx as usize]);
                let i = match &key {
                    AssocKey::Int(value) => *value as usize,
                    _ => 0,
                };
                let changed = match &mut regs[base as usize] {
                    Value::Logic(value) => {
                        value.set_bit(i as u32, v.as_logic().get_bit(0));
                        true
                    }
                    Value::Queue(rc) => {
                        let mut q = rc.borrow_mut();
                        if i >= q.len() {
                            q.resize(i + 1, Value::Null);
                        }
                        q[i] = v;
                        true
                    }
                    Value::Assoc(rc) => {
                        rc.borrow_mut().insert(key, v);
                        true
                    }
                    _ => false,
                };
                if changed {
                    k.notify_runtime_change();
                }
            }
            Inst::CollMethod {
                dst,
                base,
                op,
                args,
            } => {
                let arglist = &prog.arglists[args as usize];
                let base_value = regs[base as usize].clone();
                let result = eval_coll_method(&base_value, op, arglist, regs);
                regs[dst as usize] = result;
                if matches!(
                    op,
                    CollOp::PushBack
                        | CollOp::PushFront
                        | CollOp::PopBack
                        | CollOp::PopFront
                        | CollOp::Insert
                        | CollOp::Delete
                ) {
                    k.notify_runtime_change();
                }
            }
            Inst::Concat { dst, args } => {
                let arglist = &prog.arglists[args as usize];
                if arglist
                    .iter()
                    .any(|reg| matches!(regs[*reg as usize], Value::Str(_)))
                {
                    let mut s = String::new();
                    for r in arglist.iter() {
                        s.push_str(&value_to_str(&regs[*r as usize]));
                    }
                    regs[dst as usize] = Value::Str(Rc::from(s.as_str()));
                } else {
                    let mut values = arglist.iter().map(|reg| regs[*reg as usize].as_logic());
                    let value = match values.next() {
                        Some(first) => values.fold(first.clone(), |value, part| value.concat(part)),
                        None => LogicVec::zero(1),
                    };
                    regs[dst as usize] = Value::Logic(value);
                }
            }
            Inst::SimTime { dst } => {
                let t = k.time().0;
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(t, 64));
            }
            Inst::Format { dst, fmt, args } => {
                let f = match &prog.consts[fmt as usize] {
                    Value::Str(s) => s.clone(),
                    _ => std::rc::Rc::from(""),
                };
                let line = format_display(&f, &prog.arglists[args as usize], regs);
                regs[dst as usize] = Value::Str(Rc::from(line.as_str()));
            }
            Inst::EnumName { dst, src, table } => {
                let v = regs[src as usize].as_logic().to_i64();
                let name = linkage.enum_tables[table as usize]
                    .get(&v)
                    .cloned()
                    .unwrap_or_else(|| Rc::from(v.to_string().as_str()));
                regs[dst as usize] = Value::Str(name);
            }
            Inst::StringLen { dst, src } => {
                let s = match &regs[src as usize] {
                    Value::Str(s) => s.len() as u64,
                    _ => 0,
                };
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(s, 32));
            }
            Inst::StringSub { dst, src, lo, hi } => {
                let s = match &regs[src as usize] {
                    Value::Str(s) => s.clone(),
                    _ => Rc::from(""),
                };
                let lo = regs[lo as usize].as_logic().to_u64() as usize;
                let hi = regs[hi as usize].as_logic().to_u64() as usize;
                let chars: Vec<char> = s.chars().collect();
                let lo = lo.min(chars.len());
                let hi = (hi + 1).min(chars.len());
                let sub: String = chars[lo..hi.max(lo)].iter().collect();
                regs[dst as usize] = Value::Str(Rc::from(sub.as_str()));
            }
            Inst::StringIndex { dst, src, idx } => {
                let s = match &regs[src as usize] {
                    Value::Str(s) => s.clone(),
                    _ => Rc::from(""),
                };
                let i = regs[idx as usize].as_logic().to_u64() as usize;
                let ch = s.as_bytes().get(i).copied().unwrap_or(0);
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(ch as u64, 8));
            }
            Inst::StringToUpper { dst, src } => {
                let s = match &regs[src as usize] {
                    Value::Str(s) => s.to_uppercase(),
                    _ => String::new(),
                };
                regs[dst as usize] = Value::Str(Rc::from(s.as_str()));
            }
            Inst::StringToLower { dst, src } => {
                let s = match &regs[src as usize] {
                    Value::Str(s) => s.to_lowercase(),
                    _ => String::new(),
                };
                regs[dst as usize] = Value::Str(Rc::from(s.as_str()));
            }
            Inst::StringAtoi { dst, src } => {
                let v = match &regs[src as usize] {
                    Value::Str(s) => s.trim().parse::<i64>().unwrap_or(0) as u64,
                    _ => 0,
                };
                regs[dst as usize] = Value::Logic(LogicVec::from_u64(v, 32));
            }
            Inst::Finish => return Step::Wait(Wait::Finished),
            Inst::Fork { group, join } => {
                let children: Vec<ForkChild> = prog.fork_groups[group as usize]
                    .iter()
                    .map(|&i| {
                        let captures = prog.fork_captures[i as usize]
                            .iter()
                            .map(|&(child, parent)| (child, regs[parent as usize].clone()))
                            .collect();
                        (prog.forks[i as usize].clone(), captures)
                    })
                    .collect();
                return Step::Fork { children, join };
            }
        }
    }
}

/// Format a `$display` line. `%d/%h(x)/%o/%b` format the next logic argument in
/// that radix, `%s` the next string argument, `%%` a literal percent. A leading
/// width/flags field (e.g. the `0` in `%0d`) is accepted and currently ignored
/// (minimal-width output, which matches UVM's ubiquitous `%0d`).
fn format_display(fmt: &str, args: &[Reg], regs: &[Value]) -> String {
    let mut out = String::new();
    let mut ai = 0usize;
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        while matches!(chars.peek(), Some(d) if d.is_ascii_digit()) {
            chars.next();
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d' | 'D') => out.push_str(&fmt_radix(next_arg(args, regs, &mut ai), 10)),
            Some('h' | 'H' | 'x' | 'X') => {
                out.push_str(&fmt_radix(next_arg(args, regs, &mut ai), 16))
            }
            Some('o' | 'O') => out.push_str(&fmt_radix(next_arg(args, regs, &mut ai), 8)),
            Some('b' | 'B') => out.push_str(&fmt_radix(next_arg(args, regs, &mut ai), 2)),
            Some('s' | 'S') => out.push_str(&fmt_string(next_arg(args, regs, &mut ai))),
            // `%t` time format: decimal value (time unit scaling is a later
            // refinement; at time 0 this is just `0`).
            Some('t' | 'T') => out.push_str(&fmt_radix(next_arg(args, regs, &mut ai), 10)),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn next_arg<'a>(args: &[Reg], regs: &'a [Value], ai: &mut usize) -> Option<&'a Value> {
    let v = args.get(*ai).map(|r| &regs[*r as usize]);
    *ai += 1;
    v
}

fn fmt_radix(arg: Option<&Value>, radix: u32) -> String {
    match arg {
        Some(Value::Logic(v)) => {
            let n = v.to_u64();
            match radix {
                16 => format!("{n:x}"),
                8 => format!("{n:o}"),
                2 => format!("{n:b}"),
                _ => format!("{n}"),
            }
        }
        Some(Value::Real(r)) => format!("{r}"),
        _ => "?".to_string(),
    }
}

fn fmt_string(arg: Option<&Value>) -> String {
    match arg {
        Some(Value::Str(s)) => s.to_string(),
        Some(Value::Logic(v)) => v.to_string(),
        _ => "?".to_string(),
    }
}

/// Stringify a value for string concatenation (`{a, b, c}`): strings as-is,
/// vectors as their decimal text, null/handles as empty.
fn value_to_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Logic(l) => l.to_string(),
        Value::Real(r) => r.to_string(),
        _ => String::new(),
    }
}

/// SystemVerilog truthiness for a condition: a known-nonzero vector, a non-null
/// handle/collection, or a non-empty string.
fn value_truthy(v: &Value) -> bool {
    match v {
        Value::Logic(l) => l.is_true(),
        Value::Obj(_) | Value::Queue(_) | Value::Assoc(_) | Value::Event(_) => true,
        Value::Null => false,
        Value::Str(s) => !s.is_empty(),
        Value::Real(r) => *r != 0.0,
    }
}

fn values_identical(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Logic(left), Value::Logic(right)) => left.eq_case(right),
        (Value::Real(left), Value::Real(right)) => left.to_bits() == right.to_bits(),
        (Value::Str(left), Value::Str(right)) => left == right,
        (Value::Obj(left), Value::Obj(right)) => Rc::ptr_eq(left, right),
        (Value::Queue(left), Value::Queue(right)) => Rc::ptr_eq(left, right),
        (Value::Assoc(left), Value::Assoc(right)) => Rc::ptr_eq(left, right),
        (Value::Event(left), Value::Event(right)) => left == right,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

/// Equality (`==`) for any value kind: 4-state for vectors, identity for
/// handles/collections, value for strings, with null comparisons. Returns a
/// 1-bit logic result.
fn eval_eq(a: &Value, b: &Value) -> LogicVec {
    let one = |t: bool| LogicVec::from_u64(t as u64, 1);
    match (a, b) {
        (Value::Logic(x), Value::Logic(y)) => x.eq_logical(y),
        (Value::Null, Value::Null) => one(true),
        (Value::Obj(x), Value::Obj(y)) => one(Rc::ptr_eq(x, y)),
        (Value::Queue(x), Value::Queue(y)) => one(Rc::ptr_eq(x, y)),
        (Value::Assoc(x), Value::Assoc(y)) => one(Rc::ptr_eq(x, y)),
        (Value::Str(x), Value::Str(y)) => one(x == y),
        (Value::Event(x), Value::Event(y)) => one(x == y),
        (Value::Obj(_) | Value::Queue(_) | Value::Assoc(_) | Value::Event(_), Value::Null)
        | (Value::Null, Value::Obj(_) | Value::Queue(_) | Value::Assoc(_) | Value::Event(_)) => {
            one(false)
        }
        _ => one(false),
    }
}

/// Convert an index value into an associative-array key (int or string).
fn value_to_key(v: &Value) -> AssocKey {
    match v {
        Value::Str(s) => AssocKey::Str(s.clone()),
        Value::Logic(lv) => AssocKey::Int(lv.to_i64()),
        Value::Obj(obj) => AssocKey::Obj(obj.clone()),
        Value::Null => AssocKey::Null,
        Value::Real(value) => AssocKey::Int(*value as i64),
        Value::Queue(_) | Value::Assoc(_) | Value::Event(_) => AssocKey::Null,
    }
}

fn key_to_value(key: &AssocKey) -> Value {
    match key {
        AssocKey::Int(value) => Value::Logic(LogicVec::from_i64(*value, 64)),
        AssocKey::Str(value) => Value::Str(value.clone()),
        AssocKey::Obj(value) => Value::Obj(value.clone()),
        AssocKey::Null => Value::Null,
    }
}

/// Evaluate a built-in queue/array/assoc method (`push_back`, `size`, ...).
fn eval_coll_method(base: &Value, op: CollOp, args: &[Reg], regs: &mut [Value]) -> Value {
    let int_val = |n: u64| Value::Logic(LogicVec::from_u64(n, 32));
    match base {
        Value::Queue(rc) => {
            let mut q = rc.borrow_mut();
            match op {
                CollOp::PushBack => {
                    if let Some(&a) = args.first() {
                        q.push(regs[a as usize].clone());
                    }
                    Value::Null
                }
                CollOp::PushFront => {
                    if let Some(&a) = args.first() {
                        q.insert(0, regs[a as usize].clone());
                    }
                    Value::Null
                }
                CollOp::PopBack => q.pop().unwrap_or(Value::Null),
                CollOp::PopFront => {
                    if q.is_empty() {
                        Value::Null
                    } else {
                        q.remove(0)
                    }
                }
                CollOp::Size | CollOp::Num => int_val(q.len() as u64),
                CollOp::Insert => {
                    if args.len() >= 2 {
                        let i = (regs[args[0] as usize].as_logic().to_u64() as usize).min(q.len());
                        q.insert(i, regs[args[1] as usize].clone());
                    }
                    Value::Null
                }
                CollOp::Delete => {
                    match args.first() {
                        Some(&a) => {
                            let i = regs[a as usize].as_logic().to_u64() as usize;
                            if i < q.len() {
                                q.remove(i);
                            }
                        }
                        None => q.clear(),
                    }
                    Value::Null
                }
                CollOp::First => match args.first() {
                    Some(&arg) if !q.is_empty() => {
                        regs[arg as usize] = int_val(0);
                        int_val(1)
                    }
                    _ => int_val(0),
                },
                CollOp::Last => match args.first() {
                    Some(&arg) if !q.is_empty() => {
                        regs[arg as usize] = int_val((q.len() - 1) as u64);
                        int_val(1)
                    }
                    _ => int_val(0),
                },
                CollOp::Next => match args.first() {
                    Some(&arg) => {
                        let next = regs[arg as usize].as_logic().to_u64() as usize + 1;
                        if next < q.len() {
                            regs[arg as usize] = int_val(next as u64);
                            int_val(1)
                        } else {
                            int_val(0)
                        }
                    }
                    None => int_val(0),
                },
                CollOp::Prev => match args.first() {
                    Some(&arg) => {
                        let current = regs[arg as usize].as_logic().to_u64() as usize;
                        if current > 0 && current <= q.len() {
                            regs[arg as usize] = int_val((current - 1) as u64);
                            int_val(1)
                        } else {
                            int_val(0)
                        }
                    }
                    None => int_val(0),
                },
                _ => Value::Null,
            }
        }
        Value::Assoc(rc) => {
            let mut m = rc.borrow_mut();
            match op {
                CollOp::Size | CollOp::Num => int_val(m.len() as u64),
                CollOp::Exists => {
                    let present = args
                        .first()
                        .map(|&a| m.contains_key(&value_to_key(&regs[a as usize])))
                        .unwrap_or(false);
                    int_val(present as u64)
                }
                CollOp::Delete => {
                    match args.first() {
                        Some(&a) => {
                            m.remove(&value_to_key(&regs[a as usize]));
                        }
                        None => m.clear(),
                    }
                    Value::Null
                }
                CollOp::First => {
                    let key = m.keys().next().cloned();
                    if let (Some(&arg), Some(key)) = (args.first(), key) {
                        regs[arg as usize] = key_to_value(&key);
                        int_val(1)
                    } else {
                        int_val(0)
                    }
                }
                CollOp::Last => {
                    let key = m.keys().next_back().cloned();
                    if let (Some(&arg), Some(key)) = (args.first(), key) {
                        regs[arg as usize] = key_to_value(&key);
                        int_val(1)
                    } else {
                        int_val(0)
                    }
                }
                CollOp::Next => match args.first() {
                    Some(&arg) => {
                        let current = value_to_key(&regs[arg as usize]);
                        let key = m
                            .range((
                                std::ops::Bound::Excluded(current),
                                std::ops::Bound::Unbounded,
                            ))
                            .next()
                            .map(|(key, _)| key.clone());
                        if let Some(key) = key {
                            regs[arg as usize] = key_to_value(&key);
                            int_val(1)
                        } else {
                            int_val(0)
                        }
                    }
                    None => int_val(0),
                },
                CollOp::Prev => match args.first() {
                    Some(&arg) => {
                        let current = value_to_key(&regs[arg as usize]);
                        let key = m.range(..current).next_back().map(|(key, _)| key.clone());
                        if let Some(key) = key {
                            regs[arg as usize] = key_to_value(&key);
                            int_val(1)
                        } else {
                            int_val(0)
                        }
                    }
                    None => int_val(0),
                },
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    }
}

/// A backend that turns IR programs into runnable processes. The interpreter is
/// one implementation; a JIT is a drop-in alternative (see module docs).
pub trait ExecBackend {
    /// Instantiate `prog` (a process body) with access to the design's shared
    /// `linkage` (functions + classes), producing a runnable process.
    fn instantiate(&self, prog: Rc<Program>, linkage: Rc<Linkage>) -> Box<dyn Process>;
    /// Backend name (for logs / perf reports).
    fn name(&self) -> &str;
}

/// The tree-free interpreter backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Interp;

impl ExecBackend for Interp {
    fn instantiate(&self, prog: Rc<Program>, linkage: Rc<Linkage>) -> Box<dyn Process> {
        Box::new(IrProcess::new(prog, linkage))
    }

    fn name(&self) -> &str {
        "interp"
    }
}
