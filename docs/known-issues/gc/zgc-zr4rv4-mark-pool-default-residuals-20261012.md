> **STATUS (2026-10-19, zt t6/ob4): item 3 DECIDED by local HotSpot 25.0.3 rows (`-XX:+UseZGC -Xmx256m -Xlog:gc+init -version`), PENDING BUILD of its pin test; carried zs4rv3 item 19 decided too.** HotSpot runs the count it is given and prints the flag value: `-XX:ParallelGCThreads=100` -> `Runtime Workers: 100`; `-XX:ConcGCThreads=100` -> `GC Workers Max: 100 (dynamic)`; `-XX:ConcGCThreads=0x1k` -> `GC Workers Max: 1024 (dynamic)` (accepted); `=0x0x10` and `=0x+4` -> `Improperly specified VM option` (this VM refuses both too); `=0x0` -> `Error occurred during initialization of VM` (refused); `-XX:ConcGCThreads=1g` -> a fatal error report while creating the threads; `-XX:ParallelGCThreads=1g` hung the box creating threads (killed). So the Java-visible figures are the flag values, which this heap already stores unclamped (`set_gc_worker_threads`, what `-Xlog:gc+init` and JFR report); GC workers are not Java threads, so the 64-worker pool cap is not Java-visible and stays, silent (a stderr line would be output HotSpot does not print; a `1g` count must not crash this VM). Documented on `Z_PARMARK_MAX_WORKERS`; pinned by `zt6ob4_gc_thread_counts_are_stored_unclamped_and_pools_capped` (gc/src/zgc.rs), c809ef6f4e. Item 4 was FIXED earlier: nothing open on this page.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (item 3, thread-count parity).** Items 1, 2, 4 FIXED. Open: `-XX:ParallelGCThreads` above 64 is clamped silently to `Z_PARMARK_MAX_WORKERS` (gc/src/zgc.rs:24926) while `set_gc_worker_threads` (gc/src/zgc.rs:5030) stores the unclamped value; and (carried from `zgc-zs4rv3-*` item 19) `conc` is not clamped there, so `-XX:ConcGCThreads=1g` is accepted as 1073741824. Close: raise the cap or clamp both counts in `set_gc_worker_threads`, deciding the printed figure and the refusals (`=0x1k`, `=1g`, `=0x0x10`) by local HotSpot 25.0.3 rows.

> **STATUS (2026-10-19, zt t3/tp): item 4 FIXED and built (t1 `59063eb037`; t2/x1 added `debug_assert!(self.locate(addr).is_none())` in `spill`, X1-18). Item 3 (`ParallelGCThreads` above 64 clamped silently) is OPEN and is lane mk's call.**

> **STATUS (2026-10-19, zt t1/tp): item 4 FIXED IN CODE, PENDING BUILD; item 3 OPEN.** `HeapBitmap::contains` (gc/src/heap_bitmap.rs) answers an in-span address by its bit alone. The overflow set only ever receives addresses that `locate` refuses (the `insert` / `spill` / claim `None` arms), so the result is identical by that invariant, and the lock is no longer taken for unmarked in-span probes once one address has spilled. Test `zt1tp_a_spill_does_not_change_in_span_answers`. Item 3 (`ParallelGCThreads` above 64 clamped silently) is a parity decision. HotSpot runs the count it was given. Raising `Z_PARMARK_MAX_WORKERS` (gc/src/zgc.rs:24059) is lane `mk`'s call.**

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- Item 1 is fixed and host-confirmed; its section's "PENDING HOST PROBE" is stale (zr orch-notes z4a: `Zr3MtIdlePool 10 -Xmx4g` CPU 0.35-0.55 s, equal to `PARMARK=0`). Item 2 is fixed in code (`CRATONVM_ZGC_MARK_SPAWN_BACKOFF`) but not host-measured. Items 3 and 4 are open and low: the clamp `p.min(Z_PARMARK_MAX_WORKERS)` is silent, and `HeapBitmap::contains` still takes the overflow lock (gc/src/heap_bitmap.rs:615).**

> **STATUS (2026-10-14, zs s2/rv): re-reviewed; items 3 and 4 still OPEN, low.** Item 3 is a parity decision: HotSpot runs the requested worker count, and a log line would add stderr output that HotSpot does not print. Item 4 has no observed engagement. Closing it needs a counter on `HeapBitmap::contains`'s overflow lock, which is not worth a default-path change without a row.

# Parallel mark by default (zr z3/mt): pool residuals found in review

> **STATUS (2026-10-13, zr z11/rv9): item 1 FIXED IN CODE (z3/mt follow-up); item 2 FIXED IN
> CODE, PENDING HOST PROBE (`CRATONVM_ZGC_MARK_SPAWN_BACKOFF`, default on, `=0` kill: after a cycle
> on which no pool could be spawned, the next 16 parallel-mark cycles mark serially; test
> `a_failed_mark_spawn_backs_off_for_a_bounded_number_of_cycles`); items 3 and 4 OPEN, low.** See
> `docs/internal/zgc-round-zr-20261012/z11-rv9-report.md`.
>
> **STATUS (2026-10-13, zr z4/rv4): OPEN.** The serious finding of this review, the failed-mark
> fallback, is fixed: see `zgc-zr4rv4-failed-parallel-mark-fallback-skipped-claimed-objects-FIXED-20261012.md`.
> The items below are performance and parity defects. In the review, `REF_DISPS`,
> `LAYOUT_MEMO` and `TEST_FIRST` were found to skip no reference:
> - `ref_disps` is built in the same loop as `is_ref`.
> - The memo lives for one stop-the-world `mark_closure_serial` call, keyed like `with_class_layout`.
> - Arrays, `Reference` holders, non-compact objects and `DBG_ZGC_WILD` fall back to the shared walk.
> - `TEST_FIRST` is a read-only pre-test in front of the same `claim`.

## 1. Idle persistent mark workers wake every 5 ms (OPEN, default-path cost)

> **STATUS (2026-10-13, zr z3/mt follow-up): FIXED IN CODE, PENDING HOST PROBE.**
> `ZMarkTerminator::wait_for_cycle` parks with an untimed `Condvar::wait` between cycles
> (`CRATONVM_ZGC_MARK_IDLE_PARK_UNTIMED`, default ON, `=0` restores the 5 ms poll). No
> notification can be lost: `arm` changes `cycle_generation` under the lock and notifies;
> `request_stop` stores `should_stop`, then `wake_all` takes the lock before notifying.
> Waits inside a cycle keep their bounded poll. New counter `idle_wakeups()`; unit test
> `zr3mt_idle_mark_workers_do_not_wake_between_cycles` (0 wake-ups over idle windows, cycles
> still join, shutdown joins; the `=0` arm is the positive control);
> `a_heap_that_kept_a_mark_pool_still_drops` and `zr2cc_no_worker_holds_the_cycle_context_after_end_cycle`
> still pass. Host row: probe `Zr3MtIdlePool 10 -Xmx4g`, CPU user+sys, default vs
> `CRATONVM_ZGC_PARMARK=0` vs `CRATONVM_ZGC_MARK_IDLE_PARK_UNTIMED=0` (see
> `docs/internal/zgc-round-zr-20261012/z3-mt-report.md`, section "Follow-up").

`PARMARK=auto` and the persistent pool are both on by default. Every ZGC VM therefore
parks N workers between collections in `wait_for_cycle`. Each worker uses
`wake.wait_for(.., Z_MARK_PARK_POLL_MS = 5)` (`gc/src/zgc/mark.rs`) and takes the shared
lock on every wake-up: about 200 wake-ups per second per worker for the life of the
process, even when it is idle. With 5 workers that is about 1,000 per second per VM.

To close: between cycles, wait without a timeout (or with a long one), since
`begin_cycle` and `request_stop` already notify. The deciding row is idle-process CPU
(`/usr/bin/time` on a `Thread.sleep(30000)` probe with `-Xmx4g`), auto against `PARMARK=0`.

## 2. Spawn failure retries every cycle (OPEN, low)

When `persistent_mark_pool` cannot spawn its pool, every later collection tries the
persistent spawn again, then the per-cycle `try_new`, then serial. Each attempt spawns and
joins up to N-1 threads inside the pause and logs a `warn!`.

To close: add a per-heap "spawn failed at N" latch with backoff.

## 3. `-XX:ParallelGCThreads` above 64 is clamped silently (OPEN, parity)

`set_gc_worker_threads` stores the unclamped value, so `Runtime Workers: 100` is printed,
but the pools run `Z_PARMARK_MAX_WORKERS` (64). `ConcGCThreads` is clamped to 1..=64 the
same way.

To close: log the clamp once and document the cap, or raise it.

## 4. `TEST_FIRST` takes the overflow mutex on a non-empty bitmap overflow (OPEN, suspected perf)

`HeapBitmap::contains` locks `overflow` when `overflow_len != 0`, which happens before the
registry screen. This is probably never hit, because registered bases are in-span and
8-aligned. To close: add a counter or a debug assertion that the mark bitmap's overflow
stays empty.
