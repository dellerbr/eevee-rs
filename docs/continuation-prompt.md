# Continuation Prompt

Continue implementing the standalone Rust SystemVerilog simulator in
`C:\Users\dellerbr\eevee-rs-standalone` toward full IEEE 1800 compliance.
Work autonomously through implementation, focused tests, full validation,
documentation, commit, and HTTPS push.

## Non-Negotiable Boundary

- Execute the actual vendored UVM SystemVerilog source unmodified.
- Never intercept a UVM class or method name and emulate its behavior in Rust.
- Native Rust is allowed for IEEE language/runtime primitives and generic host
  facilities: scheduling, process handles, events, collections, DPI, files,
  randomization engines, and system tasks.
- When UVM exposes a failure, trace the real SV statement and fix the owning
  language, elaboration, IR, interpreter, scheduler, or host-service layer.
- Keep `git diff -- uvm-core` empty when finished.

## Verified Baseline

- Branch: `main`; remote: `https://github.com/dellerbr/eevee-rs.git`.
- Toolchain: stable `x86_64-pc-windows-gnu`; prepend
  `$env:USERPROFILE\.cargo\bin` to `PATH`.
- `cargo fmt --all -- --check` passes.
- `cargo test --workspace` passes all 125 tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- `uvm_elab`: 680 classes, 7,284/7,535 callables compiled (96.7%).
- `uvm_run_test`: 683 classes, 7,308/7,559 callables compiled. Real UVM calls
  `my_test::run_phase`, emits `MYTEST` at 0 and 10,000,000 fs, and exits.

The passing probe deliberately creates `new("uvm_test_top", null)` and calls
`run_test("")`. The factory itself has separately crossed the real
`type_id::create -> uvm_default_factory -> create_object_by_type` path.

## Recently Implemented

- IEEE array assignment-copy semantics.
- Packed bit/part selects and numeric concatenation.
- Procedural NBA updates and value-change expression events.
- `do...while` with correct `continue` behavior.
- Generic DPI import AST metadata, `DpiCall` IR, and an injectable
  per-simulation `DpiRegistry`.
- Default host bindings for UVM argv iteration and tool name/version.
- Inherited virtual methods remain virtual when an override omits the keyword.
- Constant class property initializers are established before constructors.
- Real UVM phase traversal, delayed `run_phase`, and objection completion.

## Next Priorities

1. Make normal named test selection work: `run_test("my_test")` and
   `+UVM_TESTNAME=my_test`, using the real UVM registry/factory source. Add a
   focused probe that does not manually construct `uvm_test_top`.
2. Verify all common UVM callbacks and the final report summary. Trace any
   missing callback through virtual dispatch and phase traversal rather than
   adding UVM-specific runtime behavior.
3. Implement real IEEE `process::self`, status transitions, await/kill, and
   phase-worker teardown. Keep process support generic and scheduler-owned.
4. Reduce the highest UVM compile-stub buckets with focused language tests:
   callback collection typing, `uvm_typeid_base::typename`, nested indexed
   receiver typing, collection `foreach`, process status enums, and generic
   min/max methods.
5. Expand DPI only at the generic boundary. The UVM regex, polling, and HDL
   backdoor symbols are currently intentionally unbound; unknown imports must
   fail explicitly rather than return a plausible zero.
6. Continue the README roadmap through hierarchy/generate/interfaces,
   signedness and sizing, runtime primitives, constraints/randomization,
   assertions/coverage, files/waves, and chapter-based conformance closure.

## Validation Commands

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
Set-Location C:\Users\dellerbr\eevee-rs-standalone
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -q -p eevee-elab --example uvm_elab
cargo run -q -p eevee-elab --example uvm_run_test
git diff --check
git diff -- uvm-core
```

Use a process-level timeout around `uvm_run_test` while debugging. Useful
diagnostics are `EEVEE_TRACE=1`, `EEVEE_DUMP_STUBS=1` (or a callable-name
substring), and `EEVEE_DUMP_GLOBALS=1`.
