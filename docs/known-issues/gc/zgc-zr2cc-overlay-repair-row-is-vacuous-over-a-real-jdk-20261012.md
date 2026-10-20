> **STATUS (2026-10-19, zt t5/pg3): OPEN (a row, opt-in concurrent mode).** Nothing to fix in code; what remains is the coverage verdict: run O1 / O2 locally (`CRATONVM_COMPAT_ALIGN=-jdk-linked-collections,-jdk-tree-map CRATONVM_ZGC_CONC_START=60 cratonvm --compatible -Xmx512m --verbose:gc Zr1MkConcOverlayMove 2 20000 30`, then with `CRATONVM_ZGC_CONC_OVERLAY_REPAIR=0`). If O1 reads `overlay_repair=0/0` too, re-scope the repair to the synthetic JDK in its doc (`overlay_rows_written_during_mark`, gc/src/zgc.rs) and retire.

> **STATUS (2026-10-19, zt t3/mk): OPEN, unchanged (probe-coverage gap; needs a real-JDK workload that writes an overlay row during an open phase).**

> **STATUS (2026-10-19, zt t1/mk): OPEN, probe-coverage gap; no code change.** Nothing in this lane's code changed the overlay repair (`overlay_rows_written_during_mark`); the late-root harvest's overlay ADDITIONS are now screened to this heap's arena (`ZLateRoots::screened_foreign`), which can only lower `late_owners=`, never `overlay_repair=`. The row still needs a workload that writes a native-collection overlay row during an open phase on a real JDK; until one exists, the honest close is a census that re-scopes the repair to the synthetic JDK.

# The concurrent overlay-repair row is vacuous over a real JDK: `--compatible` no longer keeps `LinkedList` / `TreeMap` contents in overlays

> **STATUS 2026-10-15 (zs s5/pg): OPEN -- This is still a probe-coverage gap, and no zs lane touched it. The code is as described: `overlay_rows_written_during_mark` (gc/src/zgc.rs:5823), `CRATONVM_ZGC_CONC_OVERLAY_REPAIR`, and the `jdk-tree-map` / `jdk-linked-collections` alignment tokens. No host run has recorded a non-zero `overlay_repair=`.**

> **STATUS (2026-10-12, zr z2/cc): OPEN (a probe-coverage gap, not a collector defect); deciding rows given.** Found by reading the host's `Zr1MkConcOverlayMove` result (10/10 `ok`, `overlay_repair=0/0` on all 1139 `[GC] zgc-markend:` lines).

## What the row assumed

`Zr1MkConcOverlayMove` (zr z1/mk) moves elements between two `java.util.LinkedList`s and two
`java.util.TreeMap`s on `--compatible`, on the premise that "`--compatible` keeps collection
contents in native overlays". The concurrent overlay repair
(`ZgcRealHeap::overlay_rows_written_during_mark`, `CRATONVM_ZGC_CONC_OVERLAY_REPAIR`) re-reads
the overlay rows written during the concurrent phase; `overlay_repair=<rows>/<refs>` is its
engagement count.

## Why it read 0/0 everywhere

Over a real JDK, `--compatible` now runs both families' JDK bodies, as `--jdk-only` does:
`HotSpotAlignment::JdkLinkedCollections` (`types/src/hotspot_alignment.rs`, "`java.util.LinkedList`
and `java.util.ArrayDeque` ... run the JDK's own bodies") and `HotSpotAlignment::JdkTreeMap`
(same file). Their state lives in real fields, the SATB barrier sees every store, and no overlay
row is written -- so the repair had nothing to repair and the row tested the ordinary SATB path
only. The SATB hole the repair closes is still reachable for any collection that IS
overlay-backed in the run, and for both families when the alignment rows are turned off.

## Deciding rows (orchestrator)

| row | command | pass |
|---|---|---|
| O1 positive control | `CRATONVM_COMPAT_ALIGN=-jdk-linked-collections,-jdk-tree-map CRATONVM_ZGC_CONC_START=60 cratonvm --compatible -Xmx512m --verbose:gc Zr1MkConcOverlayMove 2 20000 30`, 10 runs | `ZR1MK-CONC-OVERLAY movers=2 n=20000 lost=0 corrupt=0 ok` 10/10 and `overlay_repair=` NON-zero on some `[GC] zgc-markend:` line, `mark=concurrent` cycles present |
| O2 kill | O1 + `CRATONVM_ZGC_CONC_OVERLAY_REPAIR=0` | `overlay_repair=0/0` everywhere; a `BAD` confirms the hole, an `ok` says the race was not hit |
| O3 alignment control | O1 without `CRATONVM_COMPAT_ALIGN` (the z1 row) | `overlay_repair=0/0` (records that the default `--compatible` no longer reaches overlays for these families) |
| HotSpot | `java -XX:+UseZGC -Xmx512m Zr1MkConcOverlayMove 2 20000 30` | the `ok` line |

If O1 also reads `overlay_repair=0/0`, the alignment tokens did not restore the native bodies on
that image (check `[GC] zgc-markend:` `overlay_repair` and the native kind map) and the repair is
covered by its unit test only.

## What would close it

O1 green with a non-zero `overlay_repair`, or a census showing which collection families still
write overlay rows on `--compatible` over a real JDK (if none, the repair is reachable only on the
synthetic JDK and with `CRATONVM_COMPAT_ALIGN=none`, and the row should say so).
