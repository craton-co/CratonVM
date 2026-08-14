# ZGC-specific SIGSEGV cluster — 7 classes crash under ZGC, pass cleanly under G1

**Status:** OPEN, newly found (2026-08-14). Found on Windows running the
**full** 657-class netty suite (not a non-passed subset) in 2 GC variants,
G1 and ZGC, 4 shards each, identical binary built from commit `6a206f689`
(merged fresh to `origin/dev`).

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

## Not yet done

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
