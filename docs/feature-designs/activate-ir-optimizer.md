# The IR optimizer (GVN / const-fold / DSE / LICM + scalar replacement)

**Status:** Shipped (default on). The optimizing tier runs on general method
shapes; the residual gaps are listed below.

## What it does today

`optimize()` in `jit/src/ir_optimize.rs` runs GVN, constant folding, algebraic
simplification, dead-store elimination (`eliminate_dead_stores` +
`eliminate_write_only_stores`) and DCE **unconditionally** in a fixed-point
loop. LICM and full unrolling of small constant-trip counted loops run inside
the same loop, each behind a default-on flag.

`jit/src/ir.rs` carries `Op::Load`/`Store` (with a `MemKind`), `Op::New`,
`Op::NewArray`, `Op::Call` and `Op::LoadStatic`, so the IR path is no longer
restricted to call-free, branch-free, allocation-free leaves. Two escape
analyses are live: the IR-level one in `jit/src/escape_analysis.rs` (driven by
`escape_analysis_from_ir` / `apply_ea_to_ir` in `jit/src/lib.rs`) and a
separate bytecode-level one for the single-pass backend
(`jit/src/x64/escape_analysis.rs`).

Both wide value tiers are complete and on:

- **long (64-bit)** — arithmetic, constants, load/store, shifts, bitwise,
  compare, branches, call arguments, and `ldiv`/`lrem` with long deopt-resume.
- **double/float (XMM)** — value arithmetic, constants, FP locals, conversions,
  compares, FP arrays, `frem`/`drem`, FP-slot deopt resume, and FP
  parameters/returns.

`J`/`D`/`F` **call returns** work. `static_call_shape` (`jit/src/lib.rs`)
accepts them; a legitimate `Long.MIN_VALUE` return is distinguished from the
`i64::MIN` deopt sentinel by the `dispatch_threw` runtime helper, which peeks
every out-of-band signal only on the rare `RAX == i64::MIN` branch.

### The gates, and where their defaults actually live

Defaults are decided at the **read site**, not in `types/src/flag_groups.rs`
(which only maps token → legacy env key). The VM-side accessors are memoized in
`vm/src/runtime/env_cache.rs`; the jit-side ones sit in
`jit/src/ir_optimize.rs` and `jit/src/lib.rs`.

| Flag | Default | Opt-out / opt-in |
|---|---|---|
| `CRATONVM_JIT_IR_CALL` | on | `=0` |
| `CRATONVM_JIT_IR_CALL_SPECIAL` | on | `=0` |
| `CRATONVM_JIT_IR_CALL_VIRTUAL` | on | `=0` |
| `CRATONVM_JIT_IR_LONG` | on | `=0` |
| `CRATONVM_JIT_IR_FP` | on | `=0` |
| `CRATONVM_JIT_SCALAR_NEW` | on | `=0` |
| `CRATONVM_JIT_LICM` | on | `=0` |
| `CRATONVM_JIT_UNROLL` | on | `=0` |
| `CRATONVM_NO_IR_BRANCHY` | branchy IR on | this var is the opt-out |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | single-pass SR on | this var is the opt-out |
| `CRATONVM_JIT_REASSOC` | **off** | set to opt in |
| `CRATONVM_SCALAR_DEOPT` | **off** | set to opt in |

## What is not built yet

- **Guard-surviving scalar replacement is default-off, and SOAKED default-off
  on purpose.** The producer exists behind `CRATONVM_SCALAR_DEOPT` (which also
  needs `CRATONVM_DEOPT_REAL`), and it additionally refuses monitor-bearing
  graphs — `FrameState` in `jit/src/ir_lower.rs` hard-codes
  `monitors: Vec::new()`, and `can_deopt_resume` is gated on the graph holding
  no `Op::MonitorEnter`/`Op::MonitorExit`, so that omission cannot be reached.
  The 2026-08-27 gauntlet soak
  (`performance/scalar-deopt-gauntlet-soak-20260827.md`) found the
  flag green and **inert**: 30 392 allocation-bearing IR compiles across netty
  and hibernate produced ZERO scalar replacements, so it emitted no descriptor
  anywhere — including on the probe written to exercise it. Flipping it on that
  evidence would record a soak that never ran the feature.
- **Methods needing precise exception-handler frames never reach this tier.**
  The `!precise_exception_frames` clause in `jit/src/lib.rs` excludes them
  because the IR lowerer has no precise-handler-frame equivalent; they are
  pinned to the single-pass backend. See
  [`jit-local-exception-handlers.md`](jit-local-exception-handlers.md).
- **Affine reassociation** (`CRATONVM_JIT_REASSOC`) never soaked out of
  default-off.
- **`stack_allocatable` and `lock_coarsening`** are computed by
  `jit/src/escape_analysis.rs` and never read by anything.
- **SCEV-driven LICM.** The structural graph-level LICM is live and correct.
  The `licm_scev_corroborates` bytecode bridge exists and is unit-exercised,
  but `optimize(&mut Graph)` has no bytecode in scope, so it is not wired as a
  gate or extender. Its value over the structural pass is marginal, so this is
  a low-priority refinement rather than a gap.
- **The heavy soak.** Each default flip was validated on the bintrees
  checksums and the differential harness; the full
  kafka/spring/tomcat/hibernate gauntlet is a heavier soak that has not been
  run per-flip.

## Design rationale

Three structural choices carry this tier, and each is the answer to a hazard
that already bit once.

1. **The single-pass backend is the correctness fallback, not a legacy path.**
   If IR lowering returns `None` or a self-check fails, compilation falls
   through to `jit/src/x64.rs`. Every widening of the IR gate is safe only
   because that fallback exists.
2. **Every widening shipped behind its own flag with a differential test.**
   `jit/tests/ir_vs_singlepass.rs` compiles a corpus through both backends and
   asserts equal results. It caught a multi-return miscompile that unit tests
   did not, which is why a flag flip is gated on the harness rather than on
   review.
3. **Escape analysis is conservative-on-unknown.** Any value flowing into a
   call argument, field store, array store, return or throw **escapes**. The
   single-pass escape pass once scalar-replaced an object that escaped as a
   call argument; the IR analysis is written not to repeat it.

## Invariants that must not be broken

- **Escape-analysis soundness** (kafka bug-25 class): a single missed escape
  edge → a scalar-replaced object that should have been heap-allocated →
  null/garbage at a real use. Conservative-on-unknown is mandatory.
- **DSE aliasing**: removing a store that *was* read through an aliased base is
  a silent data-loss bug. Give up on any base the analysis can't prove
  non-aliasing.
- **LICM hoisting a load past a store** to the same location, or past an
  exception edge that should observe the pre-loop value, is incorrect — respect
  memory effects and exception edges.
- **Checksum invariants**: every default flip must preserve bt18 = 68332206 and
  the kafka/keycloak/tomcat gauntlet baselines.

