# `CRATONVM_JIT_IR_INLINE` on the gauntlet: engaged, fast, and one deterministic wrong exception

## Status

**SOAKED, 2026-08-28. NOT flipped — one deterministic correctness regression.**

Unlike its neighbour `CRATONVM_SCALAR_DEOPT`, this flag is emphatically **not
inert**: 11 030 methods spliced across 200 netty classes, 355 across the
hibernate class. It is also, on the evidence here, **faster** — 8% on a serial
netty slice, 15-26% on hibernate.

And it breaks one test, every time:
`fixed-bugs/jit/ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`
(FIXED 2026-08-28 — the spliced-bci argument was dropped on the floor, and the
replay rule asked about the whole body instead of the abandoned attempt).
A 3-byte out-of-bounds read through a spliced accessor raises `InternalError`
("precise deoptimization unavailable … reason UnreachedCode") instead of
`IndexOutOfBoundsException`. That is a user-visible wrong exception type on an
ordinary bounds-check path, so the flip waits on it.

## Engagement — the question that sank the last soak

The `CRATONVM_SCALAR_DEOPT` soak was green because the feature never executed.
So engagement was measured first, and this flag passes that bar comfortably:

| workload | `inline-plan` | methods spliced |
|---|---:|---:|
| netty, 200 classes | 27 068 | **11 030** |
| hibernate `ASTParserLoadingTest` | 1 153 | **355** |
| `probes/VoxelAlloc2.java` | 7 | 5 |

It does NOT fire on short-lived vectors — 0 splices across 8 regression-suite
classes and 0 on `RJitGc` — because those exit before the optimizing tier gets
going. Engagement needs a workload that runs long enough to tier up.

## Correctness

| gate | off | on |
|---|---|---|
| `jit/tests/ir_vs_singlepass.rs` (the designated pre-flip gate) | 142/142 | 142/142 |
| `differential`, `x64_artifact_corpus`, 3 intrinsic suites | green | green |
| `cargo test -p cratonvm-jit --lib` | 2113 pass / 1 fail | 2113 / 1 (identical) |
| regression suite | 72/72 | 72/72 |
| netty 200 classes, 5 shards | PASS 112, FAIL 58 | PASS 112, FAIL 58 |
| hibernate `ASTParserLoadingTest` | 104 ok / 0 failed | 104 ok / 0 failed (×4) |
| **netty 40 classes, SERIAL** | **PASS 23, FAIL 2** | **PASS 22, FAIL 3** |

Two of those rows need their caveat stated, because both would otherwise be read
as evidence they are not.

**The designated gate is green VACUOUSLY.** `ir_vs_singlepass` runs the inliner
zero times: with `CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_IR_COMPILES=1` it emits
0 `inline-plan` and 0 `spliced` lines across all 142 tests. It is a standalone
jit harness with no VM behind it, so `resolve_ir_inline_site` has no callee
bodies to splice. `activate-ir-optimizer.md` names this harness as the
pre-flip differential gate for exactly this class of change; for THIS flag it
cannot serve, and that should be fixed before the next attempt.

**The jit lib failure is pre-existing and arm-neutral.** Two breakages on dev,
neither about this flag: the lib test target did not compile at all
(`intern_inline_invoke_targets`' test call site kept the pre-`b56bb79b9` arity —
fixed in this branch so the target builds), and then
`layout_constant_emission_sites_are_inventoried` fires (HEADER_SIZE used 7× in
`jit/src/lib.rs`, inventory records 5×). Identical 2113/1 with the flag on and
off.

### The one real difference

`io.netty.buffer.DuplicatedByteBufTest`, serial, 5 reps per arm, one binary:

| arm | result |
|---|---|
| off | `ok=416 failed=0` ×5 |
| on | `ok=415 failed=1` ×5 |

Deterministic in both directions. `getMediumBoundaryCheck2()` expects
`IndexOutOfBoundsException` and gets `InternalError`; the method the error names
(`UnpooledHeapByteBuf._getUnsignedMedium`) is the method the flag splices into.
Full diagnosis on the known-issues page.

**The sharded run could not see this.** At 5 shards the class fails in BOTH arms
and the 200-class A/B reported zero verdict differences. Contention masked it;
the serial run is what separated the arms. Worth remembering: a wider sharded
sweep is not a more sensitive one.

## Throughput

The design note's stated cost is "more nodes per compile … compile time".
Measured, it is a *win* wherever the compile amortises, and the one apparent
regression was contention rather than the flag:

| workload | off | on | |
|---|---:|---:|---|
| netty 200, 5 shards | 858 s wall, 2 580 096 class-ms | 998 s, 3 060 411 | +16% / +18.6% |
| **netty 40, SERIAL** | **881 s wall, 852 944 class-ms** | **807 s, 781 688** | **−8% / −8%** |
| hibernate | 999 s | 739 / 766 / 738 / 852 s | −15% to −26% |

The sharded row and the serial row disagree in sign on the same suite and the
same binary. The serial one is the flag's cost; the sharded one is five forks
competing on a host already carrying unrelated load. Under the runner's fixed
180 s per-class cap that contention also moved two classes from `ABORTED` to
`HANG` in the on arm — `AlignedPooledByteBufAllocatorTest` and
`PooledByteBufAllocatorTest`, both of which pass in BOTH arms when re-run
serially, 3 reps each.

## An unexplained one-off

One hibernate on-arm run died silently: `rc=127`, 151 s, no `@@RESULT`, and no
panic, abort, OOM or crash marker anywhere in 3.6 MB of stderr — the log simply
stops mid-compile. It did **not** reproduce in four further on-arm runs, one of
which carried the identical `CRATONVM_DBG_IR_COMPILES=1` that was set when it
died. Recorded rather than dropped, because an unexplained silent death that is
written off once is how a real one gets dismissed later.

## What the soak did NOT find

The design note's standing correctness caveat — "a call left inside a spliced
body is re-executed on a deopt" — cannot fire today, and this is worth stating
because it reads like the likeliest hazard and is not. It is fenced twice:
`resolve_ir_inline_site` refuses `idiv`/`irem`/`ldiv`/`lrem`; the div-zero guard
is the ONLY guard `IrBuilder` emits; and `splice_guard_seen` refuses the whole
graph if a guard is ever built inside a splice (`ir.rs`, checked at the end of
`build`). No deopt point exists inside a spliced region, so there is no deopt to
re-execute the call. The bug that WAS found is a different shape: an
`UnreachedCode` deopt request from the ordinary lowering, in a method that
happens to carry a spliced body.

## Before the next attempt

1. **Fix the `InternalError`.** It is the only blocker on the evidence here.
2. **Make the designated gate able to see this flag.** A differential harness
   that runs the inliner zero times cannot gate it, and it is currently the
   named gate.
3. Re-run the serial arm. The sharded sweep is the wrong instrument for verdict
   differences — it masked the one real regression in this soak.

## Reproducing

```bash
# engagement
CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_IR_COMPILES=1 <cratonvm> ... 2>&1 \
  | grep -c 'spliced .* callee bod'

# the gate, and its vacuity
CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_IR_COMPILES=1 \
  cargo test -p cratonvm-jit --test ir_vs_singlepass -- --nocapture --test-threads=1 \
  2>&1 | grep -c inline-plan          # 0

# the serial netty A/B — shards=1 is load-bearing
./run-netty-suite.sh --list <40-class list> --bin <cratonvm> --out /tmp/off --shards 1
CRATONVM_JIT_IR_INLINE=1 \
  ./run-netty-suite.sh --list <40-class list> --bin <cratonvm> --out /tmp/on --shards 1
```

Netty's pass set is `testlist.txt` minus `netty-nonpassed-latest.txt`; the
latter is CRLF and the former LF, so subtract them with `tr -d '\r'` or the
subtraction silently matches nothing.
