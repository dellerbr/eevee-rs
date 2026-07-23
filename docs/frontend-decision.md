# Front-end decision: Verible (subprocess + JSON CST) for v0, slang deferred

**Status:** Decided for P0–P5. Revisit at P6 (OpenTitan) if elaboration cost dominates.
**Date:** 2026-06-26

## The choice

Two mature open-source SystemVerilog front-ends were evaluated, exactly as the
mission brief requested:

| | **slang** | **Verible** |
| --- | --- | --- |
| Output | Fully elaborated, typed semantic AST | CST (concrete syntax tree) only |
| Language coverage | Most complete OSS 1800 front-end | Very good; a few constructs need text rewrites |
| Rust integration | C++ library → `cxx`/FFI bridge | **`verible-verilog-syntax --export_json` → process + JSON** |
| Build requirement | Build from source (CMake + MSVC/clang) | **Prebuilt win64 binary already vendored in this repo** |
| Reuse of prior art | None — new lowering needed | **`eevee/lang/verible_fe.py` lowering map ports 1:1** |
| Elaboration | Free (params, generates, types resolved) | We implement it (port `exec/elaborator.py`) |

## Decision: **Verible, integrated via subprocess + JSON** — not FFI

The single most important realization from spiking both: **Verible's natural Rust
integration is not FFI at all.** The Python reference already drives it as
`verible-verilog-syntax --export_json --printtree <files>` and parses the JSON
CST on stdout. In Rust this is `std::process::Command` + `serde_json` — zero
unsafe, zero C++ build, zero `cxx` bridge. Verified working this session:

``` cmd
verible/verible-v0.0-4053-g89d4d98a-win64/verible-verilog-syntax.exe \
    --export_json --printtree <file|->   # emits JSON CST to stdout
```

### Why Verible wins for v0

1. **Zero build friction.** The win64 binary is already in `verible/`. slang would
   have to be cloned and built (CMake, a C++17 toolchain, ~minutes per build),
   then wrapped with `cxx` against a large, evolving C++ API surface. That is real,
   ongoing maintenance risk for marginal P0 value.
2. **The lowering map is the asset, and it already targets Verible.**
   `eevee/lang/verible_fe.py` (~part of the 8.9k-LOC front-end) encodes thousands
   of CST-shape decisions and Verible quirks (catalogued below). Porting that
   Python→Rust is mechanical and low-risk. Re-deriving equivalents against slang's
   AST throws away the project's hardest-won, least-fun-to-rediscover knowledge.
3. **Process isolation is a feature, not a cost.** A crashing/looping parser cannot
   take down the simulator; we parse once, cache the JSON, and never link a large
   C++ blob into our address space. DPI-C (P5) is where we *do* want FFI; keeping
   the parser out of that surface keeps the unsafe budget small.
4. **Measured cost is acceptable.** uvm_pkg.sv parses in ~24 s in the Python flow,
   almost all of it Verible's own CPU. That is a one-time, cacheable elaboration
   cost, not a per-cycle cost — and per-cycle is where the OpenTitan perf budget
   actually lives (that is the scheduler's job, P1).

### Why not slang (yet)

slang's elaborated, typed AST is genuinely attractive — it could subsume much of
`exec/elaborator.py` (params, generates, type resolution, hierarchy). But:

- The FFI friction (build + `cxx` over a big API) is exactly the "too high" case the
  brief told us to fall back from.
- We would still need our own runtime/interpreter and class-runtime semantics;
  slang gives elaboration, not execution.
- Adopting it is **not blocked** by this decision. The front-end is isolated behind
  an internal `Cst`/`Ast` boundary (see below), so a slang-backed elaborator can be
  added later as an alternative front-end without reshaping the runtime.

## What this commits us to

- A `eevee-fe` crate (P2) that: shells out to Verible, deserializes the JSON CST
  (`serde_json`), and lowers it to our own AST — a direct port of `verible_fe.py`.
- Carrying Verible's text-rewrite workarounds (the Python `_rewrite_*` passes):
  `checker`→`module`, `tagged Tag e`→`__tagged_ctor(...)`, the new-class-form rewrite.
- An AST designed so a **bytecode/IR** compile pass (the perf win) can be bolted on
  without changing the AST shape (see CHANGE list in the brief).

## Verible CST quirks to carry over (from `verible_fe.py`, encode as tests early)

- `q.sort()` parses as `kReference > [kLocalRoot, kBuiltinArrayMethodCallExtension]`;
  the method name is a keyword leaf with `tag=='sort'` and **empty `text`** — use
  `text or tag`.
- Zero-arg `p.x` parses as a *call* `CallExpr(MemberExpr(p,x))`; needs a field fallback.
- `super` is a **raw token**, not an identifier.
- Numeric literal `value` fields are **strings** — parse with radix detection.
- `q[$]` last-element select: `$` is a raw token with `text=='$'`.
- Several data-type wrappers (`kDataType`/`kDataTypePrimitive`/`kTypeInfo`/
  `kInstantiationType`) must be treated as one "type subtree" set.
- Verible parses continuous drive-strength syntax but omits its tokens from the
  exported CST. Conformance parsing therefore requests `--printtokens` together
  with `--printtree`, maps supported strength pairs to the owning `assign` byte
  offset, and rejects unconsumed or charge-strength tokens before CST lowering.
  Declaration-position `supply0`/`supply1` tokens remain supported internal net
  types. Permissive parsing requests tokens only when an exact strength keyword
  is present, so its normal UVM path remains tree-only.

## Revisit criteria

Reopen this decision at P6/P7 if **either**:

- elaboration (parse + build hierarchy) becomes a wall-clock problem on OpenTitan
  blocks (slang's prebuilt elaboration would amortize), **or**
- our hand-ported elaborator accrues more complexity than a slang binding would.
