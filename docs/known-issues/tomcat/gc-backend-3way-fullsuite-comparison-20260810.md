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
  crashes. The caution is right; the reason first given for it was not, and is
  corrected in "Why ZGC dodges it" below.

## Why ZGC dodges it — corrected 2026-08-10

The first version of this page discounted ZGC's result on the grounds that its
newer `src/zgc/` modules were "**not wired into the real allocator yet**",
quoting `gc/Cargo.toml`, which in turn quoted the "Production-ZGC submodules"
banner in `gc/src/zgc.rs`. **That chain was stale at both ends.** Counted in
`src/zgc.rs` outside its `mod tests`, on the commit these runs used:

| adopted by `ZgcRealHeap` | uses | not adopted |
|---|---:|---|
| `census` | 24 | `barrier` (named in comments only) |
| `mark` | 15 | `forwarding` |
| `tlab` | 12 | `remembered` |
| `vaddr` | 7 | `generation` |
| `page` | 2 | `relocate` |
| `metrics` | 1 | `adapters` |

Six of twelve, not zero — and the allocator is the *wrong* example to pick:
the ZGC TLAB is default-ON and serves `alloc_object`/`alloc_array` through
`alloc_raw_tlab`. (The module count was also 11, not 12; `adapters` has since
been declared.) Both upstream comments are fixed.

**What is still true, and is the actual reason:** `ZgcRealHeap` is a
stop-the-world **non-moving** mark-sweep. The six unadopted modules are exactly
the moving/generational/concurrent machinery, and `vm_init.rs` hard-codes
`RELOCATION_REQUESTED = false`. The pathology hurting the other two backends is
a *moving* collector's JIT-frame root-coverage gap — the default GC falls back
to a non-moving sweep to stay safe (slow), G1 evacuates anyway (SIGSEGV). A
collector that never relocates anything cannot be exposed to it at all.

So the caution stands, restated: ZGC's lead here is **a narrower guarantee, not
a broader correctness**. It is not evidence that ZGC is more complete — it is
evidence that the defect is specific to relocation. Three independent findings
say ZGC is *not* simply more correct:

- two ZGC-only defects were root-caused and fixed on 2026-08-10 (a missing
  reference-array un-box, and a stale generated-`$ProxyN` cache) — see
  [`zgc-real-fullsuite-regression-RETIRED-20260808.md`](../../internal/fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md),
  which replaces the dead `springboot/zgc-real-fullsuite-regression-20260807.md`
  link this page used to carry;
- ZGC needs measurably more heap for the same work: `ZipContentTests` OOMs at
  `-Xmx 2g` under ZGC and passes at 3g, where the default collector passes at
  2g — the price of not compacting;
- it still has 29 HANGs and 18 FAILs here.

## The ZGC half of the class-list diff — done 2026-08-10

Diffing the three `results.csv` files (the item below), the ZGC column holds
**47 non-PASS classes, of which exactly 2 are ZGC-only** (PASS under both the
default collector and G1):

| class | ZGC | verdict |
|---|---|---|
| `org.apache.el.util.TestMessageFactory` | FAIL 0.6s | **the reference-array un-box defect — fixed** |
| `org.apache.coyote.http2.TestHttp2Section_6_8` | FAIL 95s | socket resets under the 3-way concurrent load; unattributed |

**These runs predate both ZGC fixes.** The result directories were created
2026-08-09 23:24; the fixes merged to `dev` at 05:09 on 08-10, so this page's
ZGC column was measured on a binary without them.

`TestMessageFactory` is the interesting one and it is a clean catch.
`testFormatChoice` asserted `100 is enough` and got `100 is too many` — the
bundle entry is a `ChoiceFormat` pattern, `ChoiceFormat` keeps its thresholds
in a `double[]`, and a zeroed limits array makes every branch match so the last
one wins. `probes/ChoiceFormatProbe.java` shows it directly:

```
ChoiceFormat("0#too few|99#enough|100<too many").getLimits()
  HotSpot / default collector -> [0.0, 99.0, 100.00000000000001]
  -XX:+UseZGC, pre-fix        -> [0.0, 0.0, 0.0]           -> format(100) = "too many"
```

Deterministic, `--nojit`, any heap size. Re-run on current `dev` under
`-XX:+UseZGC`, both classes **PASS** (`TestMessageFactory` 0.6s,
`TestHttp2Section_6_8` 37.9s) — run
`zgconly2-zgc-20260810`.

The other 45 non-PASS classes are shared with at least one other backend, so
none of them is a ZGC signal. Worth noting for the reruns below: 25 of them are
300s HANGs on **all three** backends (the `TestHostConfigAutomaticDeployment*`
and `TestDefaultServletEncoding*` families), i.e. collector-independent.

## Not yet done

- The default-GC run's `NOSUMMARY` was `org.apache.catalina.tribes.test.channel.TestDataIntegrity` — already part of the known-environmental multicast family (see [tribes-multicast-family-still-environmental.md](tribes-multicast-family-still-environmental.md)), but a VM abort with no JUnit summary at all is a stronger symptom than that family's usual assertion failures. Not yet checked whether this is a distinct VM-abort defect or the same environmental flakiness manifesting differently under load.
- Diff the FAIL/HANG class lists across the three backends (not all 3 runs'
  non-PASS classes are the same 15-35 classes; a class that hangs under
  default but passes under G1/ZGC is a much stronger signal than one that
  fails everywhere). **ZGC half done above** — 2 ZGC-only classes, both now
  passing. The default-only and G1-only columns are still owed; G1's 4 CRASHes
  in particular (`TestHostConfigAutomaticDeploymentModification` and
  `…DeploymentWar` are HANGs under both other backends and CRASHes under G1)
  belong to [g1-sigsegv-unguarded-callee-jit-frame.md](g1-sigsegv-unguarded-callee-jit-frame.md).
- Isolated (non-concurrent) reruns per backend, since these three shared the
  host with each other.
