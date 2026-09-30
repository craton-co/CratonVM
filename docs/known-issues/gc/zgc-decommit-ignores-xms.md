# ZGC gives free memory back at every cycle, below `-Xms` and with no delay

> **STATUS 2026-09-26: OPEN. Owner: the ZGC collector round.** Filed
> 2026-09-23 by the GC common-infrastructure round (wave 2, lane E, as
> `common-w2e-zgc-decommit-ignores-xms.md`); renamed to the ZGC collector's
> naming in wave 5. Re-verified on `b5c9b6c6e`:
>
> - `ZgcRealHeap::collect_garbage` (`gc/src/zgc.rs` ~:17406) still runs
>   `arena.decommit_unbumped_middle() + arena.decommit_free_blocks()` in every
>   cycle's prologue (~:17629, after `retire_forwarding_words`), with no floor,
>   no delay and no flag.
> - `ZgcRealHeap::with_capacity_and_initial` (~:2440) still commits the
>   `-Xms` prefix (`arena.commit_initial_prefix`) and stores nothing a later
>   decommit could use as a floor.
> - The other two backends now both floor at `-Xms`: G1's trailing-free-run
>   shrink (`shrink_floor_bytes`) and, since 2026-09-24, Generational's
>   old-gen shrink (`OldGen::commit_floor`, `CRATONVM_GC_OLD_SHRINK`, default
>   ON). ZGC is the one backend that gives memory back below `-Xms`.
> - Measured once, by the orchestrator's w2 battery: `XmsProbe -Xms64m
>   -Xmx512m -XX:+UseZGC` printed `total=66m` (HotSpot 64m), so that probe
>   does not show the below-`-Xms` report (its live set and churn keep the
>   arena above the floor). The re-commit cost after every cycle is what the
>   code does regardless.
>
> The fix is a field in the collector plus a floor inside the arena's decommit
> walk (which granules may go), not a different call. The reporting side needs
> no change when it lands: `VmHeap::committed_bytes`' ZGC arm reads
> `ZgcRealHeap::os_committed_bytes`. The retire criteria below stand.

## Evidence

* `gc/src/zgc.rs` (`collect_garbage`'s cycle prologue):
  `arena.decommit_unbumped_middle() + arena.decommit_free_blocks()` runs at
  the start of EVERY collection, unconditionally — no `-Xms` floor, no delay,
  no flag (the Generational twin `uncommit_evacuated_young` is behind
  `gc_flags().gen_uncommit`, G1's behind `CRATONVM_G1_UNCOMMIT` and floored at
  `-Xms` by `shrink_floor_bytes`).
* `ZgcRealHeap::with_capacity_and_initial` commits the `-Xms` prefix at
  construction (`arena.commit_initial_prefix`) but stores no floor, so nothing
  downstream can honour it.
* HotSpot's ZGC uncommits only memory unused for `ZUncommitDelay` (300 s) and
  never below `MinHeapSize`, which `-Xms` sets.

## Failure scenario

Since 2026-09-23 `Runtime.totalMemory()` on ZGC is the arena's committed
granules (`VmHeap::committed_bytes`). A `-Xms64m -Xmx512m` process with a
2 MiB live set is expected to report `total=64m` until its first collection and a few MiB
after it (HotSpot: 64m throughout), so `freeMemory()` is near zero right after
a full GC. Independently of the report: an operator who set `-Xms` to avoid
paying commit faults while the heap regrows pays them after every cycle, and a
steady-state allocator re-commits (and re-zeroes, on Windows `MEM_COMMIT`) the
same granules every cycle.

## Proposed fix

Store the startup commit as a floor (`initial_commit_bytes`, as G1 does) and
make the cycle-prologue decommit skip granules below it; optionally put the
whole give-back behind a delay (HotSpot's 300 s) or a flag. The one part that
must not move is the ORDER the site documents (retire forwarding words, then
decommit, then retract the JIT inline-load bound): only the amount changes.

## Confirmation

```
rg -n "decommit_unbumped_middle\(\) \+ arena.decommit_free_blocks\(\)" gc/src/zgc.rs
```

Probe: `tools/probes/XmsProbe.java` with `-XX:+UseZGC -Xms64m -Xmx512m`. It
was expected to print a `total` below 64m; the one measurement (w2 battery)
printed `total=66m`, because the probe's live set and churn keep the arena
above the floor. Showing the defect needs a smaller live set after a
collection that freed most of the heap.

## What would retire it

`XmsProbe` on ZGC printing `total=64m`, and a `zgc.rs` unit test: a heap built
`with_capacity_and_initial(big, 16 MiB)`, one collection with nothing live,
`os_committed_bytes() >= 16 MiB`.
