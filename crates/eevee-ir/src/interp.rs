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
use eevee_sched::{Kernel, Process, Wait};

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
}

/// What running one frame to its next transition produced.
enum Step {
    /// Suspend the whole process on this wait.
    Wait(Wait),
    /// Push a new frame: call `func` with `vals` as its leading registers,
    /// returning into the caller's `ret_dst`.
    Call {
        func: u32,
        vals: Vec<Value>,
        ret_dst: Reg,
    },
    /// Pop the current frame, delivering `Some(value)` to the caller's
    /// `ret_dst` (or nothing for a void return).
    Return(Option<Value>),
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
            callers: Vec::new(),
            linkage,
        }
    }

    /// The core resume loop. Wrapped by [`Process::resume`], which catches any
    /// runtime fault and prints an SV-level call stack ([`Self::sv_backtrace`])
    /// before letting the panic propagate.
    fn run(&mut self, k: &mut Kernel) -> Wait {
        loop {
            match run_frame(&self.prog, &mut self.pc, &mut self.regs, &self.linkage, k) {
                Step::Wait(w) => return w,
                Step::Call {
                    func,
                    vals,
                    ret_dst,
                } => {
                    let callee = self.linkage.funcs[func as usize].clone();
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
                    for (i, v) in vals.into_iter().enumerate() {
                        if i < regs.len() {
                            regs[i] = v;
                        }
                    }
                    // Save the current frame and switch to the callee.
                    let prev = Frame {
                        prog: std::mem::replace(&mut self.prog, callee),
                        pc: std::mem::replace(&mut self.pc, 0),
                        regs: std::mem::replace(&mut self.regs, regs),
                        ret_dst: std::mem::replace(&mut self.ret_dst, ret_dst),
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
                            if let Some(v) = val {
                                caller.regs[self.ret_dst as usize] = v;
                            }
                            self.prog = caller.prog;
                            self.pc = caller.pc;
                            self.regs = caller.regs;
                            self.ret_dst = caller.ret_dst;
                        }
                        // Returned past the bottom frame — the process is done.
                        None => return Wait::Finished,
                    }
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
    }
}

/// Run one frame from `*pc` until it transitions (suspends, calls, or returns),
/// updating `*pc` and `regs` in place.
fn run_frame(
    prog: &Program,
    pc: &mut usize,
    regs: &mut [Value],
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
            Inst::Delay { fs } => return Step::Wait(Wait::Delay(fs)),
            Inst::WaitEdge { net, edge } => return Step::Wait(Wait::Edge(net, edge)),
            Inst::WaitCond { nets } => {
                return Step::Wait(Wait::Cond(prog.netlists[nets as usize].to_vec()));
            }
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
            Inst::Display { fmt, args } => {
                let f = match &prog.consts[fmt as usize] {
                    Value::Str(s) => s.clone(),
                    _ => std::rc::Rc::from(""),
                };
                let line = format_display(&f, &prog.arglists[args as usize], regs);
                k.display(line);
            }
            Inst::Call { func, args, ret } => {
                let arglist = &prog.arglists[args as usize];
                let vals: Vec<Value> = arglist.iter().map(|r| regs[*r as usize].clone()).collect();
                return Step::Call {
                    func,
                    vals,
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
                    ret_dst: ret,
                };
            }
            Inst::Return { value } => return Step::Return(Some(regs[value as usize].clone())),
            Inst::ReturnVoid => return Step::Return(None),
            Inst::New { dst, class } => {
                let def = &linkage.classes[class as usize];
                let mut fields = def.field_defaults.to_vec();
                // Collection fields get fresh storage per instance (no aliasing).
                for &(slot, is_assoc) in def.coll_fields.iter() {
                    fields[slot as usize] = if is_assoc {
                        Value::new_assoc()
                    } else {
                        Value::new_queue()
                    };
                }
                regs[dst as usize] = Value::Obj(Rc::new(RefCell::new(ObjData { class, fields })));
            }
            Inst::GetField { dst, obj, slot } => {
                let v = match &regs[obj as usize] {
                    Value::Obj(rc) => rc.borrow().fields[slot as usize].clone(),
                    _ => Value::Null,
                };
                regs[dst as usize] = v;
            }
            Inst::SetField { obj, slot, src } => {
                let v = regs[src as usize].clone();
                if let Value::Obj(rc) = &regs[obj as usize] {
                    rc.borrow_mut().fields[slot as usize] = v;
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
                let v = regs[src as usize].clone();
                if trace_on() {
                    eprintln!("      sset static#{id} <- {} [{}]", vsum(&v), prog.label);
                }
                *linkage.statics[id as usize].borrow_mut() = v;
            }
            Inst::NewQueue { dst } => {
                regs[dst as usize] = Value::new_queue();
            }
            Inst::NewAssoc { dst } => {
                regs[dst as usize] = Value::new_assoc();
            }
            Inst::IndexGet { dst, base, idx } => {
                let v = match &regs[base as usize] {
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
            Inst::IndexSet { base, idx, src } => {
                let v = regs[src as usize].clone();
                match &regs[base as usize] {
                    Value::Queue(rc) => {
                        let i = regs[idx as usize].as_logic().to_u64() as usize;
                        let mut q = rc.borrow_mut();
                        if i >= q.len() {
                            q.resize(i + 1, Value::Null);
                        }
                        q[i] = v;
                    }
                    Value::Assoc(rc) => {
                        let key = value_to_key(&regs[idx as usize]);
                        rc.borrow_mut().insert(key, v);
                    }
                    _ => {}
                }
            }
            Inst::CollMethod {
                dst,
                base,
                op,
                args,
            } => {
                let arglist = &prog.arglists[args as usize];
                let result = eval_coll_method(&regs[base as usize], op, arglist, regs);
                regs[dst as usize] = result;
            }
            Inst::ConcatStr { dst, args } => {
                let arglist = &prog.arglists[args as usize];
                let mut s = String::new();
                for r in arglist.iter() {
                    s.push_str(&value_to_str(&regs[*r as usize]));
                }
                regs[dst as usize] = Value::Str(Rc::from(s.as_str()));
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
        Value::Obj(_) | Value::Queue(_) | Value::Assoc(_) => true,
        Value::Null => false,
        Value::Str(s) => !s.is_empty(),
        Value::Real(r) => *r != 0.0,
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
        (Value::Obj(_) | Value::Queue(_) | Value::Assoc(_), Value::Null)
        | (Value::Null, Value::Obj(_) | Value::Queue(_) | Value::Assoc(_)) => one(false),
        _ => one(false),
    }
}

/// Convert an index value into an associative-array key (int or string).
fn value_to_key(v: &Value) -> AssocKey {
    match v {
        Value::Str(s) => AssocKey::Str(s.clone()),
        Value::Logic(lv) => AssocKey::Int(lv.to_i64()),
        _ => AssocKey::Int(0),
    }
}

/// Evaluate a built-in queue/array/assoc method (`push_back`, `size`, ...).
fn eval_coll_method(base: &Value, op: CollOp, args: &[Reg], regs: &[Value]) -> Value {
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
                // first/last/next/prev write the key back to a ref arg, which the
                // current calling convention can't express; report only success.
                CollOp::First | CollOp::Last => int_val((!m.is_empty()) as u64),
                CollOp::Next | CollOp::Prev => int_val(0),
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
