# Stream/ArrayList under extreme concurrent GC pressure: heap corruption (small-heap only), NOT fixed by the Stream/Comparator ObjectRef pin

Status: OPEN — found by accident 2026-07-14 while verifying dev commit `671c8df3`
("fix(native-collections): pin Stream/Comparator ObjectRefs across GC-triggering calls"). Distinct,
separate residual — not resolved by that fix. Not yet root-caused; no fix attempted yet.

## Repro

24 threads, each looping 3000 times; each iteration:
1. builds a 30-element `ArrayList<P>` (`particles`)
2. `particles.stream().map(lambda-that-allocates).flatMap(lambda-that-allocates-and-returns-Stream.of(3-strings)).collect(Collectors.toUnmodifiableList())`
3. separately, `particles.stream().map(...).filter(...).findFirst()`

Run under CratonVM real-JDK25 mode with `-Xmx32m` (small heap, to force heavy GC pressure) and
`CRATONVM_DBG_STALE_OBJREF=1`.

Java source can be reconstructed from the above description. It may still exist as
`StreamOnlyStressRepro.java` on the Azure Linux build host at `/data/data/` — not confirmed present at
time of writing (host cleanup may have removed it); it was NOT copied into this repo's `docs/known-issues/repros/`
tree, so treat it as lost unless found there.

## Observed

Tested two binaries, both **only under `-Xmx32m`**:

- **Unfixed** (dev `220ebb7d`, before the Stream/Comparator pin fix): hard segfault (exit 139, core
  dump), preceded by GC WARN log lines:
  ```
  GC: inconsistent header — kind=Object but array_length=512 (num_slots=0, class_id=0); inline-alloc forgot to set kind=Array
  GC: skipping suspected false root at <addr> (computed size 0 bytes...)
  ```
  This looks like genuine heap corruption, not a simple stale-ref: `CRATONVM_DBG_STALE_OBJREF` did
  **not** cleanly fire here. Either the corruption has a different root cause entirely, or it is a
  stale-ref case whose forwarding-record window had already been reclaimed by the time of the bad read
  (so the assertion has nothing left to catch).

- **Fixed** (with `671c8df3`'s `stream_apply_chain_full` + `native_stream_find_first` lazy-branch
  pinning applied): no segfault, but occasionally a **wrong result** — the test's own
  `IllegalStateException("bad size 0")` assertion fires because
  `particles.stream().map().flatMap().collect(toUnmodifiableList())` silently returns an **empty
  list** instead of the expected 90 elements (30 × 3).

- **Both binaries pass cleanly with zero errors at `-Xmx512m`**, same thread count/iteration count.
  The failure is heap-pressure/GC-timing dependent, not a deterministic logic bug — and it survives
  671c8df3's fix, so it's a separate residual in the same general area (Stream/ArrayList under
  concurrent allocation + moving GC), not something that fix was ever expected to close.

## Hypothesis / where to look

The "silently short/empty result" shape (elements dropped, not garbage) suggests either:
- another unpinned `ObjectRef` somewhere in the `ArrayList.add`/`stream`/`map`/`flatMap` chain that
  goes stale across an allocation, distinct from the two call sites `671c8df3` already fixed, or
- the backing array being read at the wrong point in a concurrent resize/realloc.

The unfixed binary's corruption warning (`kind=Object but array_length=512`, "inline-alloc forgot to
set kind=Array") points at an **inline-allocation fast path** that writes an array's header fields in
the wrong order/incompletely under GC-triggered preemption — worth checking first, since that shape
is specific enough to search for directly (`array_length` header field set before `kind=Array` is
committed).

## Suggested next steps

1. Try to recover or reconstruct `StreamOnlyStressRepro.java` (check the Azure Linux build host at
   `/data/data/` first; reconstruct from the description above otherwise) and add it under
   `docs/known-issues/repros/` so it isn't lost again.
2. `CRATONVM_DBG_STALE_OBJREF=1` + `RUST_BACKTRACE=1` first — but since the assertion didn't cleanly
   fire in the corrupted case, be ready to fall back to gdb-based live debugging (this repo's
   established gdb-live-wrapper / SpinPoll techniques for GC-adjacent bugs — note that a gdb-attached
   live wrapper can itself suppress the race by changing timing; prefer `core_pattern`+`ulimit -c`
   core-dump capture for the segfault case).
3. Bisect the "wrong size" case first (deterministic-ish once you have `-Xmx32m` reproducing it; the
   segfault case is likely the same root cause hit harder/earlier) — narrow to the specific native call
   site with the same pin/refresh-before-write pattern `671c8df3` used.
