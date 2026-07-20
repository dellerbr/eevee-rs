# eevee-rs

A native Rust SystemVerilog simulator targeting IEEE 1800 semantics and
unmodified Accellera UVM execution.

This is the active successor to the
[archived Python `eevee` executable specification](https://github.com/intel-sandbox/Eevee).
The Rust implementation replaces coroutine-per-process scheduling, polling
waits, and runtime name lookup with an event-driven kernel and register
bytecode interpreter.

> **Development status:** pre-alpha, not a signoff simulator. The complete
> source-to-execution pipeline works, real UVM source preprocesses and
> elaborates, real UVM reporting runs, and the real registry/factory path now
> creates objects. Full IEEE 1800 compliance and a complete UVM `run_test`
> remain in progress.

## Non-Negotiable Rule

**No UVM shims.** The simulator executes the actual SystemVerilog methods from
the vendored UVM library. It must not detect UVM class or method names and
replace their behavior with Rust implementations.

Native Rust implementations are allowed only for IEEE 1800 language/runtime
primitives such as process scheduling, events, mailboxes, semaphores, queues,
randomization, system tasks, and signal updates. When UVM exposes a missing
simulator behavior, the fix belongs in the language front end, elaborator, IR,
interpreter, or scheduler.

## Current Status

Validated on July 20, 2026:

- 101 Rust tests pass across core logic, scheduling, parsing, elaboration, IR,
  classes, parameterization, collections, statics, and concurrency.
- `cargo fmt --all -- --check` passes.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- The event-driven hand-written counter kernel sustains roughly 9.5 million
  cycles/second; the bytecode interpreter baseline is roughly 5.5 million
  cycles/second on the development workstation.
- The real UVM library lays out 676 classes and compiles 7,206 of 7,475
  callables (96.4%). Unsupported callables are isolated as resilient compile
  stubs so the rest of the library remains executable while language coverage
  grows.
- The real UVM report server executes end to end and emits native UVM report
  lines.
- Real `type_id::create` crosses the unmodified
  `uvm_object_registry -> uvm_default_factory -> override lookup ->
  create_object_by_type` path and constructs the requested object.
- The current `run_test` frontier is a zero-time loop while constructing the
  UVM common phase domain (`uvm_phase::add`); it has not yet reached the user
  test's `run_phase`.

## Implemented Language Surface

The current implementation includes:

- Four-state `LogicVec` values with narrow and wide representations,
  arithmetic, shifts, comparisons, reductions, slicing, concatenation, and
  X/Z propagation.
- Femtosecond simulation time and an event-driven Active/Inactive/NBA kernel.
- Blocking/nonblocking assignments, delays, edge/event waits, and event-driven
  `wait(condition)` read sets.
- Register bytecode with suspendable process frames and recursive function/task
  calls.
- Verible JSON-CST front end and recursive SystemVerilog preprocessing
  (`include`, macros, conditionals, stringize/paste, `__FILE__`, `__LINE__`).
- Packages, compilation-unit declarations, functions, tasks, extern method
  bodies, and class-scoped static calls.
- Classes, constructors, inheritance, virtual dispatch, `super`, strings,
  static fields, and shared object handles.
- Parameterized-class monomorphization, type/value parameters, typedef
  substitution, and independent specialized static state.
- Queues, dynamic arrays, associative arrays, object-handle keys, collection
  methods, indexed access, and `foreach`.
- Unpacked struct typedefs needed by UVM factory override records.
- Formal `input`, `output`, `inout`, and `ref` directions with copy-back through
  normal and virtual calls.
- `fork/join`, `join_any`, and `join_none` as real scheduler processes.
- UVM-oriented report formatting primitives including enum names, string
  concatenation, `$sformatf`, `$swrite`, `$cast`, and simulation time.

## Architecture

```text
SystemVerilog source
        |
        v
eevee-fe       preprocessing + Verible CST -> typed AST
        |
        v
eevee-elab     global declarations, class specialization, names -> IDs,
               procedural AST -> register bytecode
        |
        v
eevee-ir       register programs + resumable interpreter + future JIT seam
        |
        v
eevee-sched    nets, sensitivity lists, process table, time wheel, NBA
        |
        v
eevee-core     four-state values, time, timescale, event regions
```

| Crate | Responsibility |
| --- | --- |
| `eevee-core` | Four-state values, time, timescale, event regions |
| `eevee-sched` | Event-driven kernel, nets, waits, process scheduling |
| `eevee-ir` | Register bytecode, runtime values, interpreter, backend seam |
| `eevee-ast` | Typed SystemVerilog AST |
| `eevee-fe` | Preprocessor, Verible subprocess, CST lowering |
| `eevee-elab` | Global elaboration, specialization, bytecode generation |

The front end invokes Verible as a subprocess; simulation itself is a native
Rust process. A future Cranelift backend can implement the existing
`ExecBackend` interface for hot static RTL without changing scheduler
semantics. Dynamic UVM remains interpreted.

## Build and Validate

The development machine uses the GNU Windows target because no MSVC SDK/linker
is installed:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
Set-Location C:\path\to\eevee-rs
rustup override set stable-x86_64-pc-windows-gnu

cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Useful probes:

```powershell
cargo run --release -p eevee-sched --example counter_bench -- 10000000
cargo run --release -p eevee-ir --example counter_ir_bench -- 10000000
cargo run -q -p eevee-elab --example uvm_elab
cargo run -q -p eevee-elab --example uvm_run
cargo run -q -p eevee-elab --example uvm_run_test
```

Debugging controls:

- `EEVEE_TRACE=1` prints interpreted SV calls/returns and selected state.
- `EEVEE_DUMP_STUBS=1` lists resilient compile stubs; use a substring instead
  of `1` to filter by qualified callable name.
- `EEVEE_DUMP_GLOBALS=1` dumps package/global declarations; a substring
  filters the output.
- Runtime panics include an SV-level call stack.

The repository currently vendors the Windows Verible syntax binary and the
Accellera UVM source. Linux/macOS CI needs per-platform Verible binaries or an
installation/download step before front-end tests can be portable.

## Roadmap to IEEE 1800 Compliance

The project advances by observable conformance slices: add a focused SV test,
implement the owning language/runtime abstraction, run the full Rust suite,
then rerun the real UVM frontier. UVM behavior is never substituted.

1. **Complete UVM execution path**
   - Make common-domain phase graph construction terminate correctly.
   - Execute build/connect/elaboration/run/extract/check/report/final phases.
   - Implement complete objection, event, process, mailbox, and semaphore
     blocking semantics.
   - Run a user `uvm_test` through `run_phase` and final report summary.

2. **Core elaboration and hierarchy**
   - Module/interface ports, instances, parameter overrides, generate
     if/case/for, genvar scopes, hierarchy, bind, modports, clocking blocks,
     virtual interfaces, and robust hierarchical references.
   - Net types, continuous assignments, strengths/delays, primitives, UDPs,
     and scheduling across all IEEE event regions.

3. **Complete type and expression semantics**
   - Signedness and sizing/coercion rules, packed/unpacked arrays, structs,
     unions/tagged unions, enums, casts, streaming concatenation, assignment
     patterns, wildcard equality, inside/dist, and iterator/query methods.
   - Automatic/static lifetime, ref aliasing, default arguments, protected/
     local members, interfaces/virtual classes, pure virtual methods, and
     nested classes.

4. **Concurrency and runtime primitives**
   - Full process handles/status/control, disable fork/named blocks, event
     trigger variants, wait fork/order, semaphore/mailbox fairness, named
     events, force/release, procedural continuous assignment, and all scheduler
     regions including observed/reactive/postponed behavior.

5. **Randomization and constraints**
   - `rand`/`randc`, deterministic process RNG state, inline/class constraints,
     solve-before, soft, dist, implication, foreach constraints, array sizing,
     and a solver backend with reproducible seeds.

6. **Assertions and coverage**
   - Immediate and concurrent assertions, sampled-value functions, sequences,
     properties, local variables, repetition, multi-clock operation, assertion
     control, covergroups, bins, crosses, options, and coverage reporting.

7. **DPI, files, waves, and tooling**
   - ABI-correct DPI-C import/export and open arrays, VPI/backdoor APIs,
     complete file/system tasks, VCD/FST waveforms, plusargs, diagnostics,
     source locations, lint-quality errors, and deterministic replay.

8. **Conformance and performance closure**
   - Run chapter-organized IEEE tests, sv-tests, UVM examples, and OpenTitan
     vertical slices with tracked pass rates.
   - Profile before optimizing; add opcode fusion/typed slots and optional
     Cranelift JIT only where measurements justify it.
   - Define release gates for semantics, determinism, diagnostics, portability,
     and performance before claiming broad IEEE 1800 compliance.

## Known Limitations

- This is not yet a complete IEEE 1800 implementation or a production/signoff
  simulator.
- The current full-UVM runtime stalls while constructing the common phase
  domain; `run_phase` has not completed end to end.
- Some UVM callables still use resilient compile stubs while missing language
  features are implemented. Treat compile percentage as progress telemetry,
  not proof of behavioral correctness.
- Several hierarchy, interface, generate, randomization, assertion, coverage,
  DPI, and waveform features remain incomplete or absent.
- Forked branches currently capture `this` but do not yet provide general
  closure capture for arbitrary parent locals.

## License Notes

The vendored `uvm-core/` tree retains its upstream Accellera license and notice.
Review upstream licenses before redistributing vendored dependencies.
