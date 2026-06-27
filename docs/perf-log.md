# Performance log

Performance is a first-class requirement (the OpenTitan workload is the north
star). This file records the headline throughput number at the end of every
phase so we have a trend. **A regression is a build failure** — investigate
before moving on.

How to reproduce the current number:

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path
cd eevee-rs
cargo run --release -p eevee-sched --example counter_bench [N]
```

The benchmark is a free-running counter:
`always #5 clk = ~clk;` + `always_ff @(posedge clk) c <= c + 1;`, run for N
posedges. It exercises the real machinery (timed clock events, edge-sensitive
wakeup with **no polling**, blocking net writes, and the NBA region), not a
synthetic loop. Correctness is asserted each run (final counter must equal N).

| Date | Phase | Benchmark | cycles/sec | Notes |
| ------ | ------- | ----------- | -----------: | ------- |
| 2026-06-26 | P1 | counter, N=10M | **~9.5 M/s** | First event-driven kernel MVP, hand-written Rust processes. 30M resumes/s, 30M net-writes/s. Linear to N=50M (9.2 M/s). dev box, gnu toolchain, release profile (LTO thin, codegen-units=1). |
| 2026-06-26 | P2 | counter, N=10M (IR interp) | **~5.8 M/s** | Same design, now both clock and counter run as real **interpreted bytecode IR** (not hand-written). 1.6x interpreter tax over P1 — excellent for a bytecode VM. Backend = `interp` (swappable for a future JIT). |
| 2026-06-26 | P2.4 | counter, N=10M (IR + call stack) | **~5.45 M/s** | After adding function/task **call frames** to the interpreter. A naive `Vec<Frame>` stack cost ~20% (4.66 M/s) because the hot path indexed the Vec; fixed by keeping the *current* frame in inline fields and spilling only caller frames to a Vec, recovering to 5.45 M/s. ~7% off the no-call number — the price of call support, deemed acceptable. |

## P2 interpreter tax (the number that matters going forward)

The P2 row runs the *identical* counter through the register-IR interpreter
(`eevee-ir`). The slowdown vs. the P1 hand-coded number is the honest cost of
the dispatch loop, since the kernel and `LogicVec` work are byte-for-byte the
same in both:

- 9.53 M/s (P1) = 105 ns/cycle; 5.84 M/s (P2) = 171 ns/cycle.
- The IR loop executes ~15 opcodes/cycle (counter ~5, clock ~5x2 toggles), so
  the added 66 ns/cycle is **~4.4 ns per dispatched opcode** in safe Rust — a
  tight, jump-table `match` with pre-resolved register/net operands.
- A 1.6x tax (IR at ~61% of hand-coded) is very good; bytecode VMs are usually
  2-10x off native. It's this good because the per-cycle cost is dominated by
  shared kernel work, not interpretation.

### Optimization levers (profiler-driven, per brief these are P7)

In priority order, to attack only when a real workload shows the bottleneck:

1. **Typed slot banks** — drop the `Value` tag check in the hot loop (separate
   `LogicVec`/`real`/handle arrays). Also the cleanest JIT lowering.
2. **Opcode fusion** — `c <= c + 1` is a NetRead+Add+NbaWrite triple; a fused
   `NbaAddConst` is the single most common RTL idiom.
3. **Narrow-`LogicVec` boxing** — box the rare wide repr to halve the core
   datum (~40->~24 B). Measured *not* to be the current bottleneck (clone
   traffic ~1.4 GB/s << memory bandwidth), so deferred. Size guarded by a unit
   test (`logicvec_stays_small`).
4. **Unchecked register indexing** in the hot loop (would require relaxing
   `#![forbid(unsafe_code)]` locally with justification).

## Context for the P1 baseline

Per cycle the kernel does ~3 process resumes (clock pos-toggle, clock
neg-toggle, counter body), ~3 net writes (clk x2 blocking, c x1 via NBA), 1 NBA
apply, and 2 time-wheel advances (one per clock edge). At ~9.5 M cycles/s that
is ~28 M process resumes/s and ~28 M net writes/s of useful work.

For reference, the Python `eevee` reference (asyncio coroutine-per-process +
busy-poll `wait`) is multiple orders of magnitude slower on the same shape; the
whole point of the rewrite is this gap. The exact Python number should be
measured and added here for an apples-to-apples comparison.

### Known headroom (not yet taken — deferred to avoid premature optimization)

- The clock pushes/pops the time wheel (BinaryHeap) twice per cycle. A
  dedicated periodic-event fast path could cut heap traffic.
- P2's procedural interpreter will add per-statement cost; the **bytecode/IR**
  pass (CHANGE list in the brief) is where the interpreter perf is recovered.
- No batching of same-net waiter wakeups yet (irrelevant at fan-out 1).
