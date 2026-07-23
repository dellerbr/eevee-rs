# IEEE 1800 Conformance Matrix

## Normative target

The simulator targets **[IEEE Std 1800-2023](https://standards.ieee.org/ieee/1800/7743/),
IEEE Standard for SystemVerilog--Unified Hardware Design, Specification, and
Verification Language**. IEEE lists this as the active edition, published
February 28, 2024, and superseding IEEE Std 1800-2017.

This matrix tracks narrow features, not whole clauses. A row at
`behaviorally validated` means only the stated slice has focused executable
evidence. It never means the entire referenced clause is implemented. The
project does not publish a whole-standard completion percentage.

## Status definitions

| Status | Meaning |
| --- | --- |
| `unsupported` | No conformant implementation. Conformance mode must reject the construct instead of silently accepting it. |
| `parsed` | The construct is preserved in the typed AST, but later semantics are not claimed. |
| `elaborated` | Names/types/storage are resolved into runtime structures, but execution behavior is not yet validated. |
| `executed` | At least one end-to-end probe reaches runtime behavior, without enough focused evidence for a behavioral claim. |
| `behaviorally validated` | Focused positive or negative tests check the stated behavior through the implemented pipeline. |

## Conformance mode

A conformant in-process flow uses both strict entry points:

```rust
let ast = eevee_fe::parse_source_conformant(source)?;
let sim = eevee_elab::elaborate_conformant(&ast, &eevee_ir::Interp)?;
```

The frontend rejects Verible parser-recovery trees, known unsupported CST
paths, and unknown system tasks/functions with 1-based line and column
diagnostics. The elaborator rejects unsupported semantic forms, hierarchy
cycles and width conversion, placeholder builtins, module-elaboration panics,
and every resilient callable stub. Qualified callable names and failure reasons
are retained in `ElabStats`.

The legacy `parse_source` and `elaborate` entry points remain intentionally
permissive for coverage exploration such as the unmodified UVM frontier. A
permissive run, a resilient callable stub, or a callable compilation percentage
is never conformance evidence.

Current diagnostic limits are explicit: locations refer to preprocessed text,
and semantic failures discovered after AST lowering do not yet retain a source
span. Closing that source-map/span gap is required before standards-grade
negative diagnostics can be claimed broadly.

## Matrix

| IEEE 1800-2023 clause/domain | Narrow tracked slice | Status | Evidence and exclusions |
| --- | --- | --- | --- |
| 4, scheduling | Active, Inactive, and NBA iteration; timed wakeups | `behaviorally validated` | Scheduler tests cover ordering and NBA application. Preponed, Observed, Reactive, Re-Inactive, Re-NBA, and Postponed integration remains unsupported. |
| 5, lexical source | Verible CST ingestion of the exercised subset | `executed` | All source-driven tests cross Verible. This is not a complete lexical-conformance suite. |
| 5/22, preprocessing | Object/function macros, conditionals, continuations, includes | `behaviorally validated` | `eevee-fe` preprocessor unit tests. Full directive semantics and source mapping remain incomplete. |
| 6, integral data | Four-state packed scalar/vector storage | `behaviorally validated` | `LogicVec` tests cover narrow/wide values and X/Z behavior. Complete sizing, signing, coercion, and all built-in types remain unsupported. |
| 7, aggregate data | Queues and associative arrays used by the current runtime | `behaviorally validated` | Collection tests cover indexing, copy, methods, object keys, and `foreach`. Complete fixed/dynamic arrays, structs, unions, and assignment patterns remain incomplete. |
| 8, classes | Construction, inheritance, virtual dispatch, `super`, fields, statics | `behaviorally validated` | Class/statics tests and real UVM execution. Access control, interfaces/virtual classes, nested classes, and full lifetime semantics remain incomplete. |
| 8, parameterized classes | Type/value specialization used by current probes | `behaviorally validated` | Parameterized-class tests. This status does not cover module parameters. |
| 9, procedural processes | `initial`, `always`, `always_ff`, timescale-relative constant delays, event waits | `behaviorally validated` | A source-level clocked counter validates `always #delay` and `always_ff @(posedge clk)` execution. End-to-end/procedural tests include per-instance parameter delay expressions. Multi-source waits use generation-safe registration and remove quiet sibling waiters after the first source fires. Explicit-unit delay literals, full process handles, `disable fork`, `wait fork`, and all process control remain unsupported. |
| 9, fork/join | `join`, `join_any`, and `join_none` | `behaviorally validated` | Focused fork tests cover timing, capture, events, and object sharing. |
| 10, assignments | Procedural blocking and nonblocking assignment subset | `behaviorally validated` | Procedural and scheduler tests include clocked NBA updates and simultaneous swaps that read old values before NBA settling. Force/release and procedural continuous assignment are unsupported. |
| 11, expressions | Represented arithmetic, bitwise, logical, comparison, shift, reduction, select, and concatenation operators | `behaviorally validated` | Core logic tests plus conformant constant-initializer regression. Complete IEEE sizing/sign rules and operator set remain incomplete. |
| 12, procedural control | Blocks, `if`, loops, `case`, `foreach`, `break`, `continue` | `behaviorally validated` | Procedural/collection tests. Unsupported statement CST fails strict parsing rather than becoming `Stmt::Null`. |
| 13, tasks/functions | Calls, recursion, defaults, directions, copy-back | `behaviorally validated` | Function and formatting tests. Automatic/static lifetime and all argument/type rules remain incomplete. |
| 14, clocking blocks | Clocking declarations and skew semantics | `unsupported` | Rejected by conformance mode; no runtime model. |
| 15, synchronization | Named events and value-change waits | `behaviorally validated` | Fork/event and procedural NBA tests. |
| 15, synchronization | Mailboxes and semaphores | `unsupported` | Permissive builtin placeholders are not conformance implementations and cannot pass strict elaboration when they induce stubs/unsupported behavior. |
| 16, assertions | Immediate and concurrent assertions/SVA | `unsupported` | No conformance claim. |
| 17, checkers | Checker declarations/semantics | `unsupported` | No conformance claim. |
| 18, constrained random | `rand`, `randc`, `randomize`, constraints | `unsupported` | No solver or standards claim. |
| 19, functional coverage | Covergroups, bins, crosses, options | `unsupported` | No conformance claim. |
| 20, utility system calls | Exercised formatting, time, type-name, and cast subset | `behaviorally validated` | Formatting tests. Unknown calls fail strict frontend validation with a source coordinate. |
| 21, file/system I/O | General files and I/O tasks | `unsupported` | `$display`/`$write` capture is a narrow Clause 20/21 overlap, not file-I/O support. |
| 23, module declarations | ANSI scalar/packed logic and built-in resolved-net input/output/inout/ref port metadata | `behaviorally validated` | Frontend regressions check direction, width, and canonical `wire`/`tri`, `wand`/`triand`, `wor`/`trior`, `tri0`/`tri1`, and `supply0`/`supply1` kinds. Non-ANSI ports and full port-kind/type rules remain unsupported. |
| 23, hierarchy | Root discovery and one-or-more recursively elaborated child instances | `behaviorally validated` | Strict end-to-end test runs two child instances; a negative test rejects cycles. Explicit top selection remains unsupported. |
| 23, connectivity | Named and positional whole-signal connections; matching-resolution resolved-net port collapse | `behaviorally validated` | Strict end-to-end tests check procedural and continuous propagation plus resolved input, output, and inout ports across two child instances and a grandchild. Canonical synonyms collapse onto one scheduler net; unbound root pull ports install one implicit driver. Resolution mismatches, non-net output/inout actuals, and explicit strengths on wired ports fail preflight. Empty, implicit, wildcard, expression, select, interface, hierarchical, and width-converting actuals remain rejected. Full directionality, cross-resolution bridges, and complete variable/net port semantics are not claimed. |
| 23, module parameters | Explicit/inherited bare 32-bit `int` value defaults and named/positional overrides | `behaviorally validated` | Strict tests cover declaration-order dependent defaults, parent-parameter override expressions, independent child initializer and `initial` values, parameterized delays, and invalid override diagnostics. Type parameters/actuals, non-`int` parameter types, missing defaults, header or body localparams, module-body parameters, omitted actuals, `defparam`, nonliteral packed bounds, and parameter-dependent packed widths remain unsupported. |
| 23, hierarchical names | Procedural hierarchical references | `unsupported` | Dotted net names exist for diagnostics/inspection, but IEEE hierarchical lookup is not implemented. |
| 23/28, continuous connectivity | Internal and matching collapsed-port `wire`/`tri`, `tri0`/`tri1`, `supply0`/`supply1`, `wand`/`triand`, and `wor`/`trior`; default or explicit-strength ordinary whole-net assignments and internal net declarations with optional one-, two-, or three-value timescale-relative inertial delays; exact-width unsigned represented RHS expressions | `behaviorally validated` | Strict tests cover ordinary, wired-AND, and wired-OR four-state resolution; symmetric and asymmetric `highz`/`weak`/`pull`/`strong`/`supply` ordinary-driver resolution; implicit full-width pull/supply drivers; time-zero/zero-delay evaluation; per-instance parameter delay expressions; per-bit rise/fall/turn-off and X transition selection; two-value turn-off derivation; per-bit inertial cancellation/deadline preservation; declaration delay after multi-driver resolution; mixed zero/nonzero declaration delays; strength-plus-assignment-plus-declaration delay composition; canceled-event pruning; deduplicated RHS-source sensitivity; unrelated-write isolation; resolved-port propagation; in-range literal selects/concatenation; and invalid driver diagnostics. Constant RHS drivers evaluate once. Net declaration assignments, procedural net writes, implicit width conversion, signed operands/targets, charge strengths, nondefault wired-net strengths, explicit-unit delay literals, dynamic/out-of-range RHS selects, partial/hierarchical targets, cross-resolution port bridges, and driver release remain unsupported. |
| 24, programs | Program blocks and reactive semantics | `unsupported` | No conformance claim. |
| 25, interfaces | Interfaces, modports, virtual interfaces, clocking integration | `unsupported` | No conformance claim. |
| 26, packages | Package declarations/constants/classes/functions used by current tests and UVM | `executed` | Deep UVM package traversal is evidence of execution, not complete package/import semantics. |
| 27, generate | Generate `if`, `case`, `for`, genvar scopes | `unsupported` | No hierarchy generation model yet. |
| 28, strength modeling | Symmetric and separate logic-0/logic-1 high-Z/weak/pull/strong/supply strengths on ordinary drivers; implicit `tri0`/`tri1`/`supply0`/`supply1` drivers | `behaviorally validated` | Scheduler tests cover all-Z, every supported strength level, equal conflict, asymmetric X ranges, supply dominance, vectors, and three-driver order independence. Token-to-AST tests cover both strength-pair orders and multiple assignments per statement; strict source-to-runtime tests cover asymmetric, one-sided high-Z, supply, vector, and delay composition behavior. Charge strengths and nondefault strengths on wired-AND/OR nets remain unsupported. |
| 28, gate/switch modeling | Gate/switch primitives and primitive delays | `unsupported` | No conformance claim. |
| 29, UDPs | User-defined primitives | `unsupported` | No conformance claim. |
| 30-32, timing annotation | Specify blocks, timing checks, SDF backannotation | `unsupported` | No conformance claim. |
| 33, user-defined net types | Nettype declarations and resolution functions | `unsupported` | No conformance claim. |
| 34, configurations | Library/configuration-based design selection | `unsupported` | No conformance claim. |
| 35, protected source | Protected/encrypted envelopes | `unsupported` | No conformance claim. |
| 36, DPI | Imported callable metadata and per-simulation host registry | `behaviorally validated` | Focused custom callback, argv, and tool-identity tests. Native ABI loading, open arrays, exports, and complete type mapping remain unsupported. |
| 37+, VPI/coverage/assertion APIs | Standard procedural interfaces | `unsupported` | No conformance claim. |

## Evidence policy

A feature moves right only with a focused test that crosses every implemented
stage relevant to the claim. Negative tests are required wherever permissive
behavior could otherwise skip syntax, substitute zero, ignore a system call, or
compile a default-return callable stub. Real unmodified UVM probes remain
regression gates, but they do not raise a matrix status unless a generic IEEE
facility is isolated and behaviorally checked outside UVM.
