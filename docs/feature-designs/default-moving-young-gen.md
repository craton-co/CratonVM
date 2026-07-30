# Default Moving / Compacting Young Generation

Status (2026-07-30): **DEFAULT-ON AND ENGAGING.** `DEFAULT_MOVING_YOUNG = true`
is the shipped contract; `CRATONVM_NO_MOVING_YOUNG=1` remains the compatibility
opt-out. Every moving cycle still requires complete rewritable-root coverage and
fails closed to the non-moving sweep when that proof is incomplete.

"And engaging" is the part that had to be earned separately, and it is the
single most important thing to check before believing any status line in this
document. The constant was `true` from 2026-07-28 onward while the collector
still ran the non-moving sweep on **every** cycle of **every** process that
compiled a method — `cycles=0 coverage_fallbacks=66` on bt18 at `-Xmx512m`.
Three defects did that (a process-wide blanket that bypassed the per-cycle
proof; a stale mirror reload that erased the proof's own input; and recursion
being misread as an unguarded foreign frame) and all three are fixed. The same
lane now runs 25 real Cheney cycles with zero fallbacks and the HotSpot
checksum, at parity on wall time with both opt-out configurations. Full account
and evidence:
`docs/internal/default-moving-young-enabled-20260730.md`.

The one obligation still open by design is the **cross-thread coverage
handshake**: a cycle is treated as unproven whenever a peer thread is in
compiled code, so multi-threaded phases keep taking the non-moving sweep. It is
counted as `cross-thread-jit-peer` in the fallback histogram.

Ask `CRATONVM_DBG=gc-stats` for `[GC] moving_young: cycles=… coverage_fallbacks=…`
plus the per-reason histogram before treating this feature as active. A correct
checksum proves safety; only a non-zero cycle count proves the collector is
actually copying.

The heap corruption that blocked this feature is fixed. In the 2026-07-26
validation, five `jit/src/x64.rs` sites pushed an object reference onto the operand stack
without an oop tag, so it reached neither the precise oop map nor the shadow
stack while the safepoint still certified complete coverage. bt18 is now
`68332206` on every run with the JIT on, across 1–25 real moving cycles
(`gc_quiescence::moving_young_cycle_count()`), with zero coverage fallbacks.

**Historical throughput comparison (2026-07-26).** bt18 at `-Xmx8g`, six
interleaved rounds, minimum: the then-default non-moving sweep **1363 ms**,
moving young **2905 ms** — **2.1×**. It was 5.0× until the pre-cycle from-space
walk stopped building an `FxHashSet` of every object start (49% of the whole
process) and started using an exact bitmap.
Of the remaining 1.5 s, roughly 370 ms is codegen moving-young forces, 300 ms
the shadow push/reload, and 870 ms one Cheney copy.

bt18 is a worst case for a copying collector — a very large live set maximises
copying cost against sweeping — but it is also the workload where the
non-moving path died with `OutOfMemoryError: young gen exhausted` at
`-Xmx512m` while the compacting collector completed. Coverage fallbacks remain
an expected safety mechanism, not a reason to disable the default globally.

## Default-on closeout (2026-07-30)

`origin/dev` already contained `DEFAULT_MOVING_YOUNG = true` from
`67de5400ac60` (it arrived as part of an unrelated Liquibase/GC repair), but
the repository still documented a default-off experiment and its test asserted
only equality with that constant. This closeout makes the state deliberate:
the empty environment is asserted `true`, the opt-out is asserted `false`, and
the architecture, GC guide, and source comments all describe the shipped
default-on/fail-closed contract.

Acceptance on the uniquely named release binary:

- `cargo check --workspace` passed; GC library tests were 872/872.
- The complete VM library accounted for 2,565 tests: 2,448 passed, 111 ignored,
  and six unrelated existing failures. All moving-young/root-coverage tests
  passed, including the synthetic shadow-window fixture corrected here to
  satisfy the real fixed-capacity `ShadowStack` invariant.
- The 18-class HotSpot-differential regression suite passed in default JIT,
  default `--nojit`, and explicit `CRATONVM_NO_MOVING_YOUNG=1` JIT modes
  (54/54 class runs).
- `BinTreesClassic 18` returned the HotSpot checksum `68332206` in every lane.
  The final merged JIT build requested moving young on all 64 pressure cycles
  but safely diverted them because exact compiled-frame identity was
  unavailable. A `--nojit`, 128m `BinTreesClassic 16` control executed 31
  non-diverted moving collections and returned the HotSpot checksum `14985902`.
  At `--Xmx 8g`, explicit opt-out returned `68332206` with the moving diagnostic
  absent. These lanes prove both the active default and its relocation-safety
  veto.
- The real-JDK application gauntlet passed in both JIT and `--nojit`: four
  Spring Boot classes (17 tests per mode), ten Hibernate classes (41 tests per
  mode), and Tomcat `TestTomcat` (26 tests per mode), all with their runner
  accounting markers and zero failures/aborts/container failures.

Detailed evidence and baseline exclusions are recorded in
`docs/internal/default-moving-young-enabled-20260730.md`.

The 2026-07-01 "FINISHED" validation table later in this document remains
historical and incomplete: it declared the feature correct without reporting
`moving_young_cycle_count()`, which the diagnostics added on 2026-07-26 now
print alongside the fallback count precisely so that cannot recur.

## Goal

Make a moving (compacting / bump-allocated, semispace-copy) young generation
the **default** collector, safely — eliminating the allocation/throughput
penalty of the non-moving free-list sweep that runs whenever JIT frames are
live, while preserving the Binary-Trees-18 correctness invariant
(**checksum = 68332206 = HotSpot**).

## Historical baseline (pre-implementation)

- **Default = non-moving generational mark-sweep.** `gc/src/gen_heap.rs`
  module doc (`:7`) describes the young gen as a non-moving free-list
  mark-sweep + card table; old gen is also non-moving (`:349`).
- **The JIT-frame safety gate is the crux.** `collect_garbage_inner`
  (`gen_heap.rs:2194`) refuses to run a *moving* (Cheney) collection while any
  JIT frame is live (`:2207` SAFETY comment): JIT frames hold raw object
  pointers in spill slots / registers that are described **only conservatively**
  (`conservative_roots::scan_active_jit_frames`). A semispace copy would
  relocate those objects but cannot safely rewrite a conservatively-discovered
  slot (a stack word that *looks* like a pointer might be an `i64`). So while
  `gc_quiescence::is_active()` the collector runs `sweep_young_non_moving`
  (`:2267`–`:2274`) instead.
- **Selective promotion is the correctness fix and is DEFAULT-ON.** The
  comment at `gen_heap.rs:2243` ("Fix A (2026-06-05)") is the authoritative
  record: under live JIT frames the correct young collector is the **non-moving
  sweep + selective promotion** (`selective_on` in `sweep_young_non_moving`),
  giving **bt18 = 68332206 = HotSpot**. It marks conservatively (over-marking is
  safe), pins conservative roots, and tenures only heap-interior nodes, so no
  live `make`/`check` node is lost.
- **The moving Cheney UNDER-COUNTS bt18.** Same comment (`:2248`): the moving
  semispace produces the long-mislabelled "golden" **67674804** because a
  semispace cannot pin a conservative JIT root nor rewrite a register-resident
  one — some live nodes go stale after the swap. `CRATONVM_DBG_FORCE_MOVING`
  (`gen_heap.rs:2242`) forces the (under-counting) moving cycle for diagnostics
  only.
- **Shadow-stack precise maps exist but are incomplete and default-off.**
  `gc/src/shadow_stack.rs` (276 lines) publishes the operand-stack `Reg` oops
  the conservative scan misses (register-invisibility). The
  `CRATONVM_SHADOW_STACK` gate (`gen_heap.rs:2266`) currently routes those oops
  to be **scanned as roots → pinned** under the non-moving sweep (keeping bt18 =
  68332206), *not* to enable safe moving. The original moving-via-precise-roots
  attempt is recorded as **incomplete** (68199090, a partial fix toward
  68332206, not the answer). Background: `MEMORY.md` "precise JIT stack maps"
  project entry.

Net: the *correctness* problem (bt18 checksum) is already solved by the
non-moving sweep + selective promotion. The *performance* problem — non-moving
free-list allocation is much slower than a bump pointer, and the sweep's O(n)
walk dominates allocation-heavy workloads like bt18 — is what a default moving
young gen would fix. The blocker is that moving is only safe with **precise**
JIT roots, which the shadow stack does not yet fully provide.

## Design

The moving young gen can become the default **only** when, for every live JIT
frame, the GC has a precise, **rewritable** description of every register- and
stack-resident oop. Two routes; they converge on the same metadata as
`real-frame-deopt.md`.

### Route A — finish the shadow stack into a rewritable precise map (lower risk)

Today the shadow stack pins. To *move*, every published oop slot must be
**rewritable** so the copy can relocate it and patch the slot:

1. Extend `shadow_stack.rs` so each entry records not just the oop value but its
   **home** (register id or native stack offset) and a way to write back the
   forwarded address — i.e. the shadow becomes a precise, updatable root list,
   not just a scan list.
2. After the semispace copy computes forwarding pointers, walk the shadow stack
   and rewrite each home with the forwarded address (registers via the saved
   register file at the safepoint; stack slots in place).
3. Preserve selective promotion semantics so the **bt18 = 68332206** invariant
   holds: the moving path must tenure the same node set the non-moving sweep +
   selective promotion does (the under-counting 67674804 came precisely from
   *not* pinning/promoting those nodes).

### Route B — reuse the deopt safepoint maps (shared infrastructure)

The per-safepoint register→slot maps designed in `real-frame-deopt.md` are
*exactly* the precise root description a moving collector needs. If GC only ever
moves at safepoints (it does — `gc/src/safepoint.rs`), the safepoint's
`FrameState` enumerates every live oop and its home. Build the moving young gen
on the deopt safepoint table:

1. At a GC safepoint, for each live JIT frame look up its `DeoptimizationPoint`
   by native PC, collect the `FrameValue::Register`/`StackSlot` oops.
2. Treat them as precise rewritable roots in the Cheney copy.
3. Patch them post-copy from the forwarding table.

Route B is more work up front (depends on deopt maps) but unifies precise-roots
infra; Route A is incremental on existing shadow-stack code. Recommend **A
first** (it can ship behind a gate and be validated against bt18 in isolation),
**migrate to B** once deopt maps exist.

### Correctness invariant harness

Whatever route: gate the new default behind a flag, and make CI assert
`bt18 == 68332206` (and bt10/14/16 vs `java -cp bench BenchSuite`) before the
flag flips to default-on. The selective-promotion default-on flip (`Fix A`)
already established this as the acceptance test; reuse it. The
`MEMORY.md` bintrees measurement-loop reference documents the run recipe (8g
heap, taskkill stray cratonvm first, `build-cpu.bat`).

## Implementation steps (ordered)

1. **Make the shadow stack precise+rewritable** (Route A): record home
   (reg/stack-off) per published oop in `shadow_stack.rs`; add a writeback API.
2. **Wire moving-with-shadow-rewrite** in `collect_garbage_inner`: when shadow
   coverage is complete for all live frames, run the Cheney copy *and* rewrite
   shadow homes from the forwarding table — instead of the
   pin-only/non-moving path at `gen_heap.rs:2267`.
3. **Match selective-promotion tenuring** in the moving path so the live node
   set is identical to the non-moving sweep's.
4. **Gate + validate**: `CRATONVM_MOVING_YOUNG` (default-off). CI asserts bt18 =
   68332206 and the bt10/14/16 checksums; measure throughput vs the non-moving
   default.
5. **Coverage fallback**: if any live frame lacks complete shadow coverage at a
   safepoint, fall back to the non-moving sweep for that cycle (never move with
   an incomplete map). Count fallbacks; the flag can only become default when
   fallbacks are ~0 on the app gauntlet.
6. **Flip default**; keep `CRATONVM_NONMOVING_YOUNG` as the opt-out escape hatch.
7. **(Later) migrate to Route B** once `real-frame-deopt.md` safepoint maps
   exist, retiring the bespoke shadow rewrite.

## Risks

- **Under-counting regression** (the 67674804 trap): any node the moving path
  fails to keep alive or promote that the non-moving sweep would have is a
  silent correctness bug. The bt18 invariant catches the canonical case; other
  apps need the coverage-fallback safety net.
- **Incomplete shadow coverage** moving anyway = heap corruption (relocating an
  object whose register-home isn't rewritten leaves a dangling raw pointer in a
  JIT spill slot). The fallback in step 5 is mandatory, not optional.
- **Conservative/precise mismatch**: mixing a conservatively-scanned slot with a
  precisely-rewritten one for the *same* object must be impossible — once a
  frame is "precise", it must be fully precise.
- **Throughput might not improve as much as hoped** if the sweep cost was
  partly card-table / marking rather than allocation; measure before declaring
  victory. The `MEMORY.md` bt18 "JIT helper overhead" entry warns that part of
  the historical bt18 gap was per-node JIT-helper cost, *not* GC — re-confirm
  the GC share of the current 23x.

## Effort

XL, and gated on either finishing the shadow stack (Route A, large) or on
`real-frame-deopt.md` safepoint maps (Route B, very large). Route A is the
pragmatic first landable; the correctness harness already exists.

## Prerequisite recheck — MEASURED 2026-06-21 (dev `a6f1ec55`)

Ran the design's mandated "MEASURE FIRST" gate directly on a fresh release build
(`cvmove.exe`, worktree `CratonVM-movingyoung`, branch
`feat/default-moving-young-gen`). Benchmark = the canonical object-based
`binarytrees` (checksum 68332206 = HotSpot). `-Xmx8g`. Box load comparable to
the 2026-06-10 gap doc (HotSpot bt18 651 ms vs its 497 ms).

| Variant (bt18) | checksum | time | sweeps |
|---|---|---|---|
| HotSpot JDK 25 | 68332206 | **651 ms** | — |
| CratonVM default (non-moving sweep under JIT) | 68332206 ✓ | 48 059 ms | 2 |
| CratonVM `CRATONVM_NO_GC` (GC disabled entirely) | 68332206 ✓ | **50 087 ms** | 0 |
| CratonVM `CRATONVM_DBG_FORCE_MOVING` | **67674804** ✗ | 47 975 ms | 0 |
| CratonVM force-moving **+ `CRATONVM_SHADOW_STACK`** | **67674804** ✗ | 46 502 ms | 0 |

Two conclusions, both against the design premise:

1. **GC is ~0 % of the gap.** Turning GC *completely off* does **not** speed bt18
   up (50.1 s vs 48.1 s — it is marginally *slower*, the heap balloons). Only
   **2** sweeps run in the entire bt18 run. Across all five GC configurations the
   time is 46–50 s — the GC choice does not move the needle at all. A moving
   young gen would replace 2 cheap non-moving sweeps with 2 semispace copies,
   saving at most a few percent of a 48 s run. The 48 s gap vs HotSpot's 0.65 s
   is **per-node mutator cost**, not GC — confirmed independently by the
   binary-trees throughput investigation that root-caused the same gap.

2. **The Route-A prerequisite is not met — moving silently corrupts the
   checksum.** `FORCE_MOVING + SHADOW_STACK` still yields **67674804** (the
   under-count trap), not 68332206 — shadow coverage is incomplete (it does not
   even reach the historical partial 68199090). Flipping to moving today would be
   a silent correctness regression.

**Verdict: do NOT implement the default moving young gen.** It cannot close the
bt18 gap (its own acceptance benchmark shows no GC headroom) and is unsafe until
shadow coverage is finished. The real levers are the per-node mutator costs in
the gap doc (§"next levers": inline `putfield` reference store, per-alloc TLS
fetch, 72-byte tagged `Value` field cells). Separately flagged: bt16/bt18 are
~3–6× slower than the 2026-06-10 perf-branch numbers (bt16 1.6 s → 9.9 s;
HotSpot only ~2× of that is box speed) — a probable throughput **regression**
worth its own investigation, far higher value than this feature.

## FINISHED — gated moving young gen validated correct (2026-07-01, `feat/moving-young-gen-finish`)

Status: **IMPLEMENTED and validated behind `CRATONVM_MOVING_YOUNG` (default-off).**
The 2026-06-21 "do NOT implement" verdict was **wrong on both counts**, and both
were corrected before this work:

1. **"GC is ~0 % of the gap" was an 8g artifact.** A later adversarial pass proved
   the young semi-space is `Xmx/4` and minor GC fires at 50 % occupancy, so the
   young-collection *count scales ~1/Xmx*. "2 sweeps" is just what 8g produces; at
   realistic heaps GC runs constantly (measured: bt16 @512m = 8 collections). The
   very 8g regime that "proved GC doesn't participate" is the one that fires 0–2
   cycles — it masks both the participation and the corruption.
2. **The "silent corruption" was incomplete shadow coverage, not an intrinsic
   flaw.** `collect_live_oop_homes` (x64.rs) had been *narrowed* (B-K kafka fix) to
   publish only register-invisible operand oops — a supplement to the conservative
   scan, never the complete rewritable map Route A step 1 requires. A moving copy
   then relocated frame-slot / local oops the conservative scan could mark but not
   rewrite → dangling → the 67674804 / 68199090 under-counts.

### What was implemented (Route A, completed)

- **Complete coverage** (`x64.rs::collect_live_oop_homes`): under the gate,
  publishes *every* live oop — operand entries in registers **and** frame slots,
  plus every oop local in its register or canonical frame slot (deduped), seeded
  with `param_oop_mask` for early ref-params. The push/reload codegen already
  supported both `Reg` and `Frame` homes; only the enumeration was incomplete.
- **Gate** `CRATONVM_MOVING_YOUNG` implies shadow codegen + root scan + remap
  (`shadow_stack_maps_enabled`/`shadow_stack_enabled` OR it in).
- **Suppress the conservative JIT scan** (`roots.rs`) under the gate — a
  fully-precise frame needs no conservative backstop, and mixing a
  conservatively-marked slot with a precisely-relocated object is the documented
  corruption risk. The shadow stack is then the sole precise JIT root set.
- **Run the moving (Cheney) cycle under live JIT** (`gen_heap.rs::collect_garbage_inner`)
  instead of diverting to the non-moving sweep; the `honor_promotion_oom_risk`
  guard is kept as an abort-free fallback when both generations are ~full.

### Validation (this box, `cvmmovyoung.exe`)

Every checksum = HotSpot; the moving cycle actually fires (SHADOW remap logs);
0 faults, 0 aborts, 0 non-moving fallbacks:

| case | heap | checksum | moving cycles | HotSpot |
|---|---|---|---|---|
| bt18 | 8g   | 68332206 ✓ | 2 | 68332206 |
| bt18 | 4g   | 68332206 ✓ | 4 | — |
| bt16 | 2g   | 14985902 ✓ | 2 | 14985902 |
| bt16 | 512m | 14985902 ✓ | 8 | — |
| bt14 | 256m | 3222190 ✓  | 3 | 3222190 |
| **old reg-only** (`FORCE_MOVING+SHADOW_STACK`) | 8g | **68199090 ✗** | — | (proves the fix is *completeness*) |

Default path is byte-identical (all new logic behind `if complete` / the gate);
`cargo test -p cratonvm-gc` = 737 passed, jit lib = 847 passed (4 pre-existing
aarch64 branch-overflow failures, unrelated to x64/GC). Perf is neutral-to-slightly
positive at these heaps (bt14 @512m ~9 % faster; bt18 @8g ~2 % faster) — consistent
with much of the bt gap being per-node mutator cost, not GC.

### Historical remaining work before the default flip (step 6)

Not done in the 2026-07-01 session — the flip needed the app gauntlet, which
that session could not run
end-to-end. Two narrow coverage gaps remain and are the mandatory `step 5`
fallback's job to catch (they never bite bt10/14/16/18):

- Methods with **>64 locals** → `compute_local_oop_masks` returns empty → no local
  oops published. Needs: fall back to non-moving for a cycle where such a frame is
  live (or refuse to JIT it under the flag).
- **PCs the forward oop dataflow never reached** (e.g. exception-handler-only
  entries) publish no locals. Same fallback applies.

Until those were covered by a coverage-completeness signal + fallback, the
2026-07-01 branch kept the flag default-off. Route B (deopt safepoint maps)
remains the eventual unification.
