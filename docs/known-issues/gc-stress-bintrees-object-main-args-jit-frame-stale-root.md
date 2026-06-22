# GC: object-binarytrees JIT-frame stale root under extreme `GC_STRESS` (`main` reads `args`)

**Status:** 🔴 **OPEN** — a residual member of **Family A** (GC root coverage under JIT). Found
2026-06-21. JIT-only; reproduces **only at extreme young-GC frequency** (`CRATONVM_DBG_GC_STRESS`
≤ 65536, i.e. a young GC every ≤ 64 KB of allocation) — i.e. **below** the 524288 / 4 MB thresholds at
which the A3 precise-oop-maps fix (`32649b56`, default-on) was verified. Precise maps **on or off make no
difference**; this is *not* closed by the A3 fix.

## TL;DR

The **object** version of binarytrees (recursive `TreeNode{left,right}` allocation), run under
`CRATONVM_DBG_GC_STRESS=4096`, intermittently/deterministically corrupts: a `putfield` in
`bottomUpTree` writes through a `node` reference that points at an **all-zero / garbage header**
(`set_field … obj=0x… index=0 num_slots=0 class_id=0 java/lang/Object`), and the young-sweep walk
trips on an `inconsistent header — kind=Object but array_length=2 (num_slots=80, class_id=0)`.

The corruption is **caused by `main`'s compiled code, and specifically by `main` loading its `args`
parameter** — a reference in local 0 that the register allocator colors to a callee-saved register
**shared with int locals**. The corruption itself is a JIT-frame raw pointer going **stale across a
young GC**; the Java heap stays self-consistent.

## Symptom

```
WARN cratonvm_gc::gen_heap: GC: inconsistent header — kind=Object but array_length=2
     (num_slots=80, class_id=0); inline-alloc forgot to set kind=Array. Treating as corrupt …
WARN cratonvm::gc::guard: gen_heap::set_field: out-of-bounds field write dropped …
     obj=0x1423f1558 index=0 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
     real_field_count=Some(0) value=Object(Some(ObjectRef { ptr: 0x1423f0130 }))
Exception in thread "main" java/lang/ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0
```

Two observable end-states (both are the same bug; timing decides which):
- **crash / empty output** (the `set_field` AIOOBE above), or
- **wrong checksum** — e.g. the `args.length` variant deterministically prints `1348958` instead of
  `3222190` (an *under*-count — live tree nodes lost).

## Reproduce

Canonical: object-based binarytrees that accumulates one checksum, `maxDepth` from `args[0]`.
Minimal repros and clean controls are in
[`repros/gc-stress-bintrees-main-args/`](repros/gc-stress-bintrees-main-args/).

```bash
cd docs/known-issues/repros/gc-stress-bintrees-main-args
javac -d . VAAload.java
for i in $(seq 1 8); do CRATONVM_DBG_GC_STRESS=4096 cratonvm.exe -Xmx6g -cp . VAAload 14; done
# HotSpot / clean: 3222190 every run.  CratonVM JIT: empty (crash) ~8/8.
```

bt-checksums: bt10=`135854` bt14=`3222190` bt16=`14985902` bt18=`68332206`.

## What is firmly established (empirical, all on dev with the binary rebuilt this session)

1. **JIT-only.** `--nojit` produces the **correct** answer — it is merely *pathologically slow* at this
   GC frequency (bt10 took 171 s via the O(n) free-list churn; bt14 exceeds the 120 s watchdog and
   *looks* like a hang, but is correct given time). The interpreter never corrupts.
2. **`main`'s compilation is the corruptor.** `CRATONVM_JIT_BISECT_SKIP=<Class>.main` → **clean 8/8**.
   Skipping `bottomUpTree` or `itemCheck` instead does **not** help. (`bottomUpTree`'s codegen is in
   fact correct: it spills `node` to `[rbp-10h]`/`[rbp-30h]` before each recursive call and reloads
   after — and it is byte-identical between the crashing and clean variants.)
3. **The trigger is `main` loading `args`** (`aload_0`, a reference in local 0):
   - `VAAload` (`args[0].length()`) → **crash 8/8**
   - `VArgLen` (`args.length`) → **wrong output** (`1348958`)
   - `VParseLocal` (`Integer.parseInt("14")`, never touches `args`) → **clean** → so `parseInt` is innocent
   - `VStatic` (`maxDepth = seed`, a non-constant `static int`, never touches `args`) → **clean** → so it
     is **not** "`maxDepth` is non-constant"
   - `RHard` (`maxDepth = 14`, hard-coded) → **clean**
   In the crashing variants the register allocator colors `args` (L0, a reference) to a **callee-saved
   register shared with int locals** — `main`'s local map: `L0=r12 L1=rdi L2=r12 L3=rsi L4=frame
   L5=rbx(longLivedTree) … L9=r12` (so `r12` carries `args`, `stretchDepth`, and `i`).
4. **GC-frequency boundary:** crash at `GC_STRESS` ≤ 65536; **clean** at ≥ 524288 and at 1048576, and
   clean with no stress knob at all. The A3 precise-maps fix was verified at 524288 / 4 MB — this bug
   lives *below* that band.
5. **Precise maps do not fix it.** `CRATONVM_NO_PRECISE_JIT_MAPS` (precise OFF) and default (precise ON)
   both crash identically at 4096. Same for `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`,
   `CRATONVM_SHADOW_STACK`, `CRATONVM_DBG_FULLSTACK_SCAN`, `CRATONVM_JIT_C2_FIRST_CALL=1`,
   `CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_ROOTSNAP_CACHE=0` — **none** fix it.
6. **Not the inline allocator.** `CRATONVM_JIT_DISABLE_INLINE_NEW=1` still crashes (so the
   `emit_inline_tlab_new` header write is not the cause; the inline header is in fact correct).
7. **Not selective promotion / not the moving-promotion abort.** `CRATONVM_NO_SELECTIVE_PROMOTE=1` and
   `CRATONVM_NO_GC_PROMOTION_GUARD=1` still crash.
8. **The reclaimed receiver was NOT recorded by the non-moving sweep.** With `CRATONVM_DBG_SWEEP_ZERO=1`,
   `sweep_zero_lookup(receiver)` returns **None** — the 0-slot object the `set_field` hits was *not*
   zeroed by `sweep_young_non_moving`'s zeroing path.
9. **The heap stays self-consistent.** `CRATONVM_DBG_HEAP_STALE=1` (`verify_heap_object_fields`, runs
   after every GC) reports **zero** stale heap fields. The dangling reference therefore lives **only in
   a JIT frame slot** (`bottomUpTree`'s `node` home), which that verifier does not inspect.
10. **`CRATONVM_DBG_FULLSTACK_SCAN=1` does not fix it** → the missed/stale reference is **not on the
    scanned native stack** at the GC point.

## Leading hypothesis (NOT yet proven)

Facts 8–10 together rule out the "non-moving sweep zeroes a live young object whose only root is in a
JIT frame" mechanism (that is the rest of Family A, e.g. A3). Here the receiver is *not* sweep-zeroed,
the heap is consistent, and a full-stack scan does not recover it. The remaining consistent story is:

> While `main`'s compiled frame is live and holds **raw young-gen pointers** in JIT frame slots, a young
> GC runs that **relocates / resets** young memory (the moving Cheney collector, or the selective-
> promotion evacuation), and one of those JIT-held pointers is **not remapped** — so after the GC the
> frame slot is a **stale pointer** into reset/reused young space → `node` reads a garbage/zeroed header
> → `set_field` AIOOBE; the walk that strides that region reports `inconsistent header`.

This implies a `gc_quiescence` / JIT-entry-coverage question specific to `main` (the program entry
point): if `main`'s active compiled frame did **not** keep quiescence active (so the *moving* collector
ran instead of the non-moving sweep), its raw JIT pointers would not be remapped. That `main` reading
`args` is the trigger, and that the register allocator shares `args`'s callee-saved register with int
locals, suggests the precise oop set / live-range of that shared register at `main`'s safepoints is
where coverage breaks.

**This last mechanism is a hypothesis, not proven** — the session was redirected before the confirming
experiments were run.

## Next steps to confirm / fix

1. **Confirm which collector ran.** Instrument `collect_garbage`'s collector-choice (`gc_quiescence::
   is_active()`, `promotion_oom_risk`, `force_moving`) and `record_swept`/the evacuation path to print
   whether the receiver's address was **relocated** (moving / selective promotion) vs left in place.
   If it was relocated, dump the `pointer_map` entry and check it against `main`'s / `bottomUpTree`'s
   frame slots after `update_all_roots` / `remap_active_jit_frames`.
2. **Check `gc_quiescence::depth()` while `main` runs compiled.** `CRATONVM_DBG_CORRUPT_FRAMES` prints a
   `[quiesce-leak]` line on imbalance; add a symmetric "quiescence INACTIVE while a JIT frame is live"
   assertion at the GC entry. If `main`'s entry does not bump quiescence (or is not on
   `JIT_ENTRY_CHAIN`), that is the bug.
3. **Inspect the shared callee-saved register's oop liveness at `main`'s safepoints** via
   `CRATONVM_DBG_JIT_DISASM=<Class>.main` (prints the `L0=r12 …` map + per-PC `bc@` labels) and
   `compute_local_oop_masks` — the `args` oop sharing `r12` with `stretchDepth`/`i` is the structural
   anomaly that distinguishes the crashing from the clean variants.
4. **Eventual fix:** **precise JIT stack roots** (`project_precise_jit_stack_maps`) — but note this bug
   persists with the *current* precise-maps implementation default-on, so either the maps are wrong for
   this case (live-range of a shared oop/non-oop register) or the gap is on the *moving*/evacuation
   remap side rather than the mark-scan side. The precise map must cover this case **and** be consumed
   by the relocation remap (`remap_active_jit_frames`), not only the mark scan.

## Relationship to existing Family-A members

- Closest to **A2** ([reflrepro-register-resident-jit-root-handoff.md](reflrepro-register-resident-jit-root-handoff.md)):
  both show `inconsistent header` on the young-sweep walk and are **not** fixed by precise maps. A2's
  repro is reflection / String-array churn; this one is object-graph recursion with a sharp,
  one-opcode trigger (`main` reads `args`) and a clean GC-frequency boundary. They may share a root
  cause (a JIT-held young pointer not remapped across an evacuating young GC).
- **A3** (register-invisibility, FIXED by precise maps default-on) is the *adjacent* class this bug is
  **not** — A3's missed root is recovered by a stack scan; this one is not (fact 10), and precise maps
  do not help (fact 5).
- See the README's **Family A** section and `project_precise_jit_stack_maps`.

## Diagnostic knobs used

`CRATONVM_DBG_GC_STRESS`, `CRATONVM_JIT_BISECT_SKIP`, `CRATONVM_DBG_JIT_DISASM`,
`CRATONVM_DBG_SWEEP_ZERO` (+ the `[SWEEP-ZERO-HIT]` line in the `set_field` OOB guard),
`CRATONVM_DBG_HEAP_STALE`, `CRATONVM_DBG_FULLSTACK_SCAN`, `CRATONVM_NO_PRECISE_JIT_MAPS`,
`CRATONVM_JIT_DISABLE_INLINE_NEW`, `CRATONVM_NO_SELECTIVE_PROMOTE`, `CRATONVM_DBG_FORCE_MOVING`.
