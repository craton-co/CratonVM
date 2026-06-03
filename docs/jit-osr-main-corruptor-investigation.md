# JIT heap corruption — the `main()` corruptor (OSR is the trigger; the non-moving young sweep is the root cause)

This is the second member of the JIT young-gen heap-corruption family (the first,
the scatter-store BCE elision, is fixed — see `jit-bce-scatter-store-fix.md`).

## RESOLVED root cause (reconciled with the concurrent GC session, commit `caf8451`)

The corruptor is the **non-moving young sweep** (`gc/src/gen_heap.rs`
`sweep_young_non_moving`), which the GC uses whenever quiescence is active (live
JIT frames present) instead of the moving Cheney collector. Commit `caf8451`
proved it with `CRATONVM_DBG_FORCE_MOVING=1` (force the moving GC even when
active): the corruption goes **211 → 0**. A leaked `JitEntryGuard` (enter/leave
imbalance, e.g. on the BC EC `LongArray.modMultiply` path) keeps quiescence
wedged active so the non-moving sweep runs; full evidence is in memory
`reference_jit_junitcore_corruption` round 7. Fix follow-up there: guard
Drop-bypass in `execute_jit_call` / harden the non-moving sweep.

**The OSR analysis below is the *trigger*, not a separate bug, and it independently
confirms the root cause.** OSR is simply what makes a once-called `main`'s JIT
frame *live during its allocating loop*, so the young GC takes the buggy
non-moving sweep path. This is why `BISECT_SKIP …/main` → 0 (no live JIT frame →
moving GC), `--nojit` → 0, and now **`CRATONVM_DBG_FORCE_MOVING=1` → 0 on the
BC-free `AllocLoop` repro too** (verified: `sink=0`, rc=0, 0 corruption). The
`AllocLoop` repro is therefore a minimal, BC-free reproducer of the same
non-moving-sweep bug.

Everything below was the OSR-trigger investigation; keep it for the repro and the
ruled-out list, but the "leading open hypotheses (oop maps)" section is
SUPERSEDED by the non-moving-sweep root cause above.

---

This one is **not yet fixed**; this doc records what is firmly established so the
next attempt doesn't re-tread it.

## It is an OSR (on-stack-replacement) bug — firmly established

`main()` is invoked exactly **once**, so it can never reach the invocation-count
JIT threshold. The only way it gets JIT-compiled is via **OSR back-edges** in its
hot loop. Therefore the long-standing bisect result

```
BISECT_SKIP …/main  → ~0 corruption
BISECT_SKIP …/run   → still corrupts
```

means **OSR-compiled `main` is the corruptor**. This affects every once-called
method with a hot allocating loop (every `main`, every server accept/request
loop), so it is high-value and not specific to the JUnit harness.

This supersedes the earlier "`Long.numberOfLeadingZeros` intrinsic" /
"category-2 long slot" theories — see "Ruled out" below.

## Reliable minimal reproducer

`C:/tmp/mainbug/AllocLoop.java` — a counted loop calling an allocating method,
no exceptions, no longs:

```java
public class AllocLoop {
    static int run() { int[] a = new int[64]; int[] b = new int[64]; return a[0] + b[63]; }
    public static void main(String[] args) {
        long sink = 0;
        for (int i = 0; i < 200000; i++) sink += run();
        System.out.println("sink=" + sink);
    }
}
```

- HotSpot / CratonVM `--nojit`: prints `sink=0`, clean.
- CratonVM JIT: corrupts **every run** (3/3: 37–188 `inconsistent header` events,
  derails, rc=127/124, never prints). This is a *reliable* signal — unlike the
  chaotic corruption-count A/B bisection, "does AllocLoop derail?" is binary.

## Ruled out (each tested with a dedicated variant under JIT)

- **Long counters / category-2 slot.** `IntLoop` (identical but `int` counters,
  no longs) still corrupts. So it is not the long-operand-slot accounting.
- **Exceptions / try-catch.** `AllocLoop` (no try/catch, callee returns normally)
  still corrupts. So the exception-unwind/catch-handler path is not required.
- **The callee's inline allocation.** `BISECT_SKIP …/run` (run interpreted) still
  corrupts, so `run()`'s JIT `emit_inline_tlab_new` is not it.
- **OSR trampoline callee-saved save/restore order.** The trampoline spills
  callee-saved regs in `LOCAL_REGS` order and the epilogue restores from
  `used_callee_saved`; `available == LOCAL_REGS`, so the set+order match (the
  only divergence is high-half nulling, which `IntLoop`/`AllocLoop` — no longs —
  don't exercise).

## Tried and REFUTED: zeroing the OSR frame

Hypothesis: the OSR trampoline jumps to a loop-header PC *past* the normal
prologue, so the prologue's local zero-init never runs; the frame's
un-written spill/dead-local slots hold stale stack garbage; the conservative
JIT-frame root scanner (`vm/src/jit/conservative_roots.rs`, a blind range scan
that treats any pointer-shaped qword as a root) then "evacuates" a garbage
pointer, and the moving young GC writes a forwarding header over a real object.

Fix tried: zero `[rsp, rsp+frame_size)` at the top of `emit_osr_trampoline`
(jit/src/lib.rs) before writing the live locals/callee-saved.

Result: **did NOT fix it** — AllocLoop still corrupts 3/3 (37–39 events). The
failure mode shifted (rc 127→124, counts slightly lower/steadier), suggesting
frame garbage is at most a *minor* contributor, not the root. Change reverted.

## Leading open hypotheses for the next attempt

The corruption is a moving young-GC **forwarding-header write over a live
object**, i.e. a bad root is enumerated for the OSR'd `main` frame. Since zeroing
the in-frame spill slots didn't fix it, the bad root is elsewhere:

1. **Precise oop maps applied at the wrong PC for OSR.** `cm.oop_maps` are
   collected for the normal top-of-method flow; OSR enters at a loop-header PC.
   If the map consulted at an OSR safepoint flags a slot/register as an oop where
   the OSR'd execution actually holds a non-oop (or a dead local skipped by
   `osr_dead_mask`), GC treats garbage as a root → forwarding write. **Best next
   experiment: force precise-only vs conservative-only root scanning** and see
   which eliminates the corruption (isolates whether the bad root is a
   conservative false-positive or a wrong precise-map entry).
2. **Conservative scan reads outside the zeroed frame** — below `rsp` (red zone)
   or a region the `JitEntryGuard` recorded stack pointer covers but the
   trampoline didn't zero. Check the scan bounds recorded for an OSR frame vs a
   normal JIT frame.
3. **A callee-saved register holding a stale Rust pointer** is enumerated as a
   root via a precise `reg_oops` map (if any), or a `dead_mask`-skipped local's
   register retains a pointer that a later safepoint's map reads.

Useful knobs: `CRATONVM_DBG_OSR=1` (OSR entries + locals), `CRATONVM_GC_VERIFY_STALE=1`
(post-GC stale-root canary — note it perturbs timing), `CRATONVM_GC_ARRAY_GUARD_BT=1`
(Rust backtrace at the corrupt-header site — likely the most direct path to the
writing instruction). A WinDbg hardware write-watchpoint on a victim header byte
during a corrupting AllocLoop run remains the surest localizer.
