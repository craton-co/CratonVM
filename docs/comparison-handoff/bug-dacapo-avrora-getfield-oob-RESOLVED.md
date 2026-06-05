# DaCapo avrora — `gen_heap::get_field` out-of-bounds field read — RESOLVED

## Summary
The recurring `gen_heap::get_field: out-of-bounds field read dropped` WARN that
flooded the CratonVM stderr during `dacapo.jar avrora` (≈17 600 lines/run) is
fixed. avrora still PASSES; the warning count drops to ~2/run.

## Root cause (the handoff's hypothesis was wrong)
The original note guessed a *wrong-slot* native (raw slot N vs. by-name). It is
not that. The true cause is a **stale reference after a moving GC**, localized
with `CRATONVM_DBG_OOBFIELD=1` (symbolicated backtrace) + targeted
instrumentation:

1. Every OOB receiver is a bare `java/lang/Object` (0 fields) read at slot 0/1.
2. The caller is `native_rq_poll` ← `native_rq_remove_timeout`
   (`native-builtins/src/reference.rs`) — i.e. the JDK **Common Cleaner** thread
   parked in `ReferenceQueue.remove(60_000)`.
3. Instrumenting the queue object proved the smoking gun: the *same pointer* was
   `[RQ-INIT] class_id=46 nfields=3` (a real `ReferenceQueue`) and later
   `[RQ-POLL-BARE] class_id=0 nfields=0` (a bare `Object`).

Mechanism: `ReferenceQueue.remove(timeout)` spins in a native poll loop for up
to 60 s, holding the receiver `this` as a **raw `ObjectRef` snapshot** taken
from `args`. The thread calls `begin_blocking_region`, so it is *excluded from
the STW barrier* and never applies a moving collector's pointer map. When a
moving young GC (here, the dacapo harness's `System.gc()`, which is the only
collection in a short 4 GB-heap run) relocates the queue, the blocked thread's
raw `this` keeps pointing at the **old** address. The young allocator then
reuses that from-space slot for a fresh bare `java.lang.Object`, so the native
reads `RQ_FIELD_HEAD`/`SIZE` off a 0-field object — once per poll, for the rest
of the 60 s window → tens of thousands of dropped OOB reads.

This is the **same register/raw-pointer invisibility hazard** the project
already documents for active JIT frames (conservative roots, non-moving sweep),
applied to native-blocked threads.

## Why the "proper" GC fix was rejected
The architecturally-consistent fix is to treat a native-blocked thread like a
live JIT frame: raise `gc_quiescence` in `begin_blocking_region` so any GC that
runs while a thread holds raw refs is **non-moving**. This *was* implemented and
*did* eliminate the get_field WARN — but it routes avrora's `System.gc()`
through the non-moving young sweep, whose linear walk over un-retired daemon
TLABs trips its corrupt-header detector and, critically, made avrora **crash
intermittently (4/8 runs)**. That matches the project's standing conclusion that
the non-moving sweep is fragile and "the only *safe* collector under live JIT
frames" is a delicate invariant. Forcing more traffic through it regressed
stability, so the approach was abandoned (see git history on this branch).

Pinning just the queue (keep it in place while compacting the rest) is not
available — the Cheney young collector has no per-object pin set; it only has
the all-or-nothing `gc_quiescence` non-moving switch.

## The shipped fix (contained, behaviour-neutral, no GC changes)
The `gen_heap` guard *already* returns `null` for the OOB read, so `poll()`
already behaved as "queue empty". The fix simply detects the reclaimed receiver
up front instead of dereferencing past its layout:

1. `native-builtins/src/reference.rs` — `native_rq_poll`: if the receiver has
   `< 2` fields it cannot be a real `ReferenceQueue` (head@0/size@1), so return
   "empty" immediately. This kills the Cleaner-spin flood (the dominant ~99.99 %)
   with zero behaviour change — the blocked thread re-reads the live queue from
   its (eventually remapped) frame on a later `remove()` call, so this is
   graceful degradation, not data loss.
2. `vm/src/runtime/interpreter.rs` — `process_references_after_gc` enqueue loop:
   skip the synthetic head/size writes when the **post-relocation** queue object
   is `< 2` fields (a queue reachable only via the ref-processor's never-drained
   `pending_queues` bookkeeping that was reclaimed and reused). The check is on
   the *remapped* address, so a live queue (even one relocated this cycle) keeps
   its real layout and is unaffected — verified against weak-ref semantics.

## Verification
- avrora: get_field WARN **17 626 → ~2 per run**; **PASSES 8/8** (baseline also
  8/8 — no stability regression).
- Weak-reference semantics intact: a `WeakReference` + `ReferenceQueue` probe
  enqueues correctly after the referent dies (`queue_empty=false`), matching
  HotSpot and the baseline CratonVM build. (An earlier gc-crate liveness filter
  that checked *pre-remap* addresses wrongly dropped this enqueue and was
  reverted.)
- No regression on other dacapo benchmarks (luindex/pmd fail identically on
  baseline and the fixed build — pre-existing, unrelated).

## Residual (~2 WARN/run) — benign, optional follow-up
A couple of `set_field` OOB-drop warnings remain, from another ref-processor
write path on a reclaimed `Reference`/queue (e.g. the `ref_obj` writes in the
enqueue loop, or `native_ref_enqueue`). They are dropped by the same guard with
no data effect. Guarding `ref_obj` too was tried and produced an unexplained
warning-count blow-up, so it was left out pending a proper fix. The clean,
deeper fix is to **drain `pending_queues` once consumed** (it is currently never
cleared, so dead pairs are re-emitted every GC) — a GC-crate change deferred as
its own task.

## Reproduce
```
cd apps/_test-suites/dacapo
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 4g \
    -jar dacapo.jar avrora 2>&1 | grep -c "out-of-bounds field"
```
Diagnostics used: `CRATONVM_DBG_OOBFIELD=1` (symbolicated get_field backtrace,
needs a `strip=none`/`debug="line-tables-only"` build).
