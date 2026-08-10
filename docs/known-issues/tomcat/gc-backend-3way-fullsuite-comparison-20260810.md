# Complete Tomcat suite (651 classes) under all 3 GC backends — default is worst, ZGC is healthiest

| | |
|---|---|
| **Status** | Reference data point, supports [gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md) and [g1-sigsegv-unguarded-callee-jit-frame.md](g1-sigsegv-unguarded-callee-jit-frame.md) |
| **Discovered** | 2026-08-10, `dev` merge, 3 parallel 2-shard full-suite runs (one per GC backend), each on its own uniquely-named binary |

## Method

Same `dev` commit for all three (merged same-day), same Windows fixture,
same 2-shard parallelism, default 300s per-class timeout, run concurrently
(so all three shared host CPU with each other — a fair three-way comparison,
though not an isolated-host measurement).

- **Default** (generational): `cratonvm-gcdefault-20260810.exe`, no extra flag.
- **G1**: `cratonvm-gcg1-20260810.exe`, `-XX:+UseG1GC`.
- **ZGC**: `cratonvm-gczgc-20260810.exe` (built with `--features zgc`), `-XX:+UseZGC`.

## Results

| | PASS | FAIL | HANG | CRASH | NOSUMMARY | wall time |
|---|---|---|---|---|---|---|
| **Default (generational)** | 519 | 16 | **115** | 0 | 1 | 356.5 min |
| **G1** | 578 | 35 | 34 | **4** | 0 | 267 min |
| **ZGC** | **604** | 18 | **29** | 0 | 0 | **247.1 min** |

## Reading this

- **Default is the worst backend on every axis except FAIL count**: most
  hangs by far (115 vs. 34/29), a `NOSUMMARY` (the VM died before JUnit could
  print a summary — worth its own look, not yet identified which class), and
  the longest wall time despite having the fewest genuine FAILs. This is
  consistent with — and a much larger-scale confirmation of —
  [gc-moving-young-persistent-nonmoving-fallback-regression.md](gc-moving-young-persistent-nonmoving-fallback-regression.md)'s
  hypothesis that the generational collector's persistent fallback to a
  non-moving sweep is a broad throughput problem, not a niche one.
- **G1 finishes faster and hangs less, but crashes** — see
  [g1-sigsegv-unguarded-callee-jit-frame.md](g1-sigsegv-unguarded-callee-jit-frame.md).
  The same underlying JIT-frame root-coverage gap that makes the default GC
  slow makes G1 unsafe instead.
- **ZGC currently looks healthiest**: fewest hangs, fastest wall time, zero
  crashes. Treat this with some caution rather than as "ZGC is production
  -ready" — `gc/Cargo.toml`'s own comments (as of this session) describe
  ZGC's newer `src/zgc/` modules as compiling and unit-tested but **not
  wired into the real allocator yet** ("the whole-heap allocator is still
  linear mark-sweep"), and separately note a known regression at
  `docs/known-issues/springboot/zgc-real-fullsuite-regression-20260807.md`.
  A simpler/more conservative allocator would plausibly dodge the exact
  moving-young-collector pathology hurting the other two backends without
  that meaning ZGC's own design is more correct or complete.

## Not yet done

- The default-GC run's `NOSUMMARY` was `org.apache.catalina.tribes.test.channel.TestDataIntegrity` — already part of the known-environmental multicast family (see [tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md)), but a VM abort with no JUnit summary at all is a stronger symptom than that family's usual assertion failures. Not yet checked whether this is a distinct VM-abort defect or the same environmental flakiness manifesting differently under load.
- Diff the FAIL/HANG class lists across the three backends (not all 3 runs'
  non-PASS classes are the same 15-35 classes; a class that hangs under
  default but passes under G1/ZGC is a much stronger signal than one that
  fails everywhere).
- Isolated (non-concurrent) reruns per backend, since these three shared the
  host with each other.
