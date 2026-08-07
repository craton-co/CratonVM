# `-XX:+UseG1GC` full-suite run — 3 crashes (1 shared fault site), 4 hangs, 5 fails

**Status:** OPEN (2026-08-06). Full 4548-class Hibernate suite, `-XX:+UseG1GC`,
4 shards, real JDK, JIT on, binary `CratonVM-hib-local-0712-v3` (dev tip
`a526ca521`, includes the JIT dynamic-proxy dispatch fix from the same day).
`PASS=4440/4548` (97.6%). Results:
`apps/hib-suite-runner/runs/categorize-20260806-180134/results.tsv`.

This doc covers every non-passing, non-benign-abort class from that run. Most
are NOT new — they're already-documented residuals of the *default*
(Generational/moving-young) collector recurring unchanged under G1. Three are
genuinely new and G1-specific.

## The headline finding: 3 crashes, 1 shared fault address

`OptimizerConcurrencyUnitTest`, `DefaultCatalogAndSchemaTest`, and
`OffsetDateTimeTest` all SIGSEGV under G1 — none of them crash under the
default collector (`DefaultCatalogAndSchemaTest` was just fixed to 132/132
clean the same day this run started; `OffsetDateTimeTest` normally just runs
slow, never crashes). All three crash reports carry `gc collector: g1` and
**the same faulting instruction**, at or within 3 bytes of the same RVA in
the same exe module base across three independent process launches:

| Class | pc | faulting RVA | thread | incomplete-coverage reason |
|---|---|---:|---|---|
| `OptimizerConcurrencyUnitTest` | `0x...88F044D` | `0x30044D` | `pool-10-thread-3` (worker) | `xt-helper-window-conservative-scan` |
| `DefaultCatalogAndSchemaTest` | `0x...88F044D` | `0x30044D` | `main-vm` (primordial) | `innermost-rbp-belongs-to-unguarded-callee` |
| `OffsetDateTimeTest` | `0x...88F0450` | `0x300450` | `pool-283-thread-1` (worker) | (not captured in the excerpt reviewed) |

`exe module base: 0x00007FF6685F0000` identical across all three reports.
The 3-byte RVA delta between the second and third is consistent with two
adjacent field reads inside the same routine, not two different call sites.

**Read as one defect, not three.** The *specific* "why was GC coverage
incomplete this round" reason differs every time (worker-thread conservative
scan vs. an unguarded-callee frame vs. unknown) — that's expected, coverage
gaps have multiple legitimate causes. What's diagnostic is that whichever
reason fires, the SAME piece of code then faults reading the SAME (or an
adjacent) memory location. That's a G1-specific consumer of incomplete
coverage that doesn't fail safe the way the default collector's equivalent
paths do (compare: the default-collector "Layer 1"/`ClassId(0)` family
degrades to a *controlled* `NoSuchMethodError`/`ClassCastException` — see
`../h2/bug-h2-classid0-stale-address-family.md` — not a native SIGSEGV).
G1's version of whatever reads a possibly-stale/incomplete root here doesn't
have that same guard.

**Not yet root-caused to a specific function** — RVA-to-symbol resolution
needs `CRATONVM_DBG_JIT_NAMES=1` (not set for this run) or offline
symbolization of the exact binary (`cratonvm-hib-fullsuite-20260805-e91a010ed.exe`
build, or rebuild-and-diff since it's the same source). Recommended next
step: reproduce one of the three (`DefaultCatalogAndSchemaTest` is the most
deterministic single-class repro, and the one that regressed from a clean
prior fix) with `CRATONVM_DBG_JIT_NAMES=1` and `CRATONVM_SYMBOLIZE=<the RVA>`
against the same binary to name the function, then read it for what it
assumes about G1 coverage that the default collector's version doesn't.

### Repro
```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_JIT_NAMES=1 <cv> \
  --java-home "<jdk25>" --Xmx 1500m -XX:+UseG1GC @common.args -Dcraton.batch=1 \
  CratonRunner org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
```
Not yet confirmed deterministic — single occurrence per class, one run each.

## HANGs (flat 300s cap)

Four classes exceeded the suite's default 300s per-class timeout under G1:

- `org.hibernate.orm.test.batch.BatchTest` — already a known residual under
  the default collector too (generic throughput margin, not moving-young;
  see `hib-120s-junit-timeout-cluster-20260716.md`'s recurrence notes). Not
  new to G1.
- `org.hibernate.orm.test.type.temporal.OffsetTimeTest` — new. Its temporal
  siblings (`OffsetDateTimeTest`, `ZonedDateTimeTest`) have a timeout floor
  in `class-overrides.tsv` for exactly this shape of slowness under the
  default collector; `OffsetTimeTest` itself never needed one there (it's
  normally in the confirmed-benign-abort set, not slow). Under G1 it hits
  the flat cap — consistent with G1 generally running heavier in this run
  (see Timing below), not necessarily a new correctness issue.
- `org.hibernate.orm.test.hql.ASTParserLoadingTest` — new. Not slow under
  the default collector (see `antlr-native-roots-moving-young-hql-misparse-20260730.md`,
  which documents this class taking ~230-407s either way, sometimes over the
  300s default cap even without G1 — so this may just be the same margin
  case, not G1-specific).
- `org.hibernate.orm.test.query.hql.FunctionTests` — new, no prior doc.

None of the four have `CRATONVM_GC_STATS`/`CRATONVM_DBG` diagnostics captured
in this run (a HANG produces no output beyond `process-died rc=124`) — can't
yet say whether these are G1 running the same workload slower across the
board, or the same moving-young-style coverage tax under a different
collector's accounting. Worth a rerun with a raised per-class timeout to see
if they complete like `OffsetDateTimeTest`/`ZonedDateTimeTest` do under the
default collector.

## FAILs

- `org.hibernate.orm.test.jpa.lock.LockTest` — same already-confirmed
  non-bug as under the default collector (`locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md`):
  a hardcoded 5000ms `assertTimeout` that real HotSpot also misses under host
  load. Not G1-specific, not actionable.
- `org.hibernate.orm.test.type.temporal.ZonedDateTimeTest`,
  `org.hibernate.orm.test.batchfetch.DynamicBatchFetchTest` — both
  already-documented residuals under the default collector (moving-young
  throughput margin / generic throughput margin respectively). Recurring
  unchanged, not G1-specific.
- `org.hibernate.orm.test.sql.exec.SmokeTests` — the well-documented
  `testQueryConcurrency` throughput timeout, most recently retired to
  `../../internal/fixed-suite-bugs/hibernate/smoketests-concurrent-query-throughput-20260723-RETIRED.md`.
  Not G1-specific.
- `org.hibernate.orm.test.stream.basic.JpaStreamTest` — **new, not
  previously seen under the default collector.**
  `testStreamCloseOnTerminalOperation` fails with
  `java.util.NoSuchElementException: No value present` from
  `JpaStreamTest.lambda$runTerminalOperationTests$11` (a `Stream` terminal
  operation expecting a present `Optional`/element that came back empty).
  Not yet root-caused — plausibly related to G1's different
  timing/collection behavior around a lazily-materialized stream/cursor, but
  that's speculation, not evidence. Single occurrence, one run.

## Timing — G1 is heavier than the default collector for this suite

Sum of per-class ms across all 4548 classes: **62,897,431 ms (1048.3 min)**
under G1, 4 shards, vs the default collector's most recent full run
(2026-08-05, 2 shards) which was lighter per-class overall — see
`docs/known-issues/hibernate/README.md` for the running cross-run timing
comparison. Shard count differs between the two runs so wall-clock isn't
directly comparable, but the per-class sum is shard-count-independent and
shows G1 costing more, consistent with G1 being a less-optimized/newer
collector path in this VM (see the code comment at
`vm-cli/src/main.rs:3331` distinguishing G1/ZGC-real/default maturity).

## Related

- `zgc-collector-fullsuite-crash-fails-20260806.md` — the ZGC sibling run,
  same day, same binary. `DefaultCatalogAndSchemaTest` crashes under ZGC too,
  but via a clean `AbstractMethodError` cascade rather than a native SIGSEGV
  — a regression of the same-day JIT proxy-dispatch fix, not the mechanism
  documented here. See that doc before assuming this one's shared-fault-site
  finding explains the ZGC crash too.
- `../h2/bug-h2-classid0-stale-address-family.md` — the default collector's
  equivalent "stale/incomplete coverage" family, which fails *safe* (a
  controlled Java exception) where this G1 defect does not (a native
  SIGSEGV). Worth comparing mechanisms once the G1 fault site is symbolized.
- `defaultcatalogandschematest-jit-proxy-dispatch-abstractmethoderror-FIXED-20260806.md`
  (in `../../internal/fixed-suite-bugs/hibernate/`) — the JIT dynamic-proxy
  fix that made `DefaultCatalogAndSchemaTest` clean under the default
  collector the same day this G1 run started. This doc's G1 crash on the
  same class is a **different** mechanism (native SIGSEGV vs. the fixed
  bug's clean `AbstractMethodError`) — do not conflate the two.
