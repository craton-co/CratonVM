# Proposal: the remaining HotSpot Serial `-Xlog:gc*` lines (exit heap summary, metaspace, full-collection phases, tenuring distribution)

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 17
> of 54).** Not built (items 1-4 as scoped by obs8). The lines that exist
> match: d7 xlog_bare prints Serial's `Using Serial`, `gc,init`, `gc,start`,
> `gc,heap`, `gc` and `gc,cpu` lines (its DIFF is the timestamps and sizes).
> **Gate:** a GCViewer / GCeasy parse of the output; `gc,heap,exit` printed
> once at a normal exit; `grep -c 'Metaspace:'` equals the `GC(n)` count; item
> 2 only after its pause cost is measured. **Size:** S (item 1), S (item 3), M
> (items 2 and 4).

> **STATUS (2026-09-26, gen r5w4/obs8): NOT IMPLEMENTED; scoped.** obs8
> took the two observability defects first and did not start this. What it
> found about each item, for whoever does:
> - **1 (`gc,heap,exit`)** needs the exact JDK 25 Serial text (the
>   `def new generation` / `eden space` / `from space` / `to space` /
>   `tenured generation` / `the space` / `Metaspace` lines, with `[base,
>   top, end)` addresses) captured from `java -XX:+UseSerialGC -Xlog:gc+heap+exit`
>   before writing it: nobody has recorded it in-tree, and the addresses mean
>   a GCeasy parser, not a byte diff, is the right oracle. Hook:
>   `VmHeap::print_gc_summary`'s shutdown call site; the tag set
>   (`LogTag::GcHeapExit`) already parses.
> - **2 (`gc,metaspace`)**: the only metadata figure is
>   `VmDiagnosticState::heap_summary(..).metaspace_used`, a class-store walk
>   under the class-manager read lock; taking it at both edges of every
>   collection is a pause cost to measure first, and there is no
>   `NonClass` / `Class` split to print.
> - **3 (`gc,phases`)**: the phase marks
>   (`gen_heap::take_last_pause_phases`) are stashed only while a JFR
>   recording runs (`jfr_phases` in `collect_garbage_inner`) and only for the
>   young cycle; a full collection's four Serial phases need marks in
>   `major_gc` / the old-gen sweep and the stash armed under
>   `-Xlog:gc+phases` too (both in `gc/src/gen_heap.rs`, outside the
>   observability lane).
> - **4 (`gc,age`)**: unchanged (needs the young cycle's per-age census).

*Filed 2026-09-26 by generational GC round 5, wave 3, lane `obs7`. A
direction, not a defect.*

## Where things stand

Since gen r5w3/obs7 the Generational backend prints HotSpot Serial's
`Using Serial`, a truthful subset of the `gc,init` block, and per collection
`gc,start`, both `gc,heap` lines, the `gc` line and `gc,cpu`, with HotSpot's
decorators and padding
(`../../internal/gc/gengc-r5w2-obs6-proposal-xlog-gc-hotspot-compatible-output-DONE-20260928.md`).
Four `gc*` families HotSpot prints are still absent:

1. **`gc,heap,exit`** — the heap summary at VM exit (`Heap`, `def new
   generation total …K, used …K [0x…, 0x…, 0x…)`, `eden space …`,
   `from space …`, `to space …`, `tenured generation total …`,
   `the space …`, `Metaspace used …K, committed …K, reserved …K`). GCeasy
   and GCViewer use it for the final footprint. Everything but the addresses
   is in `jmx_memory_pools` + `usable_committed_parts`; the addresses are
   the arenas' bases (`region_bounds`). Needs one call at VM shutdown (the
   `--verbose:gc` exit census already has the hook).
2. **`gc,metaspace`** — `GC(n) Metaspace: 1117K(1280K)->1117K(1280K)
   NonClass: … Class: …` per collection. Needs the class-metadata footprint
   this VM reports as the `Metaspace` / `Compressed Class Space` non-heap
   pools (`jmx.rs::non_heap_memory_usage`), sampled at both edges.
3. **`gc,phases` for a full collection** — Serial's `Phase 1: Mark live
   objects`, `Phase 2: Compute new object addresses`, `Phase 3: Adjust
   pointers`, `Phase 4: Move objects` with `…ms`. The mark-compact major GC
   has the same four phases; `take_last_pause_phases` already carries
   named marks for JFR, so a name mapping plus a `gc,phases` emitter is
   enough (young-cycle phases should stay unprinted: they are not Serial's).
4. **`gc,age`** — `Desired survivor size … bytes, new threshold 2 (max
   threshold 15)` and the `- age   1:  …  bytes, … total` table
   (`-XX:+PrintTenuringDistribution`'s successor). The adaptive tenuring
   state (`TenuringState`, gen r4w6/young6) keeps HotSpot's unit already;
   the age table needs a per-age byte census in the young cycle (evac7's
   code) — only the printing half is observability.

## How to judge it

The GCViewer / GCeasy corpus check of the parent proposal, plus: a run
ending normally prints the `gc,heap,exit` block once, and `grep -c
'Metaspace:'` equals the number of `GC(n)` lines.
