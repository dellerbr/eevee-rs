# eevee-rs

A from-scratch **Rust** SystemVerilog simulator, ported from the Python `eevee`
reference (`../eevee`). Goal: a production-usable simulator that

1. executes the **actual Accellera UVM 1800.2-2020 SystemVerilog source**
   unmodified (no UVM re-implementation, no shims),
2. simulates real synthesizable RTL at signal level (4-state, IEEE 1800-2017
   stratified event queue),
3. is fast enough for the OpenTitan DV suite (millions of cycles per test).

The Python `eevee` is a correct-but-slow **executable specification**. This port
keeps the semantics it proved out and replaces the three things that capped its
speed: coroutine-per-process scheduling, busy-poll `wait`, and per-access name
resolution.

## Status

| Phase | Scope | State |
| --- | --- | --- |
| P0 | Skeleton + CI, 4-state bitvec, fs time, stratified regions, front-end decision | in progress |
| P1 | Event-driven scheduler MVP (sensitivity lists, NBA, `always_ff`, `#delay`, event-woken `wait`) + counter benchmark | done (~9.5 M cyc/s) |
| P2 | Procedural interpreter + class runtime | in progress (full pipeline **SV source → run** landed via register IR; classes next) |
| P3 | Load & run real UVM library (no-shims proof) | — |
| P4 | Randomization + constraints (Z3), SVA, coverage | — |
| P5 | DPI-C real FFI + backdoor (`uvm_hdl_*`, force/release) | — |
| P6 | OpenTitan vertical slice (RAL frontdoor/backdoor) | — |
| P7 | Scale-up + bytecode/IR perf (JIT via Cranelift behind the backend seam) | — |

## Layout

``` layout
crates/
  eevee-core/   4-state LogicVec, SimTime (fs), Timescale, Region (stratified queue)
  eevee-sched/  event-driven scheduler: nets + sensitivity lists, processes, NBA
  eevee-ir/     register bytecode IR + interpreter (ExecBackend seam for a future JIT)
  eevee-ast/    the SystemVerilog AST schema
  eevee-fe/     front-end: Verible subprocess (JSON CST) + CST→AST lowering
  eevee-elab/   elaborator: AST → nets + IR-compiled processes (a runnable Sim)
docs/
  frontend-decision.md   slang vs Verible — Verible (subprocess+JSON) chosen for v0
  perf-log.md            cycles/sec trend, recorded every phase
```

The full pipeline is wired end to end: `eevee_fe::parse_source(sv)` →
`eevee_elab::elaborate(&ast, &backend)` → a `Sim` you can `run`. See
`crates/eevee-elab/tests/end_to_end.rs` for the counter running from `.sv` text.

## Build & test

```powershell
# Rust toolchain is at %USERPROFILE%\.cargo\bin (may not be on PATH)
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
cd eevee-rs
cargo test
cargo run --release -p eevee-sched --example counter_bench   # P1 hand-coded cycles/sec
cargo run --release -p eevee-ir    --example counter_ir_bench # P2 IR-interpreted cycles/sec
```

## Non-negotiable rule

**No shims.** The simulator must execute real UVM SV for UVM constructs. Only
IEEE 1800-2017 *language* builtins (`process`, `semaphore`, `mailbox`, `event`,
`wait`, `$display`, queues, randomize, SVA, the scheduler) may be native Rust.
See `../eevee`'s memory notes and `docs/frontend-decision.md`.
