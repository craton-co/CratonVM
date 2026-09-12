# H2 `TestValueMemory` under G1 — the conservative-JIT-root account, RESOLVED 2026-09-12

*Retires `known-issues/h2/testvaluememory-fails-under-g1-on-conservative-jit-roots-20260908.md`.*

**Every cause that page names is now either implemented or refuted by
measurement.** What is left of the symptom has a different cause, is measured
here, and has its own page:
`known-issues/gc/g1-pins-a-region-for-an-interior-array-cursor-20260912.md`.

| | |
|---|---|
| **Status** | **RESOLVED as an account of the defect.** Type 0 was already fixed (`3224 -> 2227`). The G1 JIT pin set is now genuinely narrowed rather than nominally: the movable filter drops **6 of 7** pins, where it was off and dropped none of 34. |
| **Landed here** | the frame-deopt `SavedRegisters` partition (`FrameLayout::deopt_gpr_lo` / `deopt_xmm_lo`), and `CRATONVM_GC_G1_MOVABLE_PINS` **default ON**. |
| **Refuted here** | the page's own "the A5 span sweep is most of it" — `a5_sweeps=0`, `a5_roots=0` on this workload, so the sweep contributes nothing to measure. |
| **Measured on** | `dev@c0bebbde5` + this change, Windows 11 / x86-64 / 8 cores, `probes/TvmProbe.java` (a standalone port of the test, added here), `-XX:+UseG1GC --Xmx 2g`. |

---

## 1. The instrument, because the test itself cannot be run

`org.h2.test.unit.TestValueMemory` extends `org.h2.test.TestBase`, which lives in
H2's `src/test` tree and is **not published to Maven Central** — the
`h2-<v>.jar` carries `org/h2/**` and zero `org/h2/test/**` entries. Everything
the measurement needs from `TestBase` is `assertEquals`, `fail` and a
`config.traceTest` flag.

`probes/TvmProbe.java` is that transcription: `testType`, `create`, and the
`DataHandler` / `LobStorageInterface` implementations the LOB types need, with
the three `TestBase` calls inlined. It prints **every** row rather than stopping
at the first failure, which is what makes a rate measurable at all.

It is calibrated against the reference: HotSpot 25, `-Xmx2g`, reads
**Type 0 = 488**, which is the number the retiring page reports for the same
row, to the kilobyte.

## 2. What was actually costing the rows

The page's account was `a5_roots=4794` from the unregistered-frame span sweep.
That is no longer true and the census says so in one line:

```text
[bandpath] bands=194 fallback=0 foreign_innermost=0 a5_sweeps=0
           a5_roots=0 a5_frames=0 published=(movable=348 unrewritable=1010 …)
```

`a5_sweeps=0` over the whole run. The cost was in the **band path's own
unrewritable half**, and the pin census names the storage class:

```text
[bandword] 0x…bc80000 off=720 refusal=callee-saved region=outgoing-args-or-deopt-regs
[bandword] 0x…bc80040 off=712 refusal=callee-saved region=outgoing-args-or-deopt-regs
[jitpins]  words=13 distinct=6 unrew_set=5 movable_set=2 …
[g1][PINS] pin_addrs=6 pin_regions={0, 1, 6, 82} pinned_bytes=2546K
           [6:HumongousStart occ=976K pins=1 jit 0x…7080000(+0x0,Object,1000016B)]
           [82:Eden occ=997K/1024K pins=3 jit 0x…bc80000(64B) 0x…bc80040(80B) 0x…bc801d0(56B)]
```

**Region 82: 997 KB of Eden held out of the collection set to protect 200 bytes
of objects.** `Used memory` on that row read 3225 against a 2928 threshold;
ZGC and the generational collector read 2227 on the identical row. The
difference is one G1 region, and the three words holding it were all
`outgoing-args-or-deopt-regs`.

### `outgoing-args-or-deopt-regs` is two regions and they want opposite treatment

`FrameLayout::region_name` buckets everything past `reg_spill_hi` under that one
name. It is:

* the **outgoing-argument reserve** — uninitialised memory `SUB RSP, frame_size`
  never writes, which `is_dead_outgoing_reserve` has skipped since 2026-09-09
  (it accounted for 1624 skipped words in the run above); and
* the frame-deopt **`SavedRegisters`** region — 256 bytes the deopt stub spills
  the whole register file into, which is NOT uninitialised and NOT this frame's
  to discard.

`outgoing_lo` was published so a consumer could tell them apart, and every
surviving refusal above is in the second. So the repair is about
`SavedRegisters`, and its two halves need **opposite** arguments.

## 3. The fix: one 256-byte region, two halves, two arguments

Both halves are now published as their own ranges
(`FrameLayout::deopt_gpr_lo/hi`, `deopt_xmm_lo/hi`) by both backends, and
`region_name` reports `deopt-saved-gpr-image` / `deopt-saved-xmm-image`.

The split is almost all of the bucket, which is why naming the two halves was
the whole of the work. Per-run `[bandword]` refusals, one binary each:

| | `outgoing-args-or-deopt-regs` | `safepoint-gpr-spill-image` |
|---|---:|---:|
| `dev@c0bebbde5` | 2888 | 732 |
| + the GPR half rewritten | 1394 | 726 |
| + the XMM half let go of | **36** | 732 |

Thirty-six. The uninitialised outgoing reserve — the thing the bucket is named
after and the thing `is_dead_outgoing_reserve` was written for — was 1.2 % of
it; the other 98.8 % was `SavedRegisters`.

**The GPR half is REWRITTEN.** `register_image_remap_admits` now admits it, so
`remap_register_image_words` fixes up a moved reference there on every
collection. The argument is `deopt::try_resolve_value`: `FrameValue::RegisterRef`
reads `regs.gpr` and yields an `Object`, so the deopt stub genuinely resumes
from these words and a stale one is reconstructed into an interpreter frame at
its pre-move address. **This is a correctness completion, not only a pin
optimisation** — it was safe before only because the pin prevented the move.

**The XMM half is LET GO of.** It is published movable with no rewriter behind
it, on the dead-word arm of the same partition `band_slot_is_verifiable` already
spends. The argument is the same function read the other way: `regs.xmm` is
touched from exactly two arms, `XmmFloat` and `XmmDouble`, tagged `Float` and
`Double`. **No reference is ever recovered from this half**, so a word there
needs no rewrite and its object needs no megabyte-granular pin.

Deliberately NOT done: rewriting the XMM half. That is what
`CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` does, its own doc comment says why it is
a diagnostic and not a default, and rewriting a `double` whose bits happen to
equal a moved object's base is a silent wrong answer.

## 4. And the consumer, because the producer alone does nothing

`CRATONVM_GC_G1_MOVABLE_PINS` is now **default ON**. It shipped off on
2026-09-09 for a measured reason — it dropped 1 pin in 34, because the A5 span
sweep produced roots that made no claim either way. With that sweep inert and
the `SavedRegisters` half correctly partitioned, the same filter now drops 6 of
7:

```text
[g1][MOVPIN] snapshot=7 kept=1 movable_claimed=7 unrew_veto=1
             honour_movable=true coverage_incomplete=false movable_set=7
```

**Neither half does anything alone, and that is the whole argument for flipping
both together.** Four arms of one binary, run CONCURRENTLY so this host's load
applies to all of them equally:

| arm | rows over threshold | worst row | sum of all 40 rows |
|---|---:|---:|---:|
| neither | 4 | 3224 | 91988 |
| the pin filter alone | 4 | 3224 | 91042 |
| the remap widening alone | 3 | 3224 | 91790 |
| **both** | **0** | **2537** | **59357** |

## 5. What it is worth, measured

Two batteries, each **sequential and alternating** so any host drift lands on
both arms. (Sequential rather than interleaved-concurrent, which is this
project's usual protocol: two VMs side by side reach a pre-existing SIGSEGV
whose crash handler then hangs, and that costs more reps than the interleaving
saves.) The second battery was run AFTER merging `dev` and rebuilding both
arms, because a merge that touches none of your files can still move your
numbers:

| battery | arm | complete | SIGSEGV | rows over threshold | sum of all 40 rows |
|---|---|---:|---:|---|---:|
| `dev@c0bebbde5` | base | 6 of 12 | 6 | mean **4.67** | **93136** |
| | + this change | 12 of 12 | 0 | mean **1.83** | **80170** |
| `dev@0a805f3fc` | control | 6 of 10 | 4 | mean **4.50** | **92535** |
| | + this change | 9 of 10 | 1 | mean **1.89** | **80457** |

Two trees thirty-seven commits apart agree to within 1 % on both columns.

And the floor, for scale — `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, unsound, no
conservative JIT roots at all: **0 rows over, sum 57621, worst row 2228.**

So of the ~35 000 KB the JIT root scan costs this suite, this change recovers
about **12 000** and §6 names the rest.

**The SIGSEGV column is an observation these batteries were not built to make**,
and it is carried rather than claimed. Pooled, it is **10 crashes in 22 control
runs against 1 in 22** (Fisher's exact p ~ 0.003) — a reduction, not an
elimination, and the first battery's 6-to-0 would have read as one. There is a
mechanism that would explain it: `remap_one_frame_register_images` rewrites an
admitted region's moved references whether or not the conservative scan rooted
the word, so before this change a reference in the deopt GPR image that
`is_object_address` happened to reject was neither pinned nor rewritten.
Confirming that needs a crash-focused battery, not these. The companion page
carries the detail.

The rows that still fail do
so by 50–330 KB against a 2928 KB threshold — one G1 region, on one row, and
the pass/fail boundary is inside this host's noise (a band-skip bisect over four
storage classes moved the failure count while moving the sum by under 2 %,
which is what a vacuous bisect looks like).

## 6. What is left, and why it is a different page

With the filter honoured, the JIT pin set on the worst row is one address — the
live 1 000 016-byte `array`, in its own humongous region, which is not a young
CSet candidate and costs nothing. The region that still holds the row over
threshold is put back by a DIFFERENT publisher:

```text
[g1][MOVPIN] snapshot=7 kept=1 …
[g1][PINS]   pin_addrs=7 pin_regions={0, 1, 5, 75} pinned_bytes=2000K
[g1] root is not an object (#32): addr=0x…58001d0 verdict=Object region=75 type=Eden
     grid=INTERIOR of=0x1a0 delta=0x30 size=0x60 cid=0 kind=Array idx=5
     — pinning region 75 instead of evacuating it.
```

The filter kept ONE region; the pin set has FOUR, because
`pinned_region_set_including_non_object_roots` re-adds the region of any root
that is not the start of a live object — and a compiled frame's band word
holding an **interior pointer into an array** is exactly that. It cannot be
narrowed by the movable partition, because `remap_one_jit_frame` rewrites
through a `PointerMap` keyed by object BASE: an interior address is not a key,
so no movable claim for it could ever be honoured.

That is a different mechanism from the one this page is about, it is not
fixable in the collector (G1 frees a region by evacuating everything out of it,
so sub-region pinning is not available), and it gets its own page:
`known-issues/gc/g1-pins-a-region-for-an-interior-array-cursor-20260912.md`.

## 7. Disposition of every claim the retiring page made

| claim | disposition |
|---|---|
| Type 0 is 3224 on G1 against 2227 elsewhere | **FIXED** before this session (`claude/jit-reg-oop-maps-20260909`); still 2227/2228. |
| the 977 KB floor is the live `array`, not a value-cell layout | **STANDS.** `--nojit` reads 977; the floor arm's worst row is 2228. |
| item 1 — narrow the pin set with a base test | **REFUTED**, by the page itself. |
| item 2 — per-safepoint liveness for compiled-frame words | **IMPLEMENTED** before this session; `deadspill=7622 outgoing=1876` words skipped per run. |
| "the A5 span sweep is most of it", `a5_roots=4794` | **REFUTED HERE.** `a5_sweeps=0 a5_roots=0` on this workload; the page's own census is the instrument. |
| `CRATONVM_GC_G1_MOVABLE_PINS` is "correct, wired, and starved" | **STANDS as a diagnosis**; it is no longer starved, and it is now the default. |
| the deopt `SavedRegisters` block "is not what keeps the last five regions pinned" | **REFUTED HERE.** It is 2852 of the 2888 `outgoing-args-or-deopt-regs` refusals, and partitioning it is worth ~12 000 KB across the suite. |
| "do not raise the threshold / exclude the class / shrink `region_size`" | **STANDS.** None was done. |

## 8. Repro

```bash
H2J=~/.m2/repository/com/h2database/h2/2.4.240/h2-2.4.240.jar
javac -cp "$H2J" -d /tmp/p probes/TvmProbe.java
cratonvm --java-home "$JDK25" -XX:+UseG1GC --Xmx 2g -c "/tmp/p:$H2J" TvmProbe
```

and the censuses it is built on, each free when unset:

```bash
CRATONVM_G1_DBG_PINS=1       # [g1][PINS] per pinned region: type, occupancy,
                             #            provenance (jit/tlab/nonobj), object sizes
                             # [g1][MOVPIN] what the movable filter kept, and why
CRATONVM_DBG_JIT_ROOTSCAN=1  # [bandpath] a5_sweeps / a5_roots / the published partition
                             # [bandword] each unverifiable word's storage class
                             # [jitpins]  distinct pin addresses with unrew/movable
CRATONVM_DBG_NO_JIT_ROOT_SCAN=1   # the floor. UNSOUND — measurement only.
```

Run the arms ALONE or as CONCURRENT PAIRS, never sequentially against a
differently-loaded host: the failure boundary is 50–330 KB wide on a 2928 KB
threshold and this host's load moves a row by a full region.
