# Problem 3: GC roots in compiled frames are found by heuristics

Filed 2026-11-11 from code analysis only. Revised the same day after
measuring: the first version described the blind conservative scan as the
live mechanism, and on the rows below it never runs.

## What the first version got wrong

It read `vm/src/jit/conservative_roots.rs` by its header and its function
names and concluded that roots are still found by scanning. Measured, the
precise half carries the load. The instruments the first plan asked for also
exist already: `CRATONVM_DBG_JIT_ROOTSCAN` (one line per collection),
`CRATONVM_DBG_VERIFY_OOP_MAPS` (the precise-versus-observed audit,
`oop_map_audit`), `frame_coverage_reason`, `band_path`.

## Measurement

`tools/probes/JitRootCoverageProbe.java`: four worker threads (exceptions
through compiled frames, streams, a monitor, reflection, array copies,
`System.gc()`) beside a main loop entered by on-stack replacement.

```text
CRATONVM_DBG_JIT_ROOTSCAN=1 cratonvm -Xmx256m [collector] -cp <dir> JitRootCoverageProbe
```

Windows x64, the `dev` release binary built 2026-10-27, one run per
collector. Cumulative counters from the last line of each run:

| | default | G1 | ZGC |
|---|---|---|---|
| collections | 36 | 101 | 37 |
| frames with a complete map (`frame_cov ok`) | 429 | 1077 | 496 |
| frames refused, any of four reasons | 0 | 0 | 0 |
| blind fallback scans (`fallback`) | 0 | 0 | 0 |
| unregistered-frame sweeps (`a5_sweeps`) | 0 | 0 | 0 |
| OSR fallbacks | 0 | 0 | 0 |
| bands scanned | 272 | 703 | 330 |
| band roots published movable | 2157 | 5916 | 2401 |
| band roots published unrewritable (pinned) | 75 | 206 | 78 |
| cross-thread coverage accepted / refused | 34 / 0 | 0 / 100 | 34 / 0 |
| collections with coverage incomplete | 0 | 100 (`cross` 97, `xt` 3) | 0 |

One run on a two-week-old binary on one host is a reading, not a verdict.
It needs repeating on current `dev` on Linux before anyone acts on the G1
column.

## Problem, as it stands

1. **Bands are still scanned.** Every collection reads the stack words
   between and inside compiled frames that no map names, and classifies each
   with the claim rules (`band_*`, `*_claim`, `*_residue`, `*_licence`).
   About 3 percent of the band roots on this row end up pinned because no
   rule can prove them rewritable. The map answers "which slots hold
   references"; the bands exist because it does not yet answer "and nothing
   else does".
2. **G1 does not discharge the cross-thread obligation.** With peers in
   compiled code, 100 of 101 G1 collections reported
   `CROSS_THREAD_JIT_PEER` or a take-over reason
   (`gc/src/gc_quiescence.rs`, `incomplete_reason`); the default collector
   and ZGC accepted the handshake every time on the same row.
3. **The retired paths are still in the tree.** `conservative_roots.rs` is
   31,432 lines with 202 statics and 89 switches. The blind scan
   (`scan_one_frame`), the unregistered-frame sweep and the OSR fallback ran
   zero times here but every new frame shape must still be taught to them.
4. **Stack traces share the file** (`active_compiled_frames_*`,
   `compiled_frame_inline_chain`), so the exception path depends on the same
   walker.

## Plan

1. **Repeat the measurement on current `dev`, Linux, three runs per
   collector,** and add the rows that should stress the retired paths: deopt
   storms, JNI upcalls, virtual threads, the H2 and Spring suites. The
   question for each retired path is whether its counter is zero everywhere.
2. **Where a counter is zero on every row, make it a hard failure in debug
   builds first** (the path panics instead of scanning), run the regression
   suite, then delete the path and its switches. Order: blind fallback,
   unregistered-frame sweep, OSR fallback.
3. **G1 cross-thread coverage.** Find why the handshake that the other two
   collectors accept is refused on G1 (`xt_cov refused=100`), on current
   `dev`. This is the one measured place where a collector still cannot rely
   on the maps.
4. **Shrink the bands by making the map total.** For each claim rule, the
   fix is at the emitter: the word the rule classifies becomes a mapped slot
   or is cleared. Track two numbers per row, band roots published and band
   roots pinned; the target for both is zero. Problem 5's single frame
   format is what makes this finite.
5. **Move stack tracing to the per-site record** so GC, deopt and
   `Throwable` read one table.

## Done when

- Band roots published and pinned are zero on the measurement rows under all
  three collectors, and `CRATONVM_DBG_VERIFY_OOP_MAPS` reports no unmapped
  live word.
- The blind scan, the unregistered-frame sweep and the OSR fallback are
  deleted.
