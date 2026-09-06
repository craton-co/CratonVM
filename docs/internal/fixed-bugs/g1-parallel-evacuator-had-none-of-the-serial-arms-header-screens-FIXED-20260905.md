# The parallel G1 evacuator had NONE of the serial arm's header screens — and it is the default arm

**Date:** 2026-09-05 **Status:** FIXED (`CRATONVM_G1_PARALLEL_EVAC_SCREEN`, default ON)
**Closes:** the public page
`g1-evac-forwarding-assert-and-three-sigsegv-clusters-20260905` (15 CRASH
classes, G1 arm only, 0 in the same run's Generational or ZGC arms).

## One-line root cause

`SharedEvac::process_object` and `SharedEvac::seed_source_region` decided
whether to follow a reference by asking `lookup_region_for_addr` + CSet
membership **and nothing else**, and `parallel_evacuate`'s Phase-1 root seed
asked only CSet membership. Their SERIAL twins have screened every candidate
with `evacuation_candidate_is_an_object` since 2026-08-26, clamped every
element walk with `holder_walkable_slots` since the same day, and refused a
root that is not an object start since 2026-09-02. **Parallel evacuation is the
default arm** (`CRATONVM_G1_PARALLEL_EVAC`, on unless `0`), so every one of
those hardening passes landed on the code that does not run.

An interior or misaligned word therefore reached `SharedEvac::evacuate`, which
dereferences it as an `ObjectHeader` and — on the evacuation-failure arm —
hands it straight to `ObjectHeader::make_forwarded`, whose first assert is
*"forwarding target must have its low 2 bits clear"*. That assert is a panic in
a `g1-evac-N` worker, which `RetireOnExit` correctly converts into a `main-vm`
abort. The whole process dies.

## Which of the two candidate targets it was

The public page left this open — "the self-forward candidate `old` … or the
fresh destination `new_addr` … this investigation did not have the means to
determine which". It is **`old`**, and the other arm can be excluded by
reading, not by measuring: `tlab_alloc` returns `(tlab.base + tlab.offset + 7)
& !7`, so `new_addr` is 8-aligned by construction and can never trip the
low-2-bits assert. Only the self-forward arm passes a caller-supplied address
to `make_forwarded`.

That is confirmed directly by the reproduction below, whose log carries the
producer three lines above the panic:

```
[g1] IMPLAUSIBLE legacy header at root-pin-scan (#1): obj=0x2485b560900
  class_id=1532365136 kind=Object num_slots=584 mark=0x000002485bab3b90
  claims=0x2490 bytes source=r23/Survivor/off=0xc0900/... grid=INTERIOR
  of=0xc08e0 delta=0x20 size=0x70 cid=1604 kind=Object idx=8588
  bytes[... >>0xc0900=0x000002485b560950<< ...]
thread 'g1-evac-3' panicked at types\src\heap_types.rs:1529:9:
forwarding target must have its low 2 bits clear (>= 4-byte aligned)
```

`class_id=1532365136` is `0x5B560950` — the low half of the heap pointer
printed two fields later. This is the 2026-09-02 fabrication pattern verbatim:
an INTERIOR address read as a header, its class id and `num_slots` cut out of a
pointer's two halves.

## Reproduction (Windows, ~140 s, no Azure needed)

The public page reported these classes from a 640-class Azure shard. They
reproduce locally once the classpath is built **jar-first** — the same shape
the Linux suite uses (`output/build/lib/*.jar`) rather than the Windows
suite's exploded `output/classes`:

```
cratonvm.exe --java-home <jdk25> --Xmx 2g -XX:+UseG1GC ... \
  -c <jars-first classpath> org.junit.runner.JUnitCore \
  org.apache.jasper.compiler.TestEncodingDetector
```

Both locally-reproducing classes hit the SAME assert, on different threads —
`TestEncodingDetector` on `g1-evac-3` (a worker, i.e. `process_object`) and
`TestJspDocumentParser` on `main-vm` (the driver, i.e. the Phase-1/2 seed).
That pair is itself the finding: the defect is not "a worker race", it is a
missing screen absent from **both** the driver and the worker path of one arm.

## Clusters A/B/C — the same defect, silently

The public page hypothesised that the ten SIGSEGVs in `runtime_var_os`,
`VersionCache::find` and `plan_object_alloc` were "a *silent* variant of the
same underlying defect that doesn't happen to trip the loud assert", and could
not establish it. The mechanism is now explicit, and it does not need a race:

* `process_object`'s array arm walked `0..header.array_length()` with **no
  region clamp**, and its flat arm used
  `for_each_flat_object_reference_trusting_header` — the entry point whose own
  doc says *"It trusts its caller"*. A fabricated holder claims a length its
  region cannot hold, so the walk leaves the object and reads its neighbours;
* and the walk WRITES. Every arm that evacuates also does
  `std::ptr::write(slot_ptr, new_ptr)` / `write_flat_object_reference`. So the
  runaway does not merely read past the holder — it stamps forwarding
  addresses over whatever follows it in the region.

An address whose bytes happen to decode as a plausible header is copied and
followed with no assert anywhere; the fault then lands in whichever code next
reads the corrupted memory, which is exactly why the three faulting symbols are
an env-var reader, a field-layout cache and an alloc-shape planner — none of
them GC code, and none of them individually unsafe. `scan_and_evacuate_refs`'s
own comment already made this argument about the serial arm: *"an unclamped
walk here does not merely read past the holder, it rewrites `Value` cells past
the holder with forwarded pointers, i.e. it corrupts whatever objects follow it
in the region."* The clamp it describes was never applied here.

The page's Jasper-family enrichment (5 of 11 `jasper.compiler` classes, ~19x
the shard rate) needs no separate explanation: JSP compilation is the
allocation-heaviest workload in the suite, so it takes the most young pauses
and gets the most draws at a rare bad candidate.

## The fix

`RegionView` — one enum, two constructors — is what lets a single body of each
screen serve both arms. The parallel arm cannot form a `&[G1Region]`: the
driver drops its guard-derived borrow for the whole dispatch and workers reach
regions through `RegionsBase`, so a whole-table slice there would assert an
exclusivity the disjointness discipline does not have. Copying the screens
instead would have been a twin pair, which is the shape this file's history
already has one defect from (G1-9, a divergence between these very two walks).

Applied, all gated on `CRATONVM_G1_PARALLEL_EVAC_SCREEN` (default ON):

| site | added |
|---|---|
| `parallel_evacuate` Phase-1 roots | `note_root_object_plausibility_view` + skip, mirroring `young_collection` / `mixed_collection` |
| `process_object` array arm | region clamp + `evacuation_candidate_is_an_object_view` |
| `process_object` flat arm | `for_each_flat_object_reference_capped` + the same candidate screen |
| `seed_source_region`, both arms | the same two |
| `SharedEvac::evacuate` | last-ditch refusal of a null/misaligned candidate, counted in `PARALLEL_EVAC_UNALIGNED_CANDIDATE` |

**The clamp uses `HolderBound::RegionEnd`, not `Cursor`, and that is
load-bearing.** The serial evacuator publishes a destination region's cursor as
it copies, so a serial to-space holder is always below it. The parallel one
carves per-worker TLABs and writes the cursor back only in `retire_tlab`, so
for the whole dispatch a freshly-copied holder sits ABOVE the published cursor.
Clamping the parallel arm to the cursor would have returned 0 for every object
it copies — not a hardening, a closure that follows nothing.

`verify_no_dangling_into_cset` also stops counting a **misaligned** word as a
dangling reference. Every object start here is 8-aligned, so such a word cannot
be one; it is the primitive or stale slot the evacuator has just declined to
follow, and counting a deliberate refusal as "incomplete remembered set => UAF"
makes the verifier report the fix as the defect. The screen is alignment ONLY —
`candidate_header_is_plausible` would make the check vacuous, because that walk
runs after Phase 5, when every CSet region is already `Free`.

## The `make_forwarded` census, re-run (the page asked for this)

Seven call sites, none of them the raw-cast defect, and only two of them take a
caller-supplied address:

| site | target | screened by |
|---|---|---|
| `g1.rs:1341` parallel self-forward | `old_ptr` | **the new screens + alignment guard** |
| `g1.rs:1398` parallel copy | `tlab_alloc` result | 8-aligned by construction |
| `g1.rs:8501` serial self-forward | `old_addr` | caller contract (root / ref-slot screens) |
| `g1.rs:8610` serial copy | `alloc_in_type_locked` result | 8-aligned by construction |
| `gen_evac.rs:1111` | gen allocator result | 8-aligned by construction |
| `gen_evac.rs:1664` | test fixture | n/a |
| `zgc.rs:8615` | a slide destination | an allocation base |

So the "exactly two call sites" claim the Aug-7 page closed on is stale in
count — and it was always the wrong question. The number that matters is **how
many sites pass an address the collector did not itself allocate**: two, and
one of them was unguarded.

## Verification

`gc/src/g1.rs::the_parallel_evacuator_refuses_a_misaligned_reference_slot` — a
reference array whose ninth element holds `good[0] + 3` (inside a live CSet
region, past its header, not 8-aligned). Pre-fix the pause aborts; post-fix it
completes, every well-formed element still moves exactly once, and the refusal
is counted. `cargo test -p cratonvm-gc`: 1858 pass, 0 fail.

### Measurements

#### 1. The kill-switch A/B the public page asked for, at n=6

Azure, ONE binary, arms interleaved per repetition, `-XX:+UseG1GC`, the Linux
suite's own classpath and flags. The only difference between arms is
`CRATONVM_G1_PARALLEL_EVAC_SCREEN=0`.
`org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml`:

| arm | CRASH | PASS | HANG |
|---|---:|---:|---:|
| `SCREEN=0` (pre-fix walks) | **3** | 3 | 0 |
| default (screens armed) | **0** | 5 | 1 |

Crash walls 107.0 / 111.9 / 118.2 s; pass walls 144.9-167.3 s (off) and
135.5-164.0 s (on). The single `on` HANG is a 600 s cap hit at host load 18 on
a shared box, not a crash — see the caveat below.

#### 2. The same A/B across all 15 classes the public page names, n=1

Same binary, same arms, 600 s cap:

| arm | CRASH | PASS | HANG |
|---|---:|---:|---:|
| `SCREEN=0` | **4** | 10 | 1 |
| default | **0** | 13 | 2 |

The four that crashed with the screens off — `TestJspDocumentParser`,
`TestELInterpreterTagSetters`, `TestMapperWebapps`,
`TestHostConfigAutomaticDeploymentXmlExternalDirXml` — all PASS with them on.
The two `on` HANGs are `TestGenerator` (HANG on BOTH arms; the census's known
868 s class against a 600 s cap) and `TestCompiler`, which is answered below.

#### 3. Windows, cross-binary, all 15 classes

dev tip `355659d00` vs this branch, jar-first classpath, 480 s cap:

| binary | CRASH | PASS | HANG | other |
|---|---:|---:|---:|---|
| dev tip | **5** | 6 | 4 | — |
| this branch | **0** | 6 | 7 | 1 OOM, 1 FAIL |

Three of the five crash classes become HANG only because they no longer die
early and then meet the 480 s cap — Windows runs these Jasper classes 2-3x
slower than Azure does (`TestHostConfigAutomaticDeploymentXmlExternalWarXml`:
207 s here, 86 s there), and the census already records that they need up to
868 s. `TestFormAuthenticatorB` and `TestHostConfigAutomaticDeploymentDeleteB`
go CRASH → PASS outright.

#### 4. Cost

Not free, not large, and not separable from this host's noise.
`TestCompiler`, Azure, 3 interleaved repetitions per arm, all six PASS:

| arm | walls (s) | median |
|---|---|---:|
| `SCREEN=0` | 223.5, 203.0, 231.0 | 223.5 |
| default | 239.9, 202.2, 256.4 | 239.9 |

Median +7.3%, ranges overlapping (the fastest run of all six is an `on` run).
Across the ten classes of measurement 2 that PASS on both arms the on/off wall
ratio ranges 0.89-1.42 with no sign, which on a box carrying load 10-18 and
other sessions' builds is noise, not a measurement. What can be said from the
code is the shape of the cost: one extra region-table lookup and two tag-byte
reads per CSet-bound reference, on a cache line `evacuate` is about to read
anyway — and the serial arm has paid exactly this since 2026-08-26.

**Do not read a timing arm from this host as a number.** It is shared, it was
carrying two other sessions' cargo builds throughout, and
[[reference_a_contended_host_hid_a_defect_that_reproduces_9_of_9]] is on record
about what that does.

## What is NOT closed by this

**The classes are still slow, and one of them is still unstable.** Removing the
crash does not make `TestGenerator` fit in 600 s (it needs ~868 s — the census
records that, and it HANGS on both arms), and it does not make
`TestHostConfigAutomaticDeploymentXmlExternalWarXml` deterministic: one of its
six screened runs hit the 600 s cap at host load 18, and on Windows two of six
screened runs failed with a heap-corruption-shaped `ClassCastException`
(`class <unknown> cannot be cast to class java.lang.String`, and `class
java.lang.Object cannot be cast to class …FrameworkMethod`) where the
unscreened arm passed twice. That is a REAL residual and it is stated here
rather than smoothed over: the screens refuse to FOLLOW a bad candidate, they
do not explain where the bad candidate comes from, and the public page's own
"silent variant" hypothesis predicts exactly such a remainder. A three-arm run
(screens off + module fix off / screens on + module fix off / both on) puts it
on the G1 side, not on this branch's classloading change — it reproduces with
`CRATONVM_CLASSPATH_JAR_UNNAMED_MODULE=0`.

`org.apache.jasper.compiler.TestCompiler` HANGS on the Windows host on **both**
arms, at the same point (immediately after
`ContextConfig.getDefaultWebXmlFragment`), 480 s timeout, while on Azure it
PASSES on both arms in ~200-260 s. That is a Windows-side throughput residual —
this fix neither causes nor cures it — and it is not the crash this page is
about.
