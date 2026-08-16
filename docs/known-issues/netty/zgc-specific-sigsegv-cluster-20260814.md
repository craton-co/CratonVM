# ZGC-specific SIGSEGV cluster — 7 classes crash under ZGC, pass cleanly under G1

**Status:** **7/7 of the cluster FIXED and re-confirmed on current `dev`
(2026-08-15).** One class remains open and is now the whole of this page's
residual: `io.netty.util.ResourceLeakDetectorTest`, which still SIGSEGVs under
ZGC. Four root-caused defects landed between `6a206f689` and `caf25c3d1`; the
"new regression" this page recorded on 2026-08-14 was one of them and is fixed.
See "Current validation" below for the table that supersedes both earlier
fix-validation sections.

**Update, later on 2026-08-15.** That residual turned out to be TWO defects
sharing one signature, and both got their own pages:

* the corpse-read page — a compiled frame holding a `DefaultResourceLeak`
  pointer in a register across a slide. ZGC never read
  `gc_quiescence::is_active()`, the flag `gen_heap` (9 sites) and `g1` (6 sites)
  use to decline relocation while a JIT frame is live. **Fixed, verified, and
  the page is retired** — its three residuals were closed out the same evening,
  one of which turned up a VM-wide soft-reference defect (soft references were
  never cleared on any collector) that is fixed with it.
* `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` — the collector
  faulting inside its own post-slide rewrite pass, reproducible with `--nojit`.
  The straddler fix on that page is real and landed. **The crash is not gone.**

**Correction, same evening: the cluster is 7/8, not 8/8.** The "12/12 `--nojit`
clean" reading that closed it was a sampling artefact — a ~1-in-10 event and
twelve draws. Re-verified against a binary built from **pristine `origin/dev`**:
`--nojit` still SIGSEGVs, with the walkability guard still reporting registered
bases whose headers decode as String data. The JIT-on arm is genuinely clean
(5/5, plus 10 earlier), but that is partly because the corpse-read fix declines
relocation on 64 of 68 cycles under a JIT-heavy load — **the first fix masks the
second defect rather than being independent of it.** See the reopened page for
the current evidence and the one hypothesis already eliminated.

The residual `failed=1` is GC-independent — but it is *not* the test owner's
problem, which this page also got wrong. The failing test is
`testConcurrentUsage`, which HotSpot passes; HotSpot fails the other two. See
`resourceleakdetector-concurrentusage-timeout-20260815.md`.

Two corrections to what this page records about that class: it crashes ~60% of
the time, not on every run (measured 6/10, `--nojit` 2/10, `RELOCATE=0` 0/10),
and two further ZGC defects were found and fixed on the way there — the
reference-processor tables and the resurrected-finalizer list, both holding
pre-slide addresses.

Originally found on Windows running the **full** 657-class netty suite (not a
non-passed subset) in 2 GC variants, G1 and ZGC, 4 shards each, identical
binary built from commit `6a206f689` (merged fresh to `origin/dev`).

## Symptom

9 classes report `CRASH` (SIGSEGV, `rc=139`... actually a plain
"Segmentation fault", no `timeout` wrapper rc, harness reports it as
`process-died`) under `-XX:+UseZGC`. Of those 9, **7 pass cleanly (0
failures) under `-XX:+UseG1GC`** — same binary, same classpath, same
`--shards 4`, only the GC flag differs:

| class | ZGC | G1 |
|---|---|---|
| `handler.codec.http.websocketx.WebSocketClientHandshaker00Test` | **CRASH** | PASS 20/20 |
| `handler.codec.http2.Http2FrameRoundtripTest` | **CRASH** | PASS 28/28 |
| `handler.codec.http.HttpObjectAggregatorTest` | **CRASH** | PASS 26/26 |
| `handler.codec.http2.WeightedFairQueueByteDistributorTest` | **CRASH** | PASS 23/23 |
| `util.internal.BoundedInputStreamTest` | **CRASH** | PASS 101/101 |
| `handler.codec.compression.LzmaFrameEncoderTest` | **CRASH** | PASS 6/6 |
| `handler.codec.http2.Http2StreamFrameToHttpObjectCodecTest` | **CRASH** | PASS 43/43 |

The other 2 of the 9 are not part of this cluster — they have their own
pre-existing, GC-independent issues and are noted for completeness, not as
part of the ZGC-specific pattern:
- `buffer.AdaptiveByteBufAllocatorGrowthTest` — HANGs under G1 too (the
  already-documented throughput gap, see
  `adaptive-bytebuf-allocator-throughput-20260812.md`); the ZGC crash here
  may or may not be the same underlying issue as the other 7, not
  determined.
- `handler.ssl.OpenSslPrivateKeyMethodTest` — FAILs 0/24 under G1 (a
  genuine, separate defect — this class no longer shows the total
  test-discovery miss recorded in
  `ssl-suite-test-discovery-undercounts-20260813.md`, worth a follow-up
  note there since that page's premise may now be stale for this one
  class), crashes under ZGC. Given it involves `OpenSsl`-native code and
  key material, plausibly a different failure mode than the other 7's
  likely-buffer/copy-shaped defects.

## Why this is not a coincidence

This full-suite run is deliberately structured to make G1-vs-ZGC the
*only* variable: identical binary (`bin/cratonvm-netty-g1.exe` and
`bin/cratonvm-netty-zgc.exe` are byte-identical copies of the same release
build), identical `testlist.txt`, identical `--shards 4`, launched back to
back. 7 classes crashing under exactly one collector and passing outright
under the other, with the classes spanning unrelated subsystems (WebSocket
handshaking, HTTP2 frame round-tripping, HTTP object aggregation, LZMA
compression, a generic `InputStream` wrapper) rather than one feature
area, is the signature of a **collector-level defect** — something ZGC
does to memory (moving/compacting, barrier insertion, page selection) that
corrupts state these classes happen to touch, not a defect in each
individual class's code path.

## A concrete lead, not yet confirmed as the cause

> **RESOLVED 2026-08-15 — the lead was right about the area and wrong about the
> direction.** `6a206f689` is the FIX for the unselected-page slide, not the
> cause of these crashes; the pre-merge binary predates compaction being
> default-on at all, which is why it did not crash. The section below is kept
> because its reasoning — "live objects getting overwritten by survivor data on
> a page ZGC didn't intend to touch" — is an exact description of a real defect
> that commit repaired, and because three further defects of the same family
> were found by taking it seriously. See "Current validation".

The `dev` merge that produced this binary (`6a206f689`, 136 commits ahead
of the previous local build) includes:

```
fix(zgc): compaction slid survivors over live objects on an UNSELECTED page
```

That commit's own description is exactly the shape of bug that would
produce scattered, workload-independent SIGSEGVs after a ZGC compaction
cycle — live objects getting overwritten by survivor data on a page ZGC
didn't intend to touch. Worth checking first whether this fix is complete,
or whether it's *this* commit that introduced the crashes (i.e. check
whether the previous ZGC binary, pre-merge, crashed on these same 7
classes or not) before assuming it's a pre-existing, unrelated defect.

## Cross-app corroboration: the same G1-clean/ZGC-crashes pattern in hibernate-reactive

Run the same day, same binary lineage (`6a206f689`, Windows), same
methodology (full suite, both GC variants, 4 shards) — hibernate-reactive
shows the identical shape, at higher absolute counts:

| variant | CRASH | PASS | HANG | FAIL | NOTESTS |
|---|---|---|---|---|---|
| `-XX:+UseG1GC` | **0** | 11 | 17 | 177 | 44 |
| `-XX:+UseZGC` | **15** | 11 | 16 | 163 | 44 |

Zero crashes under G1, 15 under ZGC, on a completely different application
(reactive Hibernate/Vert.x, not netty's own test suite) sharing nothing
but the CratonVM binary and its ZGC collector:

```
org.hibernate.reactive.ImplicitSoftDeleteTests
org.hibernate.reactive.ManyToOneMergeTest
org.hibernate.reactive.BeforeExecutionIdGeneratorTypeTest
org.hibernate.reactive.EagerElementCollectionForBasicTypeSetTest
org.hibernate.reactive.EagerOrderedElementCollectionForEmbeddableTypeListTest
org.hibernate.reactive.FindAfterFlushTest
org.hibernate.reactive.QueryTest
org.hibernate.reactive.OneToManyMergeTest
org.hibernate.reactive.types.JavaTypesArrayTest
org.hibernate.reactive.EagerElementCollectionForBasicTypeListTest
org.hibernate.reactive.EagerOneToManyAssociationTest
org.hibernate.reactive.HQLQueryParameterNamedLimitTest
org.hibernate.reactive.HQLQueryTest
org.hibernate.reactive.ReactiveMultitenantNoResolverTest
org.hibernate.reactive.ReactiveStatelessSessionTest
```

Two independent applications, two disjoint sets of crashing classes (no
overlap in name or subsystem — collections, HQL queries, merge cascades,
multitenancy, stateless sessions here vs. WebSocket/HTTP2/compression in
netty), same collector-specific signature. This makes a **CratonVM-wide
ZGC defect** (not an application-specific one) the much more likely
reading than 22 independent per-class bugs — raises confidence in the
compaction-commit lead above considerably. hibernate-reactive's PASS rate
here (11/249) is dominated by an unrelated, already-documented
Windows/Docker-Desktop environmental issue
(`testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`), not
part of this finding — only the CRASH column is relevant to this doc.

## Fix validation — hibernate-reactive, same-day, candidate fix binary

Re-ran the identical full-suite methodology (249 classes, G1 + ZGC, 4
shards each, `TESTCONTAINERS_RYUK_DISABLED=true`, cleanup sweep) with
`C:\craton\cratonvm-zgcfix-20260814.exe` in place of the earlier
`6a206f689` build:

| variant | CRASH before | CRASH after fix | PASS | HANG | FAIL | NOTESTS |
|---|---|---|---|---|---|---|
| G1 | 0 | **0** | 11 | 15 | 179 | 44 |
| ZGC | 15 | **2** | 11 | 15 | 177 | 44 |

The 2 remaining ZGC crashes are a **subset** of the original 15 — both
were crashing before too (`BeforeExecutionIdGeneratorTypeTest`,
`EagerOneToManyAssociationTest`), no new crash appeared anywhere. **13 of
15 fixed, 0 regressions.** PASS/HANG/FAIL/NOTESTS counts are essentially
unchanged from the pre-fix run (dominated by the separate, unrelated
Windows/Docker-Desktop environmental issue — see
`testcontainers-jackson-jit-stall-blocks-eventloop-20260812.md`), so this
fix is cleanly isolated to the crash path and doesn't touch anything else
observable at this level.

## Fix validation — netty, same day, same fix binary

Re-ran the original 9 netty crash classes directly (isolated, `--shards
1`, both G1 and ZGC) against `cratonvm-zgcfix-20260814.exe`:

| class | ZGC before | ZGC after fix | G1 after fix |
|---|---|---|---|
| `WebSocketClientHandshaker00Test` | CRASH | **PASS** | PASS |
| `Http2FrameRoundtripTest` | CRASH | **PASS** | PASS |
| `HttpObjectAggregatorTest` | CRASH | **PASS** | PASS |
| `WeightedFairQueueByteDistributorTest` | CRASH | **PASS** | PASS |
| `BoundedInputStreamTest` | CRASH | **PASS** | PASS |
| `LzmaFrameEncoderTest` | CRASH | **PASS** | PASS |
| `Http2StreamFrameToHttpObjectCodecTest` | CRASH | **PASS** | PASS |
| `AdaptiveByteBufAllocatorGrowthTest` | CRASH | HANG (unrelated, pre-existing throughput issue) | HANG (same) |
| `OpenSslPrivateKeyMethodTest` | CRASH | FAIL (unrelated, pre-existing) | FAIL (same) |

**All 7 of the pure ZGC-only cluster now pass cleanly on both collectors.**
The other 2 revert to their own separate, already-known non-crash issues
(unaffected by this fix, unrelated to it).

### But: one NEW crash appeared, ZGC-specific, not present before this fix

`io.netty.util.ResourceLeakDetectorTest` was checked as a control (it
crashed in the earlier full-suite non-passed rerun with this fix binary,
appearing where it never had before):

| | pre-fix (`6a206f689`) | post-fix, ZGC | post-fix, G1 |
|---|---|---|---|
| `ResourceLeakDetectorTest` | FAIL (2 ok/1 failed) | **CRASH** | FAIL (2 ok/1 failed, same as before) |

Confirmed in isolation (`--shards 1`), not a contention artifact. Under
G1 it fails the exact same way it did pre-fix (`2 ok/1 failed`) — the fix
changed nothing there. Under ZGC it now SIGSEGVs where it used to just
fail one assertion. **This fix candidate resolved 7 real crashes and
introduced 1 new one, GC-specific in the same way as the ones it fixed.**
Worth flagging loudly to whoever owns this: not a clean fix, net-positive
but not zero-regression.

## Current validation — 2026-08-15, `dev` @ `caf25c3d1`

Every class from this page, isolated (one VM per class), **both** collectors,
one binary, only the GC flag varied:

| class | ZGC | G1 |
|---|---|---|
| `WebSocketClientHandshaker00Test` | **20/20** | 20/20 |
| `Http2FrameRoundtripTest` | **28/28** | 28/28 |
| `HttpObjectAggregatorTest` | **26/26** | 26/26 |
| `WeightedFairQueueByteDistributorTest` | **23/23** | 23/23 |
| `BoundedInputStreamTest` | **101/101** | 101/101 |
| `LzmaFrameEncoderTest` | **6/6** | 6/6 |
| `Http2StreamFrameToHttpObjectCodecTest` | **43/43** | 43/43 |
| `ResourceLeakDetectorTest` | **CRASH** | ok=0 failed=3 |

**Correction to this page's earlier reading of the last row.** It records
`ResourceLeakDetectorTest` as `FAIL (2 ok/1 failed)` under G1, pre- and
post-fix, and infers that "the fix changed nothing there". On current `dev` G1
gives **0 ok / 3 failed**, and the reason is visible in the log:

```
NoSuchMethodError io/netty/util/ResourceLeakDetectorTest$DefaultResource.close(Ljava/lang/Object;)Z
```

That is a **GC-independent defect of its own** and it has drifted since this
page was written. So the row is two problems stacked, and they want separate
owners: a missing method that fails the class on every collector, and a
ZGC-compaction crash on top of it.

### The four defects that closed the 7

All four are cases of the same thing — code that was correct while the
collector never moved an object, and stopped being correct on 2026-08-13 when
compaction went default-on:

1. **The slide crossed unselected pages.** `ZRelocationSet::select` ranks by
   garbage ratio and returns a NON-CONTIGUOUS page set; the slide marched one
   cursor and memmoved survivors over live objects on the dense pages between.
   (`6a206f689`, the commit this page nominated as the lead — it was the fix,
   not the cause.)
2. **`pin_critical_region` pinned nothing under ZGC.** The copy-back at
   `ReleasePrimitiveArrayCritical` re-resolves the Get-time address; move the
   array and it writes over whatever now occupies it.
3. **The reference-processing guard tested the pre-move address** while the
   write used the post-move one, so every moved `Reference` was judged dead —
   no `WeakReference` delivery, no `Cleaner`.
4. **`prune_dead` and `remap_after_gc` collided.** Survivors slide *down* into
   space vacated by dead objects, so a dead base is often a live base
   afterwards. Measured on a four-page fixture: 169 live objects' monitors
   freed per collection in the original order. **This is the "new regression"
   this page flagged** — the first attempt screened the dead list instead of
   reordering, which stopped the free and left the dead tenant's monitor for
   the survivor to inherit (83 per collection). Prune-then-remap fixes both.

A fifth landed alongside: ZGC never consumed `pinned_jit_roots_snapshot()`,
which G1 has honoured since 2026-08-11. Conservative JIT roots are
over-approximate (a `long` can look like a root), so a moving collector must
pin rather than relocate them. Real gap, fixed — but measured **not** to be
this page's residual crash.

### What is known about the remaining crash

`ResourceLeakDetectorTest`, ZGC only, and every claim here is a measurement:

| question | answer | how |
|---|---|---|
| Is compaction the trigger? | **Yes** | `CRATONVM_ZGC_RELOCATE=0` → 5/5 clean; on → 0/15 |
| Does the slide miss a reference slot? | **No** | `missed_rewrites=0` |
| Do stale words alias live objects? | **No** | `aliasing_a_survivor=0` |
| What does the crash look like? | a **zeroed** header | `zgc real: field index OOB index=N num_slots=0` |

So the heap's slot graph is internally consistent after a slide, and the holder
of the stale address is **outside** it. `compact_low_to` zeroes the vacated span
deliberately (so a conservative scan cannot resurrect a corpse), which is why
the reader finds a well-formed ALL-ZERO object rather than a wild pointer —
`num_slots=0`, then a walk off the end of a zero-length object.

**Three hypotheses were eliminated by measurement, not argument:** an
incomplete rewrite (this page's own leading theory), compaction-unenumerable
objects, and stale-word aliasing. `--nojit` reduces but does not remove the
crash, so JIT frames are one holder and not the only one.

**Two instruments were built for this and are worth reusing:**
`CRATONVM_DBG_ZGC_VERIFY_SLIDE=1` classifies every post-slide dangling slot as
missed-rewrite / never-a-base / aliasing-a-survivor, and ZGC now honours
`CRATONVM_DBG_GC_STRESS=<bytes>` — it ignored that flag entirely before, so any
repro copied from a handoff page silently ran an ordinary workload on the
default collector.

**A measurement trap recorded so the next person does not repeat it:** the
`field index OOB` warning count is NOT a severity metric. A run that dies early
logs fewer warnings, so comparing 1250-against-1 between two arms measures how
long each survived, not how broken each is. Use completion rate over 15+ reps;
5 reps cannot separate 0/5 from 2/5 on this class.

### Next step

The holder is outside the heap slot graph and survives `--nojit`.
`CRATONVM_DBG_ROOT_SOURCE=1` attributes an address to a named root source,
which turns "something holds it" into "*this* holds it". Pair it with the first
`num_slots=0` to capture the address, then ask the registry who contributed it.

## Update 2026-08-15 — `ResourceLeakDetectorTest` crash moved collectors again

Azure host `azureuser@20.80.105.49`, worktree `/data/cvm-netty-gcrun-20260812`,
merged fresh to `6dc9ebffe` (13 commits past `caf25c3d1`, including
substantial GC-internals changes: `gen_heap.rs`, `region.rs`,
`evac_pool.rs` (new), `narrow_oop.rs`). Same 43-class non-passed list, all
3 GC variants, 1 shard each:

| variant | CRASH | PASS | HANG | FAIL | ABORTED | NOTESTS |
|---|---|---|---|---|---|---|
| default (Generational) | **1** (`ResourceLeakDetectorTest`) | 10 | 20 | 8 | 2 | 2 |
| G1 | 0 | 10 | 21 | 8 | 2 | 2 |
| ZGC | **0** | 10 | 20 | 9 | 2 | 2 |

**ZGC is now clean** — 0 crashes, matching G1's already-established 0.
But `ResourceLeakDetectorTest`, the one new crash the previous fix
binary introduced (2026-08-14, crashing under ZGC that day), now crashes
under **default/Generational instead**, on this newer build. It has now
been observed crashing under two different collectors across two
different builds, and has never crashed under G1 in either. This argues
against it being a fixed per-collector defect (like the original 7-class
cluster clearly was — same 7 classes, same collector, every time) and
more likely something timing/allocation-pattern-sensitive that different
collectors happen to trigger differently. Not yet isolated in isolation
(`--shards 1` was already used here, so this isn't shard contention) —
worth a dedicated, focused investigation of this one class rather than
folding it into the original cluster's narrative.

## Not yet done

> **Updated 2026-08-15.** Of the four items below, three are now done: isolated
> `--shards 1` reproduction (every class in the table above is one VM per
> class), the bisect against the compaction commit (done, and it inverted — see
> the lead section), and CratonVM-side crash diagnostics (the `num_slots=0`
> signature and the slide verifier). The HotSpot cross-check is still not run
> and is still not needed to establish the finding.

- No stack traces, crash dumps, or `gdb`/WinDbg analysis — the harness
  only records "Segmentation fault" from the shell, no CratonVM-side
  crash diagnostics were captured.
- Not bisected against the specific ZGC compaction commit above — don't
  assume it's the cause without checking the pre-commit binary.
- HotSpot cross-check not run — unnecessary to establish this is
  CratonVM-specific (the G1-vs-ZGC comparison on the same binary already
  proves that), but would confirm these are otherwise-healthy classes.
- Individual isolated (`--shards 1`) reproduction not done for any of the
  7 — all evidence here is from the 4-shard full-suite run. Worth
  confirming at least one reproduces standalone before deep investigation,
  per this session's established discipline for HANG-vs-real-defect
  triage (though a SIGSEGV is much less likely to be a shard-contention
  artifact than a timeout-based HANG).

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.codec.http.websocketx.WebSocketClientHandshaker00Test \
  io.netty.handler.codec.http2.Http2FrameRoundtripTest \
  io.netty.handler.codec.http.HttpObjectAggregatorTest \
  io.netty.handler.codec.http2.WeightedFairQueueByteDistributorTest \
  io.netty.util.internal.BoundedInputStreamTest \
  io.netty.handler.codec.compression.LzmaFrameEncoderTest \
  io.netty.handler.codec.http2.Http2StreamFrameToHttpObjectCodecTest > /tmp/zgc-crash.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/zgc-crash.txt --gc zgc --shards 1 --out /tmp/repro-zgc
CV_BIN=bin/cratonvm-netty-g1.exe  bash run-netty-suite.sh --list /tmp/zgc-crash.txt --gc g1  --shards 1 --out /tmp/repro-g1
```

## Related

- `docs/known-issues/netty/adaptive-bytebuf-allocator-throughput-20260812.md`
  — `AdaptiveByteBufAllocatorGrowthTest`'s separate, already-documented
  throughput HANG (present under G1 too, not part of this cluster).
- `docs/known-issues/netty/ssl-suite-test-discovery-undercounts-20260813.md`
  — `OpenSslPrivateKeyMethodTest` was a total discovery-miss (0 tests)
  there; under G1 in this run it discovers 24 and fails all of them —
  worth reconciling whether that's the same environment shift noted
  elsewhere in this session or a further change.
