# GC: object-binarytrees JIT-frame stale root under extreme `GC_STRESS` (`main` reads `args`)

**Status:** ✅ **FIXED on the default path (Windows)** — dev merge `77c98761` / fix `6e92f0e5`.
Root-caused (instrumented) to the generational MOVING young collector relocating the **compiled
entry-point `main`**'s live objects: `main`'s JIT frame is invisible to `gc_quiescence` (it is
invoked via `Vm::invoke` WITHOUT a `JitEntryGuard`), so `is_active()` is false and the collector
moves instead of running the non-moving sweep. The fix detects an unregistered JIT frame on the
native stack (a JIT code return address via `cratonvm_jit::lookup_jit_code_range`), conservatively
marks its oops with a full-stack scan, and runs the non-moving sweep (pin, don't relocate).
**Validated:** `VAAload 14 @CRATONVM_DBG_GC_STRESS=4096` → `3222190` 5/5 (was crash 8/8); all repro
variants ==HotSpot; `bt16` throughput byte-identical; `cratonvm-gc` 732/732 (incl. regression
`non_moving_sweep_when_unregistered_jit_frame_on_stack`). **Follow-up done:** the
`JIT_ENTRY_CHAIN`-non-empty case (an unregistered `main` ABOVE a registered chain) is now also
covered — the detection scans `[cover_hi, stack_high)` regardless of chain length and marks the
above-chain frame (no collector-choice change when the chain is non-empty, so no throughput cost;
dev merge `9c64d691`). **Remaining residual:** Windows-only — the detection reuses
`current_thread_stack_high` (Win32 `GetCurrentThreadStackLimits`); a portable (pthread) port is a
tracked follow-up that needs a non-Windows environment to validate. See the CRACKED + fix notes below.

Originally a residual member of **Family A** (GC root coverage under JIT). Found 2026-06-21.
JIT-only; reproduced **only at extreme young-GC frequency** (`CRATONVM_DBG_GC_STRESS` ≤ 65536, i.e.
a young GC every ≤ 64 KB) — **below** the 524288 / 4 MB thresholds at which the A3 precise-oop-maps
fix (`32649b56`) was verified. Precise maps **on or off** made no difference; not closed by A3.

---

## ⚠️ UPDATE 2026-06-22 (re-investigation) — several earlier conclusions are now CORRECTED

Re-run on current dev (binary built from dev tip after the G1 mixed-GC merge `3d067512`; the JIT
root path is unchanged by that merge). **The state has FLIPPED and two leading hypotheses are
refuted.** Read this section before the older body below.

1. **Generational CRASHES; G1 is CLEAN** (the doc/memory had it backwards). `VAAload 14`
   `CRATONVM_DBG_GC_STRESS=4096`:
   - **Generational (default): deterministic CRASH** — empty output, `inconsistent header`,
     `set_field out-of-bounds dropped` (receiver = a zeroed `java/lang/Object`), then
     `ArrayIndexOutOfBoundsException`. `RHard` (the `args`-free control) is **clean `3222190`** —
     the `main`-reads-`args` discriminator still holds. Crashes at every stress ≤ 262144; **clean at
     ≥ 524288** (boundary 262144↔524288, unchanged).
   - **G1 (`-XX:+UseG1GC`): CLEAN `3222190`, zero guard warnings, every stress.** G1's
     conservative-JIT-root **region pin** (`5d761809`, dev `f564b156`) excludes JIT-referenced
     regions from the CSet, so G1 never relocates them → it *works around* this bug. **So A5 today
     is a generational moving-young problem (the DEFAULT collector), not a G1-specific one.** Fixing
     it would also let G1 drop the pin workaround (a throughput win).

2. **It is NOT a relocation / moving-GC stale-pointer bug** (refutes this doc's old "leading
   hypothesis" and the A2 "moved-but-not-remapped" story). Every collector path gives the **byte-
   identical** failure: `CRATONVM_NO_SELECTIVE_PROMOTE=1`, `CRATONVM_DBG_FORCE_MOVING=1`,
   `CRATONVM_SHADOW_STACK=1`, `CRATONVM_NO_PRECISE_JIT_MAPS=1` — all crash identically. If
   relocation were the mechanism, disabling it (`NO_SELECTIVE_PROMOTE`, non-moving sweep) would
   change the outcome. It does not. **The receiver is a *zeroed* header → a live young object that
   was MARK-missed and FREED (swept), then its slot reused** — a missed *mark* root, not a missed
   *remap*.

3. **`CRATONVM_DBG_SWEEP_EDGES` stays silent** — no root / heap-field / dirty-card edge reaches the
   freed object (confirming the gen_heap.rs:139-144 note). So at the freeing GC the object is
   reachable from *nothing the marker scans*.

4. **NEW — disassembly proves `node` IS spilled to the stack at every safepoint, yet is still
   missed.** `CRATONVM_DBG_JIT_DISASM=VAAload.bottomUpTree`: `bottomUpTree`'s locals are
   `L0=depth=r13`, **`L1=node=r12` (a callee-saved register)**. The canonical spill slot is
   `[rbp-10h]` (`r12→[rbp-10h]` is re-emitted before *every* GC-capable call: the two recursive
   calls at `0x18e`/`0x1fd` (`0x17f`,`0x1ee`) and after each, plus `[rbp-30h]`). At both `putfield`
   safepoints (`0x1cb`,`...`) the receiver is reloaded from `[rbp-30h]` and `[rbp-10h]` still holds
   `node`. **So `node` is present on the stack in `bottomUpTree`'s frame at every safepoint** — and
   yet `FULLSTACK_SCAN=1` (scan the *entire* native stack) **does not recover it** (the
   `inconsistent header` goes away but the `set_field`-into-zeroed-`Object` and the AIOOBE remain).
   `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` and the dev-default callee-saved-operand-stack-oop spill
   (`7c7d8148`, which only covers operand-stack temporaries, **not locals**) also do not fix it.

### ✅ CRACKED (2026-06-22, instrumented) — it is a LOST-TAG object on the MOVING path, NOT a bottomUpTree JIT-frame stale root

An instrumented build (probe in `collect_roots` + `collect_garbage_inner` + the sweep, gated
`CRATONVM_DBG_A5`, since reverted) settled it. **Every prior "bottomUpTree node in r12 / register-
only / scan coverage-or-accept" framing is WRONG for this repro** — `bottomUpTree` is **not even
JIT-compiled** at the crash. The decisive observations on `VAAload 14 GC_STRESS=4096`:

1. **The corruptor is `main`'s COMPILATION, decisively.** `CRATONVM_JIT_BISECT_SKIP=VAAload.main`
   → **clean `3222190`** (30+ GCs, no corruption); default (main compiled) → crash. `main` compiles
   at startup (the `DBG_JIT_DISASM` "first VAAload.main" event fires **before GC#0**).
2. **`bottomUpTree` is NEVER compiled before the crash** (no `first VAAload.bottomUpTree` event; no
   OSR event). So the corruptee `node` lives in an **interpreter** frame, not a JIT frame — the
   whole `[rbp-10h]`/`r12` disasm analysis below was chasing the wrong frame.
3. **Every corrupting GC is a MOVING (Cheney) collection at `quiescence_depth=0`, `jit_thread=false`**
   — i.e. the *interpreter* is running (no JIT frame registered) and the **relocating** collector
   runs. The crash is very early (GC#2–3, old gen empty → young-only).
4. **`--nojit` does NOT corrupt** — it only hits the 120 s watchdog (interpreted `bottomUpTree` is
   slow). So the bug needs JIT *enabled* (so `main` compiles) but does not need JIT *running* at the
   GC.

**Mechanism (matches an already-documented hazard).** `main` runs compiled, calls a method across a
JIT↔interpreter boundary, and **a JIT callee's object return value reaches an interpreter local /
operand slot under a NON-OBJECT tag** (lost-tag). The tag-filtered `Frame::scan_local_objects`
omits it, so it is not a GC root. Because `quiescence=0` the **moving** collector runs and
**relocates/frees** that unrooted-but-live young object → its slot is reused → the stale reference
reads an all-zero `java/lang/Object` header → `set_field` OOB → `ArrayIndexOutOfBoundsException`.
This is the SAME class as the Fork6 FJP lost-tag bug — see the comment on
`vm/src/memory/roots.rs::conservative_locals_enabled` / the `scan_locals_conservative` call there,
which literally describes *"a JIT callee's object return value can reach an interpreter local under
a non-object tag (e.g. `main`'s `f = POOL.submit(t)`); the tag-filtered `scan_local_objects` then
omits it … → stale all-zero receiver."* That recovery scan exists **but is gated to
`CRATONVM_REAL_FORKJOINPOOL` AND the non-moving sweep** (`base && gc_quiescence::is_active()`). For
`VAAload` neither holds (no FJP gate; `is_active()=false` → moving collector), so the lost object is
never recovered. The recovery is deliberately OFF on the moving path because conservatively rooting
a pointer-shaped `long` there would get it **relocated and corrupted** — so the fix cannot simply
flip the gate.

**Fix direction.** Preserve the object tag across the JIT→interpreter return boundary (the real fix
— make the returned reference land in the interpreter slot tagged as an object so the precise scan
roots it), OR provide a precise (rewritable, not conservative-pin) recovery of JIT-boundary return
oops that is safe on the moving path. The `main`-reads-`args` discriminator is a red herring for the
*frame* but real for *triggering main's compilation + the specific call/return shape*; `RHard`
(args-free) is clean because its `main` compiles to a shape that does not lose the tag (or does not
compile the same way).

**Confirming experiment for the next session:** add a force-non-moving knob (or reuse the FJP gate
path) and verify that with the conservative-locals recovery engaged the crash disappears — that
pins "lost-tag on the moving path" beyond doubt — then implement the precise tag-preserving fix and
verify `VAAload`/`binarytrees` are clean at `GC_STRESS=4096` with JIT on.

### (historical — the "register-only / coverage-or-accept" hypotheses below are SUPERSEDED by the CRACKED section above)

---

### (older body — superseded where it conflicts with the 2026-06-22 update above)

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
[`repros/gc-stress-bintrees-main-args/`](../repros/gc-stress-bintrees-main-args/).

```bash
cd docs/internal/springboot/gc-stress-bintrees-main-args
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

- Closest to **A2** ([reflrepro-register-resident-jit-root-handoff.md](../../known-issues/reflrepro-register-resident-jit-root-handoff.md)):
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
