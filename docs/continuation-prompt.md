# Continuation Prompt

Continue implementing the standalone Rust SystemVerilog simulator in
`C:\Users\dellerbr\eevee-rs-standalone` toward full IEEE Std 1800-2023
compliance.
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
- `cargo test --workspace` passes all 174 tests.
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
- The normative target and narrow feature statuses are tracked in
  `docs/conformance.md`; no whole-standard percentage is claimed.
- `parse_source_conformant` rejects unsupported CST, unsupported continuous
  assignment/net forms, unknown system calls, incomplete port actuals, and
  non-signal connection expressions, as well as Verible parser-recovery trees,
  with preprocessed-source line/column diagnostics.
- `elaborate_conformant` rejects semantic preflight failures, elaboration
  panics, and every resilient callable stub. `ElabStats::callable_stubs` retains
  qualified names and reasons; permissive APIs remain UVM exploration tools.
- Strict hierarchy preflight rejects duplicate/unknown names, cycles, and port
  width conversion before recursive allocation.
- ANSI scalar/packed module ports and recursively scoped child instances
  support named and positional whole-signal connections. A strict end-to-end
  test validates delayed propagation through both forms.
- Constant initialization evaluates every unary/binary operator represented by
  the current AST instead of returning an operand for unsupported cases.
- Explicit/inherited bare 32-bit `int` module value parameters now support
  ordered defaults and named/positional overrides. Parent-parameter
  expressions feed child overrides; per-instance constants drive net
  initialization, `initial`/`always` expressions, and delays.
- The narrow sequential RTL core is source-to-runtime validated:
  `always #delay`, `always_ff @(posedge clk)`, nonblocking updates, and NBA old-value
  behavior execute correctly. This does not imply broad synthesizable-Verilog
  compatibility; generate, complete typing/sizing, memories, and full port/net
  semantics remain major gaps.
- Strict parsing/elaboration reject type or non-`int` module parameters,
  header/body localparams, module-body parameters,
  nonliteral/parameter-dependent packed widths,
  unknown/duplicate/excess overrides, and unresolved/nonconstant parameter
  expressions.
- Default-strong whole-net continuous assignments over the supported internal
  net types execute as reactive IR processes with optional one-, two-, or
  three-value inertial delays.
  Scheduler-owned driver slots resolve Z, X, and conflicting 0/1 values; child
  output wire nets alias and drive parent nets.
- Continuous assignments now derive a deduplicated RHS `NetId` read set and
  park on only those sources. Constant RHS drivers evaluate once. Unrelated
  writes no longer resume continuous processes.
- Generic `Wait::Cond`/`Wait::Sensitivity` registration now uses process wait
  generations and reverse net tracking, preventing duplicate wakeups and
  removing stale quiet-sibling waiters after the first source fires.
- Continuous assignments accept one common delay, rise/fall delays, or
  rise/fall/turn-off delays as known nonnegative timescale-relative parameter
  expressions. Two-value turn-off uses `min(rise, fall)`. The typed time wheel
  selects delays and tracks inertial generations per bit, so mixed vector
  transitions complete independently, changed requests cancel only their bit,
  identical requests retain their deadlines, and fully canceled events are
  pruned before time advancement. Zero-delay bits apply immediately.
- Internal net declarations accept the same one-, two-, and three-value delay
  forms as known nonnegative parameter expressions. A distinct post-resolution
  net-delay stage tracks inertial generations per bit, so multi-driver
  resolution occurs first; mixed zero/nonzero bits, invalid operands, and
  assignment-plus-net delay composition have strict tests.
- Internal `tri` nets now share ordinary wire resolution;
  `wand`/`triand` and `wor`/`trior` use strengthless wired-AND and wired-OR
  resolution. Strict tests cover Z/X behavior and dominating 0/1 values.
- Ordinary wire resolution records separate logic-0 and logic-1 strengths per
  driver. Explicit continuous-assignment pairs support high-Z, weak, pull,
  strong, and supply levels in either source order. The strength-qualified
  interval is retained through X resolution; tests cover equal conflicts,
  asymmetric X, supply dominance, vectors, and three-driver order independence.
  Drivers without an explicit pair remain strong/strong.
- Internal `tri0`/`tri1` nets elaborate with implicit full-width pull-strength
  0/1 drivers. `supply0`/`supply1` elaborate with implicit supply-strength 0/1
  drivers. A strict end-to-end test checks default values, strong overrides of
  pulls, and supply dominance over strong sources.
- Strict parsing exports Verible tokens alongside the CST. This closes a
  Verible gap where explicit drive-strength tokens are omitted from the CST:
  supported drive pairs are attached to continuous assignments by the `assign`
  token's byte offset. Permissive parsing selectively requests tokens only when
  an exact strength keyword is present, preserving its normal tree-only UVM
  path. Charge strengths remain rejected; declaration-position
  `supply0`/`supply1` remains a supported internal net type.
- Strict mode rejects net declaration assignments, procedural or signed net
  drives, implicit width conversion, resolved net ports, explicit strengths on
  wired-AND/OR nets, charge strengths, explicit-unit delay literals,
  negative/X/Z/nonconstant delay operands, dynamic/out-of-range RHS selects,
  partial targets, unknown RHS names, and unsupported RHS calls.

The module connectivity model currently aliases a scheduler net. Full port
directionality, complete net/variable port semantics, and width-converting or
expression connections are not claimed.

## Next Priorities

1. Extend continuous connectivity with resolved net ports and driver release.
  Then extend module parameters into parameter-dependent packed widths and
  complete value typing/coercion.
2. Add hierarchical references and generate `if`/`case`/`for`, preserving
  scoped instance identity and adding explicit top selection.
3. Carry source spans/maps through preprocessing, AST, elaboration, and runtime
  so strict semantic diagnostics are source-located rather than only
  actionable by callable or signal name.
4. Make normal named test selection work: `run_test("my_test")` and
   `+UVM_TESTNAME=my_test`, using the real UVM registry/factory source. Add a
   focused probe that does not manually construct `uvm_test_top`.
5. Verify all common UVM callbacks and the final report summary. Trace any
   missing callback through virtual dispatch and phase traversal rather than
   adding UVM-specific runtime behavior.
6. Implement real IEEE `process::self`, status transitions, await/kill, and
   phase-worker teardown. Keep process support generic and scheduler-owned.
7. Reduce the highest UVM compile-stub buckets with focused language tests:
   callback collection typing, `uvm_typeid_base::typename`, nested indexed
   receiver typing, collection `foreach`, process status enums, and generic
   min/max methods.
8. Expand DPI only at the generic boundary. The UVM regex, polling, and HDL
   backdoor symbols are currently intentionally unbound; unknown imports must
   fail explicitly rather than return a plausible zero.
9. Continue the README roadmap through hierarchy/generate/interfaces,
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
