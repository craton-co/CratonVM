# G1's `needs_gc` performed a shared atomic RMW on every interpreted allocation

> **STATUS 2026-09-29: PARTLY IMPLEMENTED, NOT RETIRED.** The interpreter now
> polls less often after its TLAB work, and this integration replaces G1's
> remaining shared recount RMW with a thread-local countdown. Acceptance is
> not a performance claim and this page remains open for repeatable evidence.

The interpreter now asks its occupancy triggers after a TLAB refill, an
outside-TLAB allocation, or 16 KiB of thread allocation. Where G1 does receive
the query, it previously began with `needs_gc_since_recount.fetch_add(1,
Ordering::Relaxed)`, making allocating threads contend for one cache line even
though the result only chose which caller performed a once-per-1024-query drift
check.

`needs_gc_recount_due` now owns that cadence in a `thread_local!` `Cell`.
Each query still reads the same `G1Collector::free_region_count`, applies the
same free-fraction and young-target conditions, and occasionally validates the
maintained cache. A missed cache publication therefore remains bounded per
mutator without putting a locked RMW in the common allocation path.

## Findings recorded 2026-09-29

- The implementation uses `NEEDS_GC_RECOUNT_TICK: thread_local! Cell<usize>`;
  `G1Collector::needs_gc` no longer has a shared
  `needs_gc_since_recount.fetch_add` counter. The cache and trigger arithmetic
  were intentionally left unchanged.
- `cargo test -p cratonvm-gc --lib` passed **2400 tests** (4 ignored) in the
  task worktree before the final probe iteration.
- A fresh `--nojit -XX:+UseG1GC -Xmx128m`
  `G1NeedsGcContentionProbe` ran 400,000 total allocations successfully in
  both configurations: one thread reported `wallMs=272`, checksum
  `79999800000`; eight threads reported `wallMs=130`, checksum
  `58221988800`; both reported `PROBE-OK`.

The timings are one host sample, not a performance acceptance claim. The
probe folds the worker ID into its checksum, so equal total work deliberately
does not produce equal checksums across different thread counts. A future
retirement must use repeated, controlled measurements and merge/push evidence;
until then this is an implemented containment, not a completed delivery.
