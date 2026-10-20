# Non-heap pools on ZGC after the merge decision: what still differs from HotSpot 25

> **STATUS (2026-10-19, zt t6/ob4): item 3 FIXED IN CODE, PENDING BUILD; the carried zs4rv3 item 13 test FIXED IN CODE; nothing else open here.** Item 3: a ZGC cycle record carries the three `CodeHeap '...'` entries sampled at its two edges (`BackendGcBeans::note_code_heap_before` / `append_code_heap_usage`, called from `gc_events.rs` beside the `Metaspace` edge samples; pause records keep their zeros), so `jmx.rs::gc_info_non_heap_wanted` no longer samples them at delivery (`CRATONVM_ZGC_GCINFO_CODE_HEAP_EDGES`, default ON, `=0` kill; G1 never). Test `zt6ob4_cycle_records_carry_code_heap_edge_samples` (gc_metrics.rs; kill and zs s3/ob2-list controls). Carried item 13: `retain_collector_beans` (the `GarbageCollectorMXBean` filter, out of line) and the test `zt6ob4_plain_managers_lead_and_the_collector_filter_drops_them` (jmx.rs: plain managers first, each answers its pools, the filter keeps the collectors only; empty-filter and plain-name controls). Item 2 stays structural (GC.md 'Non-heap pools'), item 5 is the proposal. Row: `Zs2ObBeanSurface`-style probe comparing a code heap's `GcInfo` before/after across a collection after a fresh compile (HotSpot twin; `=0` arm: before == after). Commits 123c6df354.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (item 3, plus a carried test).** 1, 4 FIXED (row OB7 SAME); 5 is the proposal `zgc-zt1ob-proposal-incremental-metaspace-figures-20261019.md`; 2 is structural (one unsegmented code arena reported in `CodeHeap 'non-profiled nmethods'`): GC.md paragraph 'Non-heap pools'. Open: 3, sample the code heaps at both edges into the cycle record (vm/src/runtime/interpreter/gc_events.rs edge code; `gc_info_pool_usage`, native-builtins/src/jmx.rs:5842). Carried from `zgc-zs4rv3-*` item 13: a mock test of the plain `MemoryManagerImpl` lists (`alloc_plain_memory_manager`, `is_gc_bean`, the platform `GarbageCollectorMXBean` filter).

> **STATUS (2026-10-19, zt t3/ob): items 1, 4 and 5 FIXED IN CODE, PENDING BUILD / PROBE; items 2 and 3 OPEN.**
> - Item 1: on ZGC each code heap reports HotSpot's `init` (2555904), so the non-heap `init` is 7667712 (`CRATONVM_ZGC_CODE_HEAP_INIT_HOTSPOT`, default ON, `=0` kill; G1 / Generational keep 2 MiB, by the round's scope). Row: t3-ob report OB7 (`Zt3ObNonHeapInit`).
> - Item 4: a back-filled first-cycle record carries its prefix's committed and class space (`vm_init::class_metadata_prefix_figures`, `BackendGcBeans::backfill_metaspace_figures`).
> - Item 5 is the census memo lag. x1 item 7 is fixed (the figures are now keyed by census), but the lag on an in-place redefinition stays, by design of the memo: the pool bean is exact, and the proposal `zgc-zt1ob-proposal-incremental-metaspace-figures-20261019.md` is the lever.
> - Item 2 (the code heaps are one figure split three ways) is truthful and has no close short of a segmented code cache: proposed to record as intended.
> - Item 3 (the code-heap `GcInfo` entries are sampled at delivery) needs the edge code in lane `jf`'s `gc_events.rs`.

> **STATUS (2026-10-19, zt t1/ob): OPEN.** Filed with the merge decision (`CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS`, `zgc-zs2ob-non-heap-memory-pools-are-not-modelled-FIXED-20261019.md`). ZGC now lists HotSpot's five non-heap pools with gci i15/nh / i33/ms's figures. These are the differences that remain; items 1-2 are dev's every-backend model, so changing them changes G1 and Generational output too and needs a decision outside the ZGC round.

## 1. The non-heap `init` is 2 MiB, HotSpot's is the code heaps' sum

`MemoryMXBean.getNonHeapMemoryUsage().getInit()` is the sum of the pools' `init`
(`jmx.rs` `non_heap_usage_sum`). Here only `CodeHeap 'non-profiled nmethods'` has one
(`NON_HEAP_INIT_BYTES`, 2 MiB, kept because Spring Boot's
`ProcessInfoTests.memoryInfoIsAvailable` asserts a positive `init`); the other two code heaps
read 0. HotSpot 25.0.3 locally (zs s3/ob2 page item 6): `7667712`, three code heaps of
`2555904` each (`InitialCodeCacheSize`, 2496K on x86_64); `Metaspace` and
`Compressed Class Space` contribute 0.

**Close**: give each code heap HotSpot's initial size as its `init` (then the sum is
HotSpot's byte for byte on x86_64), after checking on the host that the figure is the same
with other CPU counts and `-XX:ReservedCodeCacheSize` values. Every backend's answer
changes, so it needs the G1 / Generational owners' agreement. Row:
`cratonvm -XX:+UseZGC -cp tools/probes Zs2ObBeanSurface` stderr `nonheap init=` against the
HotSpot twin, on two CPU counts.

## 2. The code heaps are one figure split three ways

The JIT keeps one unsegmented arena, so `CodeHeap 'non-profiled nmethods'` carries all of it
(method bodies, OSR trampolines, deopt stubs) and the other two read 0 bytes with `max`
undefined. HotSpot's `non-nmethods` heap holds the interpreter and stubs (a few MiB used), its
`profiled nmethods` heap the C1 tier. Truthful as it stands (this VM's interpreter is Rust code
in the binary); no close short of a segmented code cache.

## 3. A `GcInfo`'s code-heap entries are sampled at delivery, not at the edges

HotSpot samples every pool at `gc_begin` / `gc_end`. Here a cycle record carries the heap
pools and (ZGC) `Metaspace` / `Compressed Class Space` from its edges; the three code heaps
are rendered from their usage when the `GcInfo` is built (`jmx.rs` `gc_info_pool_usage`).
The before and after entries are therefore equal, and can include code installed after the
collection. A program would need to compare a code heap's before / after across a collection
to notice. **Close**: sample the two relaxed JIT loads at both edges into the record (cheap;
lane `jf` owns the edge code in `gc_events.rs`).

## 4. A back-filled first-cycle `Metaspace` key has `committed = used`

`BackendGcBeans::backfill_metaspace` (zs s6/ob5) gets a prefix's used figure from
`vm_init::class_metadata_prefix_bytes`, which sums used bytes only, so such a record's
`Metaspace` entry has `committed = used` and no `Compressed Class Space` entry (that one is
rendered at delivery). **Close**: sum a running `MetaspaceArenaTally` in the prefix walk and
take `hs_stats_from_sums` at each cut (the same arithmetic `metaspace_stats` does), then pass
`MetaspaceSpaceFigures` to the back-fill.

## 5. The `GcInfo` edge memo lags an in-place redefinition

The ZGC edges reuse a walk while the loaded / unloaded counts are unchanged
(`BackendGcBeans::metaspace_sample_figures`), so a JVMTI `RedefineClasses` between two
collections is seen at the next define or unload. The pool bean walks per query and is
exact. See the proposal `zgc-zt1ob-proposal-incremental-metaspace-figures-20261019.md`.
