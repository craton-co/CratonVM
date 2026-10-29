> **Host check (orchestrator, 2026-10-29): STILL LIVE on Linux.** On 64.177.43.150, default `--compatible`
> `R26SegLongLoops` hits the battery's 300 s timeout in 6 of 6 interleaved runs on both snapshot 15
> (`7045eb2c6b` + fix-ups) and snapshot 16 (`2ae2841723`, dev `2ce3296084` merged); `time heap-put` is
> 321-324 ns/op in every run. `--jdk-only` passes. s15's own battery run passed once, so the state is bistable (see
> [`j27-seglongloops-heap-sum-bistable-slow-state`](../../internal/fixed-bugs/jit/j27-seglongloops-heap-sum-bistable-slow-state-FIXED-20261028.md)). The Windows "not reproduced" note below holds
> for Windows only. Performance, so out of scope for j27 wave 9: this page stays filed.

> **j27 wave 9 re-check after the dev merge (lane W9VER, 2026-10-29): NOT REPRODUCED on the merged tree.** On `7045eb2c6b` (Windows `cvm-ver-m1.exe`) `R26SegLongLoops --compatible` takes 2 s (`--jdk-only` 2 s) and prints HotSpot's fact; the 551-585 s runs were on `cvm-ver-2.exe` (`47479dc6e0` + WIP). Not bisected (perf is out of scope); the host should confirm before the page is retired.

# `--compatible`: the fast-boot receiver-profile bypass makes `R26SegLongLoops` ~180x slower

**Status:** open (performance; filed only, perf is out of scope for j27 wave 9). Found by j27 wave 9,
lane W9VER (builder, Windows), 2026-10-29.
**Area:** `--compatible` start-up tuning that survived dev's fast-boot revert: `vm/src/vm/vm_init.rs`
"Phase 4 Fast-Boot Tuning" (`receivers_wanted` is false in `--compatible` while
`CRATONVM_COMPAT_FAST_BOOT_JIT` is on, its default, unless `CRATONVM_TIER_PGO_RECEIVERS=1`) and
"Phase 2" (`jit_invocation_threshold` 500 -> 2000).

## Evidence (measured, Windows, release LTO-off build of the j27 tree at 47479dc6e0 + wave-9 WIP)

`tools/probes/jit/r26/R26SegLongLoops.java`, wall seconds of the whole run (facts equal HotSpot in
every arm: `fact sums 8415232 8386560 true`; HotSpot 25.0.3: 2 s):

| arm | seconds |
|---|---|
| `--jdk-only` | 3 |
| `--compatible` (defaults) | 585, 551 (two runs, also 551 with `CRATONVM_JIT_OSR_REENTRY_PENDING_REASK=1`) |
| `--compatible` + `CRATONVM_COMPAT_FAST_BOOT_JIT=0` | 3 |
| `--compatible` + `CRATONVM_TIER_PGO_RECEIVERS=1` | 3 |
| `--compatible` + `CRATONVM_JIT_THRESHOLD=500` (undoes only the threshold raise) | > 700 (killed by the timeout) |
| `--compatible` + `CRATONVM_TIER_PGO_RECEIVERS=1` + `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT=1` | 10 |
| `--compatible` + `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT=1` (defaults otherwise) | > 600 (killed by the timeout) |

So the receiver-profile bypass alone is the cause, not the raised threshold: without receiver
profiles the segment accessor sites in the hot loops never get the monomorphic shape the optimizing
tier needs, and the loops stay on a slow path (not measured further; no profile taken).

## What would fix it

Decide the bypass per HotSpot: HotSpot always records receiver types in `MethodData` once a method is
profiled, so the bypass is a CratonVM-only start-up trade. Either drop it (follow dev's revert of the
fast boot to its end), or keep receiver profiling on for methods that reach the C1 profile tier (the
cold-start saving comes from methods that never get there). Measure Spring Boot / Quarkus start-up and
this probe in both arms before choosing; check the other `--compatible` probes that loop over
`MemorySegment` / megamorphic sites (`R23SegMono` is 3 s in both modes, so not every loop is hit).
