# Complete Tomcat suite (651 classes) under all 3 GC backends — the three-way gap has mostly closed

| | |
|---|---|
| **Status** | Reference data point, supports `fixed-suite-bugs/tomcat/gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`. Re-measured 2026-08-11; the G1 crash column it used to carry is now fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md. |
| **Discovered** | 2026-08-10, `dev` merge, 3 parallel 2-worker full-suite runs (one per GC backend), each on its own uniquely-named binary. Repeated 2026-08-11 in the identical shape. |

## Method

Same `dev` commit for all three (merged same-day), same Windows fixture,
same 2-worker parallelism, default 300s per-class timeout, run concurrently
(so all three shared host CPU with each other — a fair three-way comparison,
though not an isolated-host measurement).

- **Default** (generational): no extra flag.
- **G1**: `-XX:+UseG1GC`.
- **ZGC**: a binary built with `--features zgc`, `-XX:+UseZGC`.

2026-08-10 binaries: `cratonvm-gc{default,g1,zgc}-20260810.exe`.
2026-08-11 binaries: `cratonvm-g1tom-20260811.exe` (default and G1 arms) and
`cratonvm-g1tomzgc-20260811.exe`; the G1 arm additionally ran with
`CRATONVM_DBG=g1-dbg-reach`, which costs nothing measurable (0.89× the 08-10
per-class time on the 81 classes compared mid-run).

## Results

| | PASS | FAIL | HANG | CRASH | NOSUMMARY | wall time |
|---|---|---|---|---|---|---|
| Default 08-10 | 519 | 16 | **115** | 0 | 1 | 356.5 min |
| **Default 08-11** | **628** | 12 | **11** | 0 | 0 | **177.5 min** |
| G1 08-10 | 578 | 35 | 34 | **4** | 0 | 267 min |
| **G1 08-11** | **623** | **7** | 20 | **1** | 0 | 212.9 min |
| ZGC 08-10 | **604** | 18 | 29 | 0 | 0 | 247.1 min |
| **ZGC 08-11** | **629** | 11 | **11** | 0 | 0 | 178.4 min |

## Reading this

**The 08-10 reading below is superseded on its main point.** It said the default
collector was "the worst backend on every axis except FAIL count" — 115 hangs
and 356 min. One day later the same fixture gives it 11 hangs and 177 min, and
the three backends are within 6 PASSes and 35 minutes of each other. Whatever
was costing the default collector 100 hangs was fixed in that window, not by
anything on this page. Treat single-day cross-backend gaps here as perishable.

What still holds:

- **The default collector's `NOSUMMARY` is gone** (0 on all three arms in the
  re-run), so it was not a standing property of that backend either.
- **G1 crashed, and that crash is now fixed.** 4 → 1 → **0**. The cause was an
  unaligned G1 TLAB carve, **not** the JIT-frame root-coverage gap this page
  originally credited — see
  fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md.
  The 1 in the 08-11 G1 row is not a VM crash either: it was
  `TestChunkedTransferEncodingWithProxy` faulting *inside* the verifier that
  `CRATONVM_DBG=g1-dbg-reach` enables — and that flag was on the **G1 arm
  only**. Flag off, the class passes 3/3. See
  fixed-suite-bugs/tomcat/g1-sigsegv-chunked-transfer-httpd-proxy-20260811-FIXED.md.
  Read the 08-11 G1 crash column as **0**.
- **The 08-11 arms were not otherwise identical**, and the Method section above
  now says so: the G1 arm carried a diagnostic the other two did not. That is a
  variable of the comparison, and it is what produced the row above. An
  instrument belongs on every arm or none.
- **ZGC is still marginally healthiest** and is still the narrower guarantee,
  for the reason in "Why ZGC dodges it" below — but with the G1 defect fixed and
  the default collector's hangs gone, the margin is now 1–6 classes, not 85.
- **G1 is now the slowest arm** (212.9 min vs 177/178). That is new and
  unexplained. It survives the obvious suspicion — the diagnostic ran at 0.89×
  the 08-10 per-class time on the 81 classes compared mid-run — but with that
  flag now known to have changed an outcome on this arm, the timing deserves a
  re-measure with the arms matched before anything is built on it.

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
  `fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md`,
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

## The default-only column — 63 classes, and 98% of them name the same cause

**63 classes are non-PASS under the default collector and PASS under BOTH G1
and ZGC** — 59 HANGs and 4 FAILs. That is over half of the default arm's 115
HANGs, and it is not 63 separate problems:

| group | n | shape |
|---|---:|---|
| `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite*ValidWrite*` | 44 | one parameterized family; 44 of its 64 classes hang under default only |
| other 300s HANGs | 15 | `TestAsyncContextImpl`, `TestRateLimitFilter{,WithExactRateLimiter}`, `TestVirtualContext`, `TestJNDIRealmIntegration`, `TestDefaultServletOptions`, `TestWebdavServletOptionCollection`, `TestCachedResource`, `TestHttp11Processor`, `TestAsync`, `TestStreamProcessor`, `TestStreamQueryString`, `TestEncodingDetector`, `TestParser`, `TestWarDirContext` |
| HTTP/2 FAILs | 3 | `TestHttp2Limits`, `TestHttp2Section_5_1`, `TestHttp2Section_8_1` |
| `TestCharChunkLargeHeap` | 1 | a different defect — see below |

**The attribution is quantified, not asserted.** Counting
`[moving-young] fallback` lines in each class's own `.log.err`:

| | logs a fallback |
|---|---|
| the 63 default-only non-PASS classes | **62 / 63 (98%)** |
| the default arm's 519 PASSing classes | 47 / 519 (9%) |
| the same 63 classes on the **G1** arm | **0** |
| the same 63 classes on the **ZGC** arm | **0** |

The counter reaches **#1024** within a single class, across seven reasons:
`xt-helper-window-conservative-scan` (289 lines), `unregistered-jit-frame-on-stack`
(256), `innermost-rbp-belongs-to-unguarded-callee` (209),
`compiled-frame-oop-not-published` (53), `active-safepoint-map-incomplete` (52),
`compiled-frame-band-unbounded` (4), `cross-thread-jit-peer` (4). The logs show
these classes making *progress* the whole time — Tomcat starting, servicing,
stopping — just far too slowly to finish inside 300s. Throughput, not deadlock.

This is `fixed-suite-bugs/tomcat/gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`
at class-list scale, and the 0-vs-0 rows are why the other two backends are
clean here: the mechanism is generational-only by construction, so a collector
that does not have that young-generation copying path cannot exhibit it.

### It reproduces on current dev, unchanged

All 63 re-run under the default collector on current `dev`, one class per
process, 400s budget (not 300s), 2-parallel — run `defonly63-default-20260810`:

| | |
|---|---:|
| HANG | **51** |
| PASS | 11 |
| FAIL | 1 |

**The `TestHttpServletDoHead*` family is 44 HANG out of 44** — not one of them
moved, at a budget a third larger than the one that produced the original
column. The 11 that now pass are all from the "other HANGs" and "HTTP/2 FAILs"
groups, i.e. the classes that were merely near the boundary. `TestCharChunk-
LargeHeap` still FAILs in 3.1s, exactly as before, because it is the separate
allocation-size defect below and nothing about it has changed.

So this is not a stale measurement being kept alive by an old binary: the
generational collector's largest single behaviour gap in this suite is
untouched by everything that landed between 2026-08-09 and 2026-08-10. **That
is the finding this page's default flip rests on** — see
[`docs/gc-tuning.md`](../../gc-tuning.md).

### `TestCharChunkLargeHeap` is a separate default-only defect

FAIL in 2.9s, not a hang, and no fallback involved:

```
java.lang.OutOfMemoryError: Java heap space (alloc_array length 2147483639)
  at org.apache.tomcat.util.buf.CharChunk.makeSpace(CharChunk.java:425)
```

The test asks for a ~2 GB `char[]` (4 GB of payload) at `-Xmx 2g`. G1 and ZGC
both serve it and pass in ~6.4s — G1 through humongous regions, ZGC out of its
single arena — while the generational heap cannot, because a semi-space young
generation is a fraction of `-Xmx` and the array has to fit in one space. Note
the direction: this is the **mirror image** of ZGC's `ZipContentTests` heap
floor. Each collector has an allocation shape it serves worst, and neither is
"the correct one".

## The G1-only column — 6 classes, plus the 4 CRASHes

**6 classes are non-PASS under G1 and PASS under both default and ZGC.** The 4
CRASHes are not among them (those classes hang under the other backends), so
they are carried here too. Re-run on current `dev` under `-XX:+UseG1GC`, with a
3-arm control for everything that stayed bad:

| class | 08-10 G1 | G1 now | default now | ZGC now | verdict |
|---|---|---|---|---|---|
| `TestDigestAuthenticatorAlgorithms` | HANG | **PASS 29.6s** | — | — | gone |
| `TestXmlValidationUsingContext` | FAIL | **PASS 56.3s** | — | — | gone |
| `TestChunkedTransferEncodingWithProxy` | HANG | **PASS 314.5s** | — | — | gone (but 314s — budget-bound) |
| `TestCoyoteAdapterCanonicalization` | FAIL | FAIL 26.6s | PASS 38.8s | PASS 36.9s | **G1-only — the zeroed-header defect** |
| `TestSwallowAbortedUploads` | FAIL | FAIL 68.9s | PASS 45.8s | PASS 41.1s | G1-only, unattributed |
| `TestEncryptInterceptorLargeHeap` | HANG | HANG 400s | PASS 215.6s | PASS 147.4s | **G1-only, not root-caused** |
| `TestHostConfigAutomaticDeploymentModification` | CRASH | HANG 400s | HANG 400s | HANG 400s | no longer G1-specific |
| `TestHostConfigAutomaticDeploymentWar` | CRASH | FAIL 274.4s | HANG 400s | PASS 158.2s | crash gone |
| `TestHostConfigAutomaticDeploymentWarXml` | CRASH | **PASS 134.4s** | — | — | crash gone |
| `TestHttpServletDoHeadInvalidWrite1ValidWrite511` | CRASH | **PASS 102.3s** | — | — | crash gone |

**Zero CRASHes on the rerun** — all four are PASS/FAIL/HANG now. The G1-only
column is down from 6 to 3.

`TestCoyoteAdapterCanonicalization` is the one that belongs to
fixed-suite-bugs/tomcat/g1-sigsegv-unguarded-callee-jit-frame-FIXED.md
and is worth adding to its evidence: **166** `g1::get_field: out-of-bounds field
read dropped … num_slots=0 class_id=ClassId(0)` guard hits, **0** on both other
arms of the same class. That is the same zeroed-header-on-a-live-object
signature that page documents, presenting here as a cascade of
`LifecycleException: lifecycleBase.stopFail` rather than as a SIGSEGV.

The other two are **not** that defect and should not be filed under it:
`TestSwallowAbortedUploads` logs zero guard hits and fails on
`SocketException: Connection aborted (os error 10053)` — the load-flake shape,
same as the ZGC `TestHttp2Section_6_8` above. `TestEncryptInterceptorLargeHeap`
logs zero guard hits and produces no output past the JUnit banner, i.e. it
stalls before the first test reports; given the name and G1's humongous path
that is where to look, but nothing here establishes it.

## Not yet done

- The default-GC run's `NOSUMMARY` was `org.apache.catalina.tribes.test.channel.TestDataIntegrity` — already part of the known-environmental multicast family (see [!tribes-multicast-family-still-environmental.md](!tribes-multicast-family-still-environmental.md)), but a VM abort with no JUnit summary at all is a stronger symptom than that family's usual assertion failures. Not yet checked whether this is a distinct VM-abort defect or the same environmental flakiness manifesting differently under load.
- ~~Diff the FAIL/HANG class lists across the three backends.~~ **DONE
  2026-08-10**, all three columns — see the three sections above. Summary:
  default-only 63, G1-only 6, ZGC-only 2. The asymmetry is the finding: the
  default arm's problem is one mechanism replicated across a parameterized
  family, G1's is a handful of classes plus a crash mode that is now gone, and
  ZGC's was two defects that are fixed.
- Isolated (non-concurrent) reruns per backend, since these three shared the
  host with each other.
