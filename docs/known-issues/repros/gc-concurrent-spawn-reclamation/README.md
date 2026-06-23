# Concurrent-thread-spawn live-object reclamation under GC stress (OPEN)

**Severity:** high (heap corruption / VM crash). **Manifests** only under frequent GC (tiny heap or `CRATONVM_DBG_GC_STRESS`); at normal heap sizes it is rare/absent. **Status:** OPEN — deterministic repro + characterization below; not yet root-caused to a single site.

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

- GC initiator frames + `java_thread_obj` + native pins + monitors — `update_all_roots` (`vm/src/memory/gc.rs`, steps 1, 10, 21).
- Parked-thread frames + `java_thread_obj` + native pins — `apply_pointer_map_to_thread` (`vm/src/runtime/interpreter.rs`).
- Registry `java_thread_obj` mirrors + `unpark` reverse index — `update_thread_objs_after_gc` (`vm/src/threading/thread_registry.rs`).
- Blocked-thread snapshots — `fold_pointer_map_into_blocked`.
- `fixup_object_fields` does NOT null unmapped refs (only rewrites forwarded ones), so old-gen refs aren't wrongly zeroed.

## Leading hypotheses for the owner

1. **A transient thread state with no remap site.** A thread that is mid-`Thread.start()` (the spawning thread) or a just-spawned worker in its bootstrap window may hold/own the mirror in a way that neither `update_all_roots` (initiator), `apply_pointer_map_to_thread` (parked), nor `check_post_block_gc` (blocked) covers — e.g. running native thread-setup code, or after `register`/`set_root_snapshot` but before the worker's first cooperative safepoint. The `thread_obj_for_spawn` raw `ObjectRef` captured at spawn (vm/src/vm/vm_exec.rs) is "captured … and NEVER remapped" by design (it reads back from the registry) — re-audit that read-back across *back-to-back* moving GCs.
2. **Multi-collection staleness:** mirror moved by GC1 (ref updated), then GC2 moves it again while the holding thread is in a window not covered by a remap site → the ref from GC1's new address becomes stale.
3. **`Thread.currentThread()` path:** confirm it always resolves the mirror through a remapped source (registry / `thread.java_thread_obj`) and never a separately-cached `ObjectRef`.

## Useful diagnostics

`CRATONVM_DBG_SWEEP_EDGES`, `CRATONVM_DBG_SWEEP_ZERO`, `CRATONVM_DBG_MTROOTS`, `CRATONVM_DBG_CORRUPT_FRAMES`, `CRATONVM_DBG_A2`. Note: the env var is **`CRATONVM_DBG_GC_STRESS`** (now also aliased to `CRATONVM_GC_STRESS`).
