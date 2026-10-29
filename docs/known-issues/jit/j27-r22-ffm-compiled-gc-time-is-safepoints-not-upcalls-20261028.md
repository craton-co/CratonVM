# `R22FfmCompiledGc` spends its time at safepoints, not in up-calls (Windows)

> **j27 wave 8 (lane W8FFM, 2026-10-28): NOT FIXED (NEEDS MEASUREMENT); probe and triage recipe added, PENDING HOST RUN.** No VM code change: the split needs no new census, because the existing channels already answer each part (`CRATONVM_DBG_STW_CENSUS=1` prints `[stw-ttsp] ttsp_us=..` per pause, which includes every peer's pre-park `update_root_snapshot` / JIT frame scan; `CRATONVM_GC_STATS=1` prints each pause). What was missing is a way to remove the forced collections from the same probe, and a per-collection normaliser, since the churn thread's `System.gc()` count depends on how fast it runs (so the two VMs do a different number of collections).
> - **Probe:** `tools/probes/jit/j27/J27W8FfmGcSplitRows.java`, R22FfmCompiledGc's shape with `-Dj27.churnGc=false` (the churn thread still allocates, never collects) and `-Dj27.upcallGc=false` (the comparator never collects). Expected HotSpot 25, every arm:
>   ```
>   fact llabs-held 19999900000
>   fact llabs-critical-held 19999900000
>   fact qsort-unsorted 0
>   fact held-boxes-ok true
>   fact upcalls-positive true
>   ```
>   `-Dj27.time=true` adds `time main-ms`, `time gc-count` (MXBean collections over the rounds), `time forced-gc` (the `System.gc()` calls) and `time ms-per-forced-gc`.
> - **Recipe** (Linux host and Windows P-cores; one binary; interleaved; median of 3):
>   ```
>   export JAVA_HOME=<jdk25>; javac -d out tools/probes/jit/j27/J27W8FfmGcSplitRows.java
>   F="--enable-native-access=ALL-UNNAMED -Xmx256m -Dj27.time=true -cp out J27W8FfmGcSplitRows 2"
>   for rep in 1 2 3; do for gc in true false; do
>     $JAVA_HOME/bin/java -Dj27.churnGc=$gc $F > hs.$gc.$rep
>     cratonvm -Dj27.churnGc=$gc $F > cvm.$gc.$rep
>   done; done
>   # per-pause split, CratonVM, churn on, one run each:
>   CRATONVM_DBG_STW_CENSUS=1 cratonvm $F 2> census.err      # grep '\[stw-ttsp\]'
>   CRATONVM_GC_STATS=1 cratonvm --verbose:gc $F 2> stats.err
>   ```
>   Every `fact` line must equal HotSpot in every arm. Let `W(gc)` be the median `main-ms`.
> - **How to read it (flip rule for the next step):**
>   - `W(false)` within about 3x of HotSpot's `W(false)`, and `W(true) - W(false)` far above HotSpot's: this page's claim holds. The per-collection cost of the main thread is `(W(true) - W(false)) / forced-gc`; compare it with HotSpot's. Split it with the census: the median `ttsp_us` (arrival work, incl. `update_root_snapshot`, `scan_active_jit_frames`, `publish_peer_jit_coverage_for_stw`), the pause itself from `GC_STATS`, and the rest (post-pause fix-up, `refresh_moving_young_coverage_for_current_thread`). The largest part names the fix; file it under `docs/known-issues/gc/`.
>   - `W(false)` itself far above HotSpot's: the FFM work is the cost after all; go back to `ffm-upcall-per-call-costs-microseconds-20261027.md`, and A/B `-Dj27.upcallGc=false` to drop the comparator's collections too.
>   - `forced-gc` much lower on CratonVM than on HotSpot, with `W(true)` still higher: each collection is slower, not more numerous; the GC_STATS pause times decide.

| | |
|---|---|
| **Status** | **OPEN** (perf). Filed 2026-10-28 by j27 wave 5, lane FFMUP. |
| **Area** | GC / safepoint (`gc_and_alloc::safepoint_check`, `jit::conservative_roots`). Not the FFM lane's files. |
| **Probe** | `tools/probes/jit/r22/R22FfmCompiledGc.java 2` (`-Xmx256m`). |

## Evidence

Windows, P-cores pinned, the 2026-10-28 tree: 45-55 s wall, HotSpot 25 about 2 s.
[`ffm-upcall-per-call-costs-microseconds-20261027`](ffm-upcall-per-call-costs-microseconds-20261027.md)
blamed the up-calls. The probe also runs a churn thread that allocates 4 KiB arrays and
calls `System.gc()` every 4,000 of them, for the whole run.

The stack sampler on the MAIN thread (the one running `llabsHeld` / `qsortHeld`), 10 s from
5 s in, 4,558 samples:

| Samples | What |
|---|---|
| 3,548 (78%) | `gc_and_alloc::safepoint_check` (from `jit_safepoint_slow_path`): `update_root_snapshot`, `scan_active_jit_frames` / `scan_one_frame_precise`, `publish_peer_jit_coverage_for_stw`, `refresh_moving_young_coverage_for_current_thread`, the barrier wait |
| 133 (3%) | `panama_upcall::upcall_entry` (the whole up-call path) |

The churn thread spends 70% in `force_gc` (`full_gc_door`, `stw_take_over_and_wait`,
`helper_window_pass`).

So the main thread stops for every forced collection and re-scans its compiled frames each
time. The up-calls are a few percent of the run here. The Linux host's 137-397 s (lane WINPOLL) has not been
broken down. Run the same split there (perf, main thread only) before blaming up-calls.

## What would fix it

The cost of a mutator's participation in a forced full collection (`safepoint_check`'s root
snapshot and precise JIT frame scan) against HotSpot's, per `System.gc()`. Count the
collections (`CRATONVM_GC_STATS=1`) and divide.
