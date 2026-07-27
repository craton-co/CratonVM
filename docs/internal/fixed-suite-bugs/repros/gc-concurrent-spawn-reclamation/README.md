# Concurrent-thread-spawn live-object reclamation under GC stress (✅ FIXED 2026-06-29)

**Severity:** high (heap corruption / VM crash). **Manifests** only under frequent GC (tiny heap or `CRATONVM_DBG_GC_STRESS`); at normal heap sizes it is rare/absent.

> **✅ FIXED 2026-06-29; archived from `../../../../known-issues` on 2026-07-01.** The fix is present
> on current `dev`. **ROOT CAUSE:**
> it was **not** a missed root nor a remap gap — it was VM-internal native code holding a
> freshly-allocated `java.lang.Thread` mirror (and its `FieldHolder` / `ThreadGroup` /
> `interruptLock`) in a **bare Rust local across GC-capable allocations**. `currentThread()`
> lazily builds the main mirror (`current_thread_object` → `build_thread_field_holder` →
> `get_or_create_main_thread_group`); each step allocates (name string, FieldHolder, the
> ThreadGroup + `Reference.<clinit>`), and a moving young GC fired there RELOCATES the mirror.
> The per-thread `java_thread_obj` field and the registry are remapped by `update_all_roots`/
> `update_thread_objs_after_gc`, but a **bare Rust local is not a GC root**, so it went stale:
> the subsequent `set_field(holder/group/name/lock, …)` wrote to the OLD (now-zeroed) address
> (`gc::guard` dropped it → live mirror kept `holder == null`), and the stale ref was returned.
> Hence `Thread.<init>`'s `currentThread().getThreadGroup()` NPE'd (`this.holder is null`, then
> `g is null`). **FIX:** in those four `vm_exec.rs` helpers, re-read the mirror from the
> GC-remapped field (`java_thread_obj`) — or, where it isn't field-backed, pin it on
> `native_pin_roots` (which the GC remaps in place) — after every allocating step, and write/
> return the live address. **Validated:** `Spawn` → `DONE`, **0 corruption** (no-stress and at
> `GC_STRESS=1MB` ×3); the `holder`/`group`-null NPEs are eliminated (also in `Fork6Hard`); no
> regression (A2 `ReflRepro` `ok=8000 bad=0`, `bintrees16/18` golden, `Fork6` 3/3 ALL-OK).
>
> **Residual (NOT this bug):** at *pathological* GC frequency (`GC_STRESS` ≤ 256 KB, esp. 4096)
> `Spawn` is **correct + corruption-free** but extremely **slow** (>10 min) — a GC-stress
> *throughput* characteristic (every-few-KB moving GC × heavy thread-spawn allocation on a slow
> box, same class as the `sieve250k` bench), not a correctness issue. And `Fork6Hard` under
> GC stress still hits the separate **A4 FJP task-reclamation** (`ClassCastException`), tracked
> in `fork6-fjp-multithread-jit-root-reclamation.md`.

**Status (historic, pre-fix):** OPEN — deterministic repro + characterization below.

## Deterministic repro

`Spawn.java` — spawn 8 trivial threads × 50 rounds, join each round.

```
javac Spawn.java
CRATONVM_DBG_GC_STRESS=4096 cratonvm --java-home <jdk25> -cp . Spawn
```

Crashes **every run** (3/3 observed) with:

```
java/lang/NullPointerException: Cannot read field "group" because "this.holder" is null
    at java/lang/Thread.getThreadGroup(Thread.java:1789)
    at java/lang/Thread.<init>(Thread.java:699)
    at Spawn.main(Spawn.java:7)
```

(`Thread.<init>` inherits the parent group via `Thread.currentThread().getThreadGroup()`; the receiver there is the **parent/main** thread's `java.lang.Thread` mirror.) Accompanied by `gc::guard` "out-of-bounds field … class_id=ClassId(0) java/lang/Object" warnings and "Stale pointer detected in invokevirtual receiver (all-zero header)" — i.e. a live object's memory has been **zeroed/reclaimed** and something still references the old location.

The same corruption surfaces intermittently at realistic small heaps: `docs/internal/bug-03-repro/XtJitRepro.java --Xmx 24m` corrupts ~1/6 runs (wrong checksum). A trivial **single-threaded** program does NOT reproduce it — **thread spawning is required**, so this is a multi-threaded GC bug.

## Characterization (what it is / isn't)

- **Not a missed mark.** `CRATONVM_DBG_SWEEP_EDGES=1` is silent — no marked/old-gen/root object references an unmarked young object. So the collector did not fail to *reach* a reachable object via heap edges.
- **It is a moving-collector stale reference.** At `DBG_GC_STRESS=4096`, GC fires before anything is JIT-compiled, so the **generational moving (Cheney) young collector** runs (no JIT frames ⇒ `gc_quiescence` not active ⇒ not the non-moving sweep). The all-zero header = a from-space slot that was reset after relocation, still pointed at by a stale reference.
- **The stale reference is to the parent thread's `java.lang.Thread` mirror** (its `holder` field reads as part of an all-zero object).
- **Distinct from BUG-03** (which is JIT-specific). This is a pure interpreter + moving-GC concurrency bug exposed by concurrent `Thread` construction/spawn.

## Remap sites already verified correct (ruled out)

All of these DO forward the thread mirror / fields across a moving GC, so the leak is elsewhere:

- GC initiator frames + `java_thread_obj` + native pins + monitors — `update_all_roots` (`../../../../../vm/src/memory/gc.rs`, steps 1, 10, 21).
- Parked-thread frames + `java_thread_obj` + native pins — `apply_pointer_map_to_thread` (`../../../../../vm/src/runtime/interpreter.rs`).
- Registry `java_thread_obj` mirrors + `unpark` reverse index — `update_thread_objs_after_gc` (`../../../../../vm/src/threading/thread_registry.rs`).
- Blocked-thread snapshots — `fold_pointer_map_into_blocked`.
- `fixup_object_fields` does NOT null unmapped refs (only rewrites forwarded ones), so old-gen refs aren't wrongly zeroed.

## Decisive localization 2026-06-29 (fresh build, `CRATONVM_DBG_BUG03` probe)

A targeted probe at the stale-receiver detection (interpreter.rs, gated
`CRATONVM_DBG_BUG03`) compares the stale receiver against the holding thread's
`java_thread_obj` field and the registry mirror. Result, **every run, on tid=0
(main)**:

```
[BUG03] stale recv=0x..0530 on tid=0 method=java/lang/Thread.getThreadGroup
        | java_thread_obj-field=0x..2998 registry-mirror=0x..2998
        (field==recv:false reg==recv:false)
```

- **The `java_thread_obj` field AND the registry mirror are both FRESH and EQUAL**
  (`0x..2998`) — they were correctly remapped across the GC that moved main's mirror.
- **The stale value (`0x..0530`, all-zero/relocated) exists ONLY in the interpreter
  frame** — the `parent` local (`= currentThread()`) / its operand-stack copy in
  `Thread.<init>`.

⇒ **This REFUTES hypotheses 1 and 3 below** (it is NOT the `thread_obj_for_spawn`
capture, NOT `currentThread()`, NOT the registry). The narrowed root cause is a
**frame-remap-coverage gap**: a moving GC (worker-initiated; reproduces with **n=1**,
`CRATONVM_DBG_SWEEP_EDGES` silent, `NO_SELECTIVE_PROMOTE` no help) relocated main's
mirror and updated the field + registry, but **main's interpreter frame was not
remapped at that GC** — so the `parent` local was stranded at the old address and a
later GC (whose pointer_map no longer contains the now-zeroed old address) can never
recover it. The gap is a GC where main passes through **none** of the three frame-remap
sites: `update_all_roots` (initiator), `apply_pointer_map_to_thread` (safepoint peer),
`check_post_block_gc` (blocked-region wake) — most likely a native window during
`Thread.start()`/`join()` where main is neither parked at the interpreter safepoint nor
registered in the blocked-region protocol. **Next step:** add a per-GC, per-thread
remap-coverage log (record which of the three paths runs for tid=0 each GC) and find the
GC where main's frame is skipped; the fix is to ensure that window remaps main's frames
(enter the blocked-region protocol around the native, or remap at native return).

A start-path hardening was added regardless (worker reads its `java_thread_obj` from the
remapped registry rather than the raw captured `thread_obj_for_spawn`, mirroring the
existing thread-END fix) — correct, but the probe shows it is not this crash's cause.

## Deep-dive 2026-06-29 (cont.) — narrowed to a frame-capture/remap timing gap on the SOLE initiator

Six instrument+rebuild cycles (all gated `CRATONVM_DBG_BUG03`, default-inert; retained
in-tree) pinned the following, which **supersedes the hypotheses below**:

- **The stale value is the `parent` local (`= currentThread()`) in `java/lang/Thread.<init>`,
  slot 7** (`Thread(ThreadGroup,String,int,Runnable,long)` — params fill slots 0–6 incl. the
  cat-2 `long stackSize` at 5–6, so `astore 7` at pc 18 is `parent`; reused as the
  `getThreadGroup` receiver at pc 77–79). Confirmed via `verify_no_stale_refs`
  (`CRATONVM_GC_VERIFY_STALE=1`): `POST-GC ZERO-HEADER LOCAL: Thread.<init> local[7]`.
- **`java_thread_obj` field AND the registry mirror are always fresh + equal** at the crash;
  only the frame copy is stale. So `currentThread()` (which returns the field) is fine — the
  divergence is the captured `parent` copy.
- **It is NOT a kind/tag skip.** The `[BUG03-ulr]` diag shows slot 7 is `is_object=true`,
  `local_kinds=0 (OTHER)`, neither `skip_kind` nor `skip_nonobj` — so `update_local_refs`
  *would* remap it. (There is no distinct object LKIND; objects are `LKIND_OTHER`.)
- **`main` (tid 0) is the SOLE GC initiator** — the worker's body never allocates, so every
  moving GC is main-initiated (23/23). `apply_pointer_map_to_thread` and `check_post_block_gc`
  **never run for main** (0 lines) — consistent, not a peer/blocked gap.
- **The smoking-gun timing:** at the GCs that actually move the mirror (e9 `0x..638`→, e10
  `0x..648`→, e11 `0x..638`→`0x..2aa0`), the slot-7 `[BUG03-ulr]` (in-map) diag does **NOT**
  fire — i.e. **slot 7's address is not in those pointer_maps when `update_all_roots` runs** —
  yet slot 7 ends up holding `0x..638` (the moved-and-zeroed mirror address) at the crash. So
  `parent` is captured/observed around the move window in a place the per-GC frame remap
  doesn't cover (a native-return / Rust transient during `currentThread()`→`astore`, or the
  `Thread.<init>` frame's pc is pre-`astore 7` so slot 7 still holds a not-yet-live value at
  the scan), and the live mirror moves out from under it.

**Remaining work to a fix (1–2 cycles):** trace `parent`'s exact capture (is `currentThread()`'s
return registered as `native_pending_return`/a root in the native-return→`astore 7` window? what
is the `Thread.<init>` frame's `pc` at e9–e11?). Candidate fixes: (a) register the
`currentThread()` native return as a tracked root until stored; (b) on the all-zero Thread-receiver
detection, recover via the fresh `java_thread_obj` when the stale object was the current thread's
mirror. **A start-path mirror hardening + the full `CRATONVM_DBG_BUG03` diagnostic suite are
committed** (branch `feat/precise-maps-a4-finish`).

## Leading hypotheses for the owner (pre-2026-06-29; (1) and (3) since refuted — see above)

1. **A transient thread state with no remap site.** A thread that is mid-`Thread.start()` (the spawning thread) or a just-spawned worker in its bootstrap window may hold/own the mirror in a way that neither `update_all_roots` (initiator), `apply_pointer_map_to_thread` (parked), nor `check_post_block_gc` (blocked) covers — e.g. running native thread-setup code, or after `register`/`set_root_snapshot` but before the worker's first cooperative safepoint. The `thread_obj_for_spawn` raw `ObjectRef` captured at spawn (vm/src/vm/vm_exec.rs) is "captured … and NEVER remapped" by design (it reads back from the registry) — re-audit that read-back across *back-to-back* moving GCs.
2. **Multi-collection staleness:** mirror moved by GC1 (ref updated), then GC2 moves it again while the holding thread is in a window not covered by a remap site → the ref from GC1's new address becomes stale.
3. **`Thread.currentThread()` path:** confirm it always resolves the mirror through a remapped source (registry / `thread.java_thread_obj`) and never a separately-cached `ObjectRef`.

## Useful diagnostics

`CRATONVM_DBG_SWEEP_EDGES`, `CRATONVM_DBG_SWEEP_ZERO`, `CRATONVM_DBG_MTROOTS`, `CRATONVM_DBG_CORRUPT_FRAMES`, `CRATONVM_DBG_A2`. Note: the env var is **`CRATONVM_DBG_GC_STRESS`** (now also aliased to `CRATONVM_GC_STRESS`).
