# eevee-rs

A native Rust SystemVerilog simulator targeting IEEE Std 1800-2023 semantics
and unmodified Accellera UVM execution.

This is the active successor to the
[reference-only Python `eevee` executable specification](https://github.com/intel-sandbox/Eevee).
The Rust implementation replaces coroutine-per-process scheduling, polling
waits, and runtime name lookup with an event-driven kernel and register
bytecode interpreter.

The Python predecessor receives no new feature work. Its GitHub repository
archive setting is managed separately and is not changed by this repository.

> **Development status:** pre-alpha, not a signoff simulator. The complete
> source-to-execution pipeline works, real UVM source preprocesses and
> elaborates, real UVM reporting runs, and the real registry/factory path now
> creates objects. The real UVM phase graph now executes a user test's
> `run_phase` through a delayed objection lifecycle. A fail-closed conformance
> mode and a minimal parent/child module hierarchy now work. Full IEEE 1800
> compliance and broader UVM/RTL workflows remain in progress.

The clause/domain-indexed [conformance matrix](docs/conformance.md) is the
source of truth for standards claims. Callable compilation percentages and
permissive UVM runs are progress telemetry, not conformance measurements.

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

Validated on July 23, 2026:

- 174 Rust tests pass across core logic, scheduling, parsing, elaboration, IR,
  classes, parameterization, collections, statics, and concurrency.
- `cargo fmt --all -- --check` passes.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- The event-driven hand-written counter kernel sustains roughly 9.5 million
  cycles/second; the bytecode interpreter baseline is roughly 5.5 million
  cycles/second on the development workstation.
- The real UVM library lays out 680 classes and compiles 7,284 of 7,535
  callables (96.7%). Unsupported callables are isolated as resilient compile
  stubs so the rest of the library remains executable while language coverage
  grows.
- The real UVM report server executes end to end and emits native UVM report
  lines.
- Real `type_id::create` crosses the unmodified
  `uvm_object_registry -> uvm_default_factory -> override lookup ->
  create_object_by_type` path and constructs the requested object.
- Imported DPI-C functions compile to generic host calls. A per-simulation
  registry supplies UVM command-line iteration and tool identity; callers can
  register additional host functions without changing the interpreter.
- The `uvm_run_test` probe elaborates 683 classes and compiles 7,308 of 7,559
  callables. Real UVM traversal invokes `my_test::run_phase`, reports at time
  zero, resumes after `#10`, reports at 10,000,000 fs, drops its objection, and
  exits normally.
- Conformance parsing rejects Verible recovery trees, known unsupported CST
  paths, and unknown system calls with line/column diagnostics; conformant
  elaboration rejects semantic fallbacks, hierarchy cycles/width conversion,
  panics, placeholder builtins, and every resilient callable stub.
- ANSI packed/scalar ports and recursive child module instances elaborate with
  named or positional whole-signal connectivity. A strict end-to-end test
  validates delayed parent-to-child-to-parent propagation through both forms.
- Explicit/inherited bare 32-bit `int` module value parameters support
  declaration-order defaults plus named and positional instance overrides.
  Per-instance values drive constant initializers, `initial`/`always`
  expressions, and delays.
- A source-level synchronous counter validates `always #delay`,
  `always_ff @(posedge clk)`, nonblocking assignment, and NBA settling. Focused
  swap tests confirm simultaneous NBAs read old values. This is a usable narrow
  sequential RTL core, not broad synthesizable-Verilog compatibility.
- Default-strong whole-net continuous assignments over the supported internal
  net types execute as reactive processes with optional one-, two-, or
  three-value inertial delays.
  Scheduler-owned drivers resolve Z, X, and conflicting unsigned values across
  hierarchy.
- Continuous assignments park on deduplicated RHS net read sets, so unrelated
  writes do not wake them. Generic multi-net waits invalidate and remove sibling
  registrations after the first source fires.
- Known nonnegative timescale-relative continuous assignment delays support
  one common value, rise/fall values, or rise/fall/turn-off values. Transition
  selection is per bit, two-value turn-off uses `min(rise, fall)`, short pulses
  are rejected independently per bit, identical requests retain their pending
  deadlines, and fully canceled events do not advance simulation time.
- Internal net declarations accept the same one-, two-, and three-value delay
  forms. These delays apply per bit to the resolved net value after all driver
  and strength resolution; driver and net delays compose as separate stages.
- Internal `tri` nets use ordinary wire resolution; `tri0`/`tri1` add an
  implicit pull-strength driver, and `supply0`/`supply1` add an implicit
  supply-strength driver. Ordinary continuous drivers support separate logic-0
  and logic-1 `highz`/`weak`/`pull`/`strong`/`supply` strengths, including
  asymmetric X ranges and equal-strength conflicts. `wand`/`triand` and
  `wor`/`trior` retain default-strong wired-AND and wired-OR resolution.

## Implemented Language Surface

The current implementation includes:

- Four-state `LogicVec` values with narrow and wide representations,
  arithmetic, shifts, comparisons, reductions, slicing, concatenation, and
  X/Z propagation.
- Femtosecond simulation time and an event-driven Active/Inactive/NBA kernel.
- Blocking/nonblocking assignments, delays, edge/event waits, and event-driven
  `wait(condition)` read sets, including procedural NBA expression events.
- Packed bit/part selects, numeric concatenation, `do...while`, `break`, and
  `continue` control flow.
- Register bytecode with suspendable process frames and recursive function/task
  calls.
- Verible JSON-CST front end and recursive SystemVerilog preprocessing
  (`include`, macros, conditionals, stringize/paste, `__FILE__`, `__LINE__`).
- Packages, compilation-unit declarations, functions, tasks, extern method
  bodies, and class-scoped static calls.
- Classes, constructors, inheritance, virtual dispatch, `super`, strings,
  static fields, constant property initializers, inherited virtual overrides,
  and shared object handles.
- Parameterized-class monomorphization, type/value parameters, typedef
  substitution, and independent specialized static state.
- Queues, dynamic arrays, associative arrays, object-handle keys, collection
  methods, indexed access, and `foreach`.
- Unpacked struct typedefs needed by UVM factory override records.
- Formal `input`, `output`, `inout`, and `ref` directions with copy-back through
  normal and virtual calls.
- `fork/join`, `join_any`, and `join_none` as real scheduler processes.
- Imported DPI-C callables with injectable, per-simulation host bindings and
  `input`/`output`/`inout` value transfer.
- ANSI module port metadata, root-module discovery, recursively scoped child
  instances, and named/positional whole-signal port binding.
- Explicit/inherited bare `int` module parameter defaults and named/positional
  value overrides with per-instance constant scopes.
- Plain `wire`/`tri`, implicit `tri0`/`tri1` and `supply0`/`supply1`,
  `wand`/`triand`, and `wor`/`trior` nets; reactive continuous assignment
  processes; and symmetric or explicit/asymmetric strength-aware ordinary-net
  resolution.
- Precise continuous-assignment sensitivity and generation-safe multi-net wait
  registration/cleanup.
- One-, two-, and three-value inertial continuous assignment delays with
  per-instance parameter expressions, per-bit transition selection, and
  zero-delay immediate application.
- One-, two-, and three-value internal net-declaration delays applied after
  resolution, including mixed immediate/delayed bits and composition with
  continuous-assignment delays.
- Fail-closed `parse_source_conformant` and `elaborate_conformant` APIs. The
  legacy APIs remain permissive for coverage exploration.
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
               hierarchy, procedural AST -> register bytecode
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

The strict in-process path is:

```rust
let ast = eevee_fe::parse_source_conformant(source)?;
let sim = eevee_elab::elaborate_conformant(&ast, &eevee_ir::Interp)?;
```

Strict parsing rejects the enforced unsupported CST/fallback paths before
best-effort lowering can drop them or substitute zero. Strict elaboration
rejects unsupported semantic forms, module-elaboration panics, and resilient
callable stubs. Diagnostics discovered after preprocessing currently refer to
the preprocessed source; post-AST semantic errors do not yet retain source
spans.

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

## Roadmap to IEEE 1800-2023 Compliance

The project advances by observable conformance slices: add a focused SV test,
implement the owning language/runtime abstraction, run the full Rust suite,
then rerun the real UVM frontier. UVM behavior is never substituted.

The [conformance matrix](docs/conformance.md) uses the explicit progression
`unsupported -> parsed -> elaborated -> executed -> behaviorally validated`.
Statuses apply only to the narrow feature in each row, never to a whole clause.

1. **Standards-grade conformance spine**
   - Extend source spans through preprocessing, AST, elaboration, and runtime
     diagnostics; reject every remaining skip/zero/no-op path in strict mode.
   - Add chapter-organized positive/negative tests, differential oracles, and
     machine-readable evidence without publishing a whole-standard percentage.

2. **Core elaboration and hierarchy**
   - Build on the ANSI-port/child-instance/integral-parameter slice with
     resolved net ports, driver release, hierarchical references, and generate
     `if`/`case`/`for`.
   - Add explicit top selection, port direction/type semantics, width
     conversion, nets versus variables, and instance arrays.

3. **Complete UVM execution path**
   - Support named `run_test("my_test")`, `+UVM_TESTNAME`, and normal static
     registry discovery without manually constructing `uvm_test_top`.
   - Complete report-summary, phase-worker teardown, process handle/status,
     mailbox, semaphore, and remaining objection/event edge semantics.
   - Run representative UVM factory, TLM, sequence, config-db, and callback
     examples through unmodified source.

4. **Complete type and expression semantics**
   - Signedness and sizing/coercion rules, packed/unpacked arrays, structs,
     unions/tagged unions, enums, casts, streaming concatenation, assignment
     patterns, wildcard equality, inside/dist, and iterator/query methods.
   - Automatic/static lifetime, ref aliasing, default arguments, protected/
     local members, interfaces/virtual classes, pure virtual methods, and
     nested classes.

5. **Concurrency and runtime primitives**
   - Full process handles/status/control, disable fork/named blocks, event
     trigger variants, wait fork/order, semaphore/mailbox fairness, named
     events, force/release, procedural continuous assignment, and all scheduler
     regions including observed/reactive/postponed behavior.

6. **Randomization and constraints**
   - `rand`/`randc`, deterministic process RNG state, inline/class constraints,
     solve-before, soft, dist, implication, foreach constraints, array sizing,
     and a solver backend with reproducible seeds.

7. **Assertions and coverage**
   - Immediate and concurrent assertions, sampled-value functions, sequences,
     properties, local variables, repetition, multi-clock operation, assertion
     control, covergroups, bins, crosses, options, and coverage reporting.

8. **DPI, files, waves, and tooling**
   - ABI-correct native-library DPI-C import/export and open arrays,
     UVM regex/polling/HDL backdoor bindings, VPI/backdoor APIs,
     complete file/system tasks, VCD/FST waveforms, plusargs, diagnostics,
     source locations, lint-quality errors, and deterministic replay.

9. **Conformance and performance closure**
   - Run chapter-organized IEEE tests, sv-tests, UVM examples, and OpenTitan
     vertical slices with tracked pass rates.
   - Profile before optimizing; add opcode fusion/typed slots and optional
     Cranelift JIT only where measurements justify it.
   - Define release gates for semantics, determinism, diagnostics, portability,
     and performance before claiming broad IEEE 1800 compliance.

## Known Limitations

- This is not yet a complete IEEE 1800 implementation or a production/signoff
  simulator.
- The current hierarchy slice supports ANSI packed/scalar ports and simple
  whole-signal named/positional connections. Ports alias a scheduler net;
  complete directionality, net/variable distinctions, width conversion,
  expression actuals, hierarchy references, and generate remain unsupported.
- Continuous assignment support is limited to internal unsigned `wire`/`tri`,
  `tri0`/`tri1`, `supply0`/`supply1`, `wand`/`triand`, and `wor`/`trior` nets;
  whole-net LHS targets; exact-width represented unsigned signal/literal RHS
  expressions; default or explicit `highz`/`weak`/`pull`/`strong`/`supply`
  strengths on ordinary continuous drivers; and optional one-, two-, or
  three-value timescale-relative inertial delays. Resolved net ports, net
  declaration assignments, procedural net writes, implicit width conversion,
  signed operands/targets, charge strengths, nondefault wired-net strengths,
  explicit-unit delay literals,
  dynamic/out-of-range RHS selects, partial/hierarchical targets, and driver
  release remain unsupported in conformance mode.
- Module parameter support is limited to explicit or inherited bare 32-bit
  `int` value parameters in the module header, each with a default. Type
  parameters, non-`int` types, module-body parameter/localparam declarations,
  header localparams, omitted actuals, `defparam`, nonliteral packed declaration
  bounds, and parameter-dependent packed widths remain unsupported in
  conformance mode.
- The passing phase probe constructs `uvm_test_top` directly and calls
  `run_test("")`; named test selection and plusarg-driven factory registration
  are not yet complete.
- Some UVM callables still use resilient compile stubs while missing language
  features are implemented. Permissive mode retains them for exploration;
  conformance mode rejects all of them. Treat compile percentage as progress
  telemetry, not proof of behavioral correctness.
- Default DPI bindings currently cover UVM argv iteration and tool identity.
  UVM regex, polling, and HDL backdoor imports remain unbound unless supplied
  through the host registry.
- Nonconstant class property initializer execution and full process-handle
  lifecycle semantics remain incomplete.
- Several hierarchy, interface, generate, randomization, assertion, coverage,
  DPI, and waveform features remain incomplete or absent.

## License Notes

The vendored `uvm-core/` tree retains its upstream Accellera license and notice.
Review upstream licenses before redistributing vendored dependencies.
