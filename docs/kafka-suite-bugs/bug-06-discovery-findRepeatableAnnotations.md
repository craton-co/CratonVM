# Bug 06 — `consumer.internals`: discovery recursion crash → now throughput + thread-lifecycle

**Original symptom (sweep 2026-06-12, before dev merges):** `consumer.internals`
failed discovery with a `JUnitException` out of recursive
`AnnotationUtils.findRepeatableAnnotations` (`@Tag` resolution) — a hard crash.

**Current status (after syncing dev + the bug-02/03/04 fixes): the crash is gone.**
`consumer.internals` no longer throws in discovery. What remains is **not a discrete
correctness bug** — it is a mix of throughput + thread-lifecycle:

1. **Throughput.** `CooperativeConsumerCoordinatorTest` (the class that "hangs")
   *completes* with the watchdog disabled (HotSpot runs 152 tests in ~13s; CratonVM
   takes >120s and trips the 120s default watchdog). Same family as bug-01 / bug-05.

2. **Thread lifecycle (the watchdog trip).** After `RunCls.main` returns, CratonVM
   reports *"VM held alive by 6 non-daemon thread(s)"* — the consumer-coordinator
   background threads are treated as **non-daemon**, so the JVM never exits and the
   watchdog kills it (rc=127). HotSpot exits cleanly in ~13s. Points at a thread
   daemon-status / lifecycle gap, not the annotation path.

3. **Incomplete discovery.** CratonVM runs fewer tests than HotSpot (e.g. 22–58 vs
   152) and the count varies run-to-run.

### Annotation equals/hashCode is **not** the cause
Probes confirmed CratonVM's annotation-proxy `equals`/`hashCode`/HashSet-dedup work
correctly for single-value, array-valued, and primitive-array members, and for the
`@Tag`/`@Retention` meta-annotation shapes — so the `findAnnotation` `visited` set
dedups correctly; there is no infinite recursion.

## Fix applied here (general perf, not a bug-06 cure)
While diagnosing, the slow path was found to flood the heap `get_field`
out-of-bounds guard: a benign **case-(B)** caller-side speculative collection-layout
probe lands on `Collections$EmptyMap` (reads slot 2 of a 2-slot map) thousands of
times per discovery, and the guard ran `resolve_class_info` (a `String` allocation)
**plus** an unthrottled `warn!` on **every** call — pure unconditional overhead.

**Fix** (`gc/src/gen_heap.rs`): rate-limit the OOB-read diagnostics to the first 512
occurrences globally (a persistent case-(A) *true* undersized-layout bug surfaces
well within that; `CRATONVM_DBG_OOBFIELD` forces full diagnostics). Past the cap the
OOB read still returns a benign null — just without the per-call `String` alloc +
`warn!`. General improvement for any workload that hits speculative layout probes.

## Status
- [x] Original discovery-recursion crash: resolved (dev merges).
- [x] General OOB-guard diagnostic overhead: rate-limited (this branch).
- [ ] Throughput (interpreter speed) + non-daemon-thread-lifecycle: open — the
      JIT/throughput + thread-lifecycle workstreams, not a discrete crash.
