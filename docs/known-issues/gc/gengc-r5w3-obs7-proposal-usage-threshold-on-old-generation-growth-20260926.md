# Proposal: check the `Tenured Gen` usage threshold when the old generation grows between collections, and deliver without waiting for one

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 16
> of 54).** Not built (no `pool_thresholds` read in `gen_heap.rs` /
> `old_gen.rs`). The collection-time thresholds hold on d7 (w3_poolthresh
> SAME). **Gate:** the page's probe (threshold at used + 8 MiB, one 16 MiB
> pretenured array, no `System.gc()`) prints HotSpot Serial's
> `threshold-exceeded Tenured Gen` within 1 s; the default path costs one load
> per old refill. **Size:** S.

> **STATUS (2026-09-26, gen r5w4/obs8): NOT IMPLEMENTED; prerequisite fixed.**
> The thresholds themselves could not be exercised until obs8 fixed the pool
> beans' construction (`gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds`:
> on `897210f83` every pool reported `false/false`), so nothing here had a
> baseline yet. The detection half belongs in the old generation's
> allocation/growth path in `gc/src/gen_heap.rs` (the humongous and promotion
> allocators and `OldGen` growth), outside the observability lane: one
> `GcNotificationQueue::pool_thresholds().is_armed()` load there, and on a
> crossing a `PoolThresholds::check` with a tenured-only
> `JmxPoolUsage` sample (`GenerationalHeap::jmx_memory_pools` takes three
> locks; a tenured-only sampler would take one). The delivery half needs a
> wake of the reference-delivery thread (`gc_notifications_pending` is
> already its predicate) or a drain at the next safepoint poll; a drain
> from inside `setUsageThreshold0` itself is wrong, because the Java caller
> holds the pool's monitor and has not yet stored the new threshold.

*Filed 2026-09-26 by generational GC round 5, wave 3, lane `obs7`. A
direction, not a defect.*

## Where things stand

Since gen r5w3/obs7 the Generational pools support HotSpot Serial's
thresholds (`../../internal/gc/gengc-r5w2-obs6-heap-pools-support-no-usage-thresholds-FIXED-20260928.md`).
Detection runs at the end of every collection and when a usage threshold is
set; delivery (`Sensor.trigger` → `MemoryImpl.createNotification`) runs at
the next GC-notification drain point. HotSpot differs in two ways:

1. `MemAllocator::Allocation::notify_allocation_low_memory_detector` checks
   the usage threshold of every collected pool after each slow-path
   allocation (`LowMemoryDetector::detect_low_memory_for_collected_pools`),
   so a large array allocated straight into the tenured generation crosses
   the threshold immediately, not at the next collection.
2. The Service Thread delivers pending sensor requests within milliseconds
   of detection, whether or not a collection follows. Here a crossing
   detected when the threshold is SET waits for the next collection's drain
   (or the reference-delivery thread's next batch under
   `CRATONVM_FINALIZER_THREAD=1`).

A memory-warning listener that sheds caches on `MEMORY_THRESHOLD_EXCEEDED`
therefore reacts one collection late on this VM.

## Proposal

- Detection: `OldGen` already signals growth to the concurrent service
  (`conc_service` / trigger stats). Add one relaxed load there —
  `PoolThresholds::is_armed()` — and, when armed and the old generation's
  used crossed the `Tenured Gen` usage threshold since the last check, run
  `PoolThresholds::check` with a cheap tenured-only sample. The default
  path stays one load per old-generation refill.
- Delivery: when a request is pending and no collection is imminent, wake
  the reference-delivery thread (the `gc_notifications_pending` path it
  already has) instead of waiting for a collection; without the delivery
  thread, the next safepoint poll's drain point.

## How to judge it

A probe that sets `Tenured Gen.setUsageThreshold(used + 8 MiB)`, then
allocates one 16 MiB array (pretenured) and waits 1 s for the notification
WITHOUT calling `System.gc()`: HotSpot Serial prints
`threshold-exceeded Tenured Gen`; CratonVM today prints nothing until a
collection happens.
