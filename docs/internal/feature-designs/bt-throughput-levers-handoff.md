# Binary-Trees Throughput Levers — Handoff

Status: **two codegen levers landed on dev** (2026-06-22); the remaining gap is
architectural. NOT pushed.

> **UPDATE 2026-06-22 — architectural lever #1 (compact reference-field layout)
> implemented + validated** on branch `feat/compact-ref-fields` (worktree
> `CratonVM-movingyoung`), gated **`CRATONVM_COMPACT_REF_FIELDS`** default-OFF;
> flag-off byte-identical to dev. Reference instance fields are now stored as
> bare 8-byte pointers (oop-map + per-class layout registry in `cratonvm-types`;
> GC scan/remap, heap accessors, and JIT helpers all compact-aware). **Correct:**
> bt10/14/16/18 + GC_STRESS + a HashMap/ArrayList/inheritance mix all ==
> HotSpot. **Footprint:** TreeNode 56 B vs 72 B, one fewer young GC at bt18.
> **Throughput:** **net win** — bt16 ~10 % faster, bt18 parity (compact-aware
> inline codegen: baked per-field offset → inline 8-byte ref load/store +
> compact-size inline TLAB; the offset is hierarchy-invariant so the declaring
> class's layout suffices). The win was initially masked because the
> execute/OSR compile paths use the `x64::compile` wrapper and bypassed the
> compact_field_info — fixed via a thread-local the wrapper consumes. See
> [`compact-ref-field-layout.md`](../../feature-designs/compact-ref-field-layout.md).

Context: the object-`binarytrees` (bt) throughput gap vs HotSpot. The
long-standing "make a moving young gen the default" plan
([`default-moving-young-gen.md`](../../feature-designs/default-moving-young-gen.md))
was the presumed next step; this work **refuted that premise by measurement**
and instead attacked the actual per-node mutator costs that dominate the gap
(see [`gap-bintrees18-gc-throughput.md`](../gaps/gap-bintrees18-gc-throughput.md)
"next levers").

The two levers compound on allocation-heavy code and are both
**correctness-preserving** — every checksum is identical to HotSpot
(bt10=135854, bt14=3222190, bt16=14985902, bt18=68332206).

---

## 0. Moving young gen — measured, REFUTED (do not build)

Ran the design's own mandated "MEASURE FIRST" gate on a fresh build. Across five
GC configurations bt18 stays 46–50 s; **`CRATONVM_NO_GC` does not speed bt18 up**
(it is marginally slower), only **2** sweeps run all-run, and forced-moving
(±shadow stack) still produces the **67674804** under-count (incomplete shadow
coverage). Net: **GC is ≈0 % of the gap**; a moving young gen cannot close it and
is unsafe until shadow coverage is finished. Recorded in the design doc's
"Prerequisite recheck" section. The gap is per-node *mutator* cost.

## 1. Per-node trivial-constructor dispatch — FIXED, default-on

Dominant per-allocation cost: `invokespecial C.<init>()V` of every `new C()`
whose `C` has the empty default ctor was dispatched through `jit_invoke_dispatch`
**once per object** (`TreeNode.<init>` 135,854× at bt10). Eliminated by emitting
such a site **as `java/lang/Object.<init>`** so the existing fix-2 codegen elision
drops it (`is_elidable_construction` is the sound predicate: body is exactly
`aload_0; invokespecial Object.<init>()V; return`).

- **All three JIT compile tiers** covered: `execute` first-call, `try_osr`, and
  `try_compile` (callee/IR), with **app-class** coverage everywhere (the resolver
  uses `load_class_concurrent`, not `find_class_by_name`, which misses app
  classes in the JIT-compile context — the key gotcha).
- Dispatch 135,854 → **0** (bt + OSR-tier); **bt16 +18 %**; checksums == HotSpot.
- Commits: `6501c016` (execute) / `46b7b9ad` (try_osr + try_compile) /
  `767929ba` (app-class resolver). Opt-out `CRATONVM_NO_CTOR_DIRECT_CALL`.
- Dead end recorded: Route B (eager-compile the ctor → direct call) — a nested
  `try_jit_compile_callee` inside `execute`'s first-compile returns `None`.

## 2. Inline reference `putfield` (gap-doc lever #1) — landed, **default-OFF**

A reference `putfield` previously always CALLed `jit_putfield_object` (4-arg
marshal + SATB pre-barrier + 16-byte tagged-`Value` store + card write-barrier),
~136 M times in bt18. Now emits an **inline 16-byte `Value` store** on the
barrier-free fast path, bailing to the helper otherwise:

- non-null receiver; **young** receiver (`gc_flags` byte @ header offset 21,
  `GC_FLAG_OLD_GEN` = bit 0 → no card); **null old value** (cell payload @ +8 == 0
  → no SATB, regardless of marking); index in bounds (`num_slots` u32 @ 16).
- Store = `mov qword [obj+cell+0], 4` (tag = `Value::Object`(4) + zeroed pad) +
  `mov qword [obj+cell+8], val`.
- **Barrier-free by construction**: every SATB-requiring (non-null old) or
  card-requiring (old receiver) store bails to the validated helper.
- Commit `b385fe09`, gated **`CRATONVM_JIT_INLINE_PUTFIELD` (default-off,
  byte-identical to dev when off)**. **bt16 +9 %.**
- Validated == HotSpot ON: bt10/14/16/18, bt16 + `CRATONVM_GC_STRESS`, a dedicated
  old→young card-barrier stress (`CardTest` + GC_STRESS = 529637376), CtorTest,
  CollSmall.
- **Remaining before a default-on flip:** the app gauntlet (WildFly/Spring) on a
  quiet box. The concurrent-marking SATB path is correct-by-bail but not
  app-soaked.

Code: `jit/src/x64.rs` — flag `inline_putfield_enabled` (~line 1866) and the
`0xb5` `b'L'|b'['` branch (~line 18085).

---

## The remaining gap is ARCHITECTURAL — 72-byte tagged `Value` field cells

With the two levers landed, the dominant residual cost on allocation-heavy code
is **memory traffic**: object fields are stored as the 16-byte Rust enum `Value`
(tag dword + payload), so a 2-reference node (`TreeNode {l, r}`) is
HEADER(40) + 2×16 = **72 bytes** vs HotSpot's ~24–32 bytes — **~2–3× the bytes
allocated, written, and swept per object**. This shows up as allocation
bandwidth, cache pressure, and GC-sweep volume on *every* allocation-heavy
workload, and **no codegen lever can remove it** — it is a data-layout decision.

This is a **larger redesign than a codegen lever** and should be its own design
doc. Options, roughly in increasing scope/payoff:

1. **Compact reference-field layout** (parallel to the existing compact
   `Object[]` array layout): store reference fields as a bare 8-byte pointer
   instead of a 16-byte tagged `Value`. The inline `putfield`/`getfield` already
   special-case reference type tags, so the codegen half is close; the heavy part
   is migrating every field reader/writer (interpreter, natives, GC field walk,
   scalar replacement, monitor/identity paths) off `read_value/write_value` for
   reference fields, plus the GC's field-scan and remap. Halves node size for
   reference-heavy classes.
2. **Typed field cells** (per-field width from the class layout): primitive
   fields stored at their natural width (4/8 bytes) rather than 16. Needs the
   per-field descriptor plumbed into the heap layout + every field access.
3. **Full object-model change** to a HotSpot-like compact header + packed typed
   fields. Largest, touches the entire heap/GC/interpreter/JIT field surface.

Recommended first slice: **(1)** — biggest payoff per unit risk, and the inline
field codegen from lever #2 already establishes the reference-cell access
pattern. Gate it, validate against the same harness (checksums + GC_STRESS +
the old→young `CardTest`), and treat the GC field-walk/remap migration as the
critical correctness surface.

---

## Validation harness / repros

Build: `build-move.bat` → `cvmove.exe` (this box is slow — 8–24 min/build). JDK
`C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot`. Throughput is
load-robust via **min-of-N interleaved** runs (not single wall-clock).

- bt repro: `../fixed-suite-bugs/repros/gc-stress-bintrees-main-args/binarytrees.java`
- Correctness oracles built this session (regenerate as needed):
  `CtorTest` (elision boundary: empty vs field-init vs non-Object-super ctors),
  `CollSmall`/`CollTest` (JDK POJOs), `OsrAlloc` (OSR tier), `IfaceApp`/`CalleeApp`
  (try_compile tier), **`CardTest`** (old→young card barrier under GC_STRESS).
- Flags: `CRATONVM_JIT_INLINE_PUTFIELD` (lever #2, default-off),
  `CRATONVM_NO_CTOR_DIRECT_CALL` (lever #1 opt-out), `CRATONVM_GC_STRESS`,
  `CRATONVM_DBG_JIT_DISPATCH` / `CRATONVM_DBG_JITC` / `CRATONVM_DBG_CTOR_FIX`.

## Cross-links

- [`gap-bintrees18-gc-throughput.md`](../gaps/gap-bintrees18-gc-throughput.md) —
  the measured root-cause analysis + "next levers" this work executed.
- [`default-moving-young-gen.md`](../../feature-designs/default-moving-young-gen.md)
  — the refuted plan + the "Prerequisite recheck" measurement.
