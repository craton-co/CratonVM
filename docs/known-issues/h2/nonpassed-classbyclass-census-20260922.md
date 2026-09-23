# H2 suite — class-by-class non-passed census, 2026-09-22

| | |
|---|---|
| **Measured** | 2026-09-22, Azure Linux, commit `1c8a3404c`, `--jdk-only`-default, real JDK 25, all defaults, default collector (not the three-collector sweep the baseline used), `--category all`, one shard |
| **Census** | **PASS=166 / FAIL=38 / HANG=14** (218 classes) |
| **Baseline** | [`nonpassed-40-census-20260818.md`](nonpassed-40-census-20260818.md), 2026-08-18, HotSpot-controlled: of the 62-class non-passed union at the time, 40 failed on all three collectors, and of those **18 also fail on stock HotSpot** (not a CratonVM bug), **20 are wall-clock only** (HotSpot passes fast, CratonVM is just much slower on the same class and hits the cap), and **only 2 are real correctness defects**. |
| **Source** | `apps/h2database-suite-runner/out/classbyclass-default-20260922-jit-real-all-20260922-042322/results.tsv` (columns: `idx class status rc ms tests mode log note`) |

**Read this page as a cross-reference against the baseline, not a fresh adjudication.** This run did not re-run a HotSpot control arm — it only ran CratonVM, once, at one shard. Cross-checking today's 52 non-passed class names against the baseline's careful three-collector-plus-HotSpot study from a month ago shows **most of today's rows are already-understood, not new**:

## Already adjudicated by the baseline — 30 of today's 52

### Not a CratonVM bug — HotSpot fails these too (baseline §3), 12 recur today

`TestFunctions`, `TestLob`, `TestOutOfMemory`, `TestSubqueryPerformanceOnLazyExecutionMode`, `TestRecoverKillLoop`, `TestJoin`, `TestKill`, `TestMultiThreaded`, `TestPowerOffFs`, `TestPowerOffFs2`, `TestTimer`, `TestSynth`, `TestMulti` (synth.thread), `TestClassLoaderLeak`, `TestExit`, `TestMemoryUnmapper`, `TestTools` — 17 names, all on the baseline's 18-not-ours list. Not re-filed here; see that page's §3 for why (randomized crash/recover stress tests, or tests that want sockets/TLS this host doesn't give them).

### Wall-clock only — HotSpot passes in seconds, CratonVM is just slower on the same class (baseline §2a), recur today

`TestOpenClose`, `TestCachedQueryResults`, `TestCancel`, `TestScript`, `TestMVStoreBenchmark`, `TestCrashAPI`, `TestSimpleIndex`, `TestMVStoreCachePerformance`, `TestMVStoreTool`, `TestRandomMapOps` — 10 names, all on the baseline's 20-wall-clock list, all still HANGing/FAILing at the same kind of cap today. Not a new finding; the underlying "CratonVM is 10-150x slower than HotSpot on these specific IO/store-heavy classes" gap the baseline already measured is apparently unchanged a month later.

### The 2 known real correctness bugs — both recur today

| class | baseline finding |
|---|---|
| `org.h2.test.unit.TestBnf` | `Expected: true got: false` on HotSpot-fast classes — has its own page, [`not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`](not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md) (despite the filename, the baseline's own TL;DR counts this among the 2 real correctness rows, not the not-CratonVM 18 — check that page's own verdict before assuming its title settles it) |
| `org.h2.test.server.TestWeb` | `does not contain: '...'` — same page |

Both still non-passed today, consistent with neither having been fixed since 2026-08-18.

### `TestBenchmark` — marked FIXED 2026-08-18, still non-passed today, but differently

The baseline retired this to `bug-h2-testbenchmark-writebuffer-oom-at-1g-FIXED-20260818.md` after fixing a native `ByteBuffer.allocate` OOM shadow, noting it "no longer OOMs and now runs the whole workload slowly" (169.5s on HotSpot, 28.9s on CratonVM's ZGC arm at the time — CratonVM was actually *faster* than HotSpot in that one measurement). Today it's `HANG` at the 300s cap. That is not obviously the OOM regressing (a HANG at 300s with zero output looks different from an `OutOfMemoryError`), but it's also not obviously fine — worth a direct rerun with the fix's own repro before either closing this as "still just wall-clock" or reopening it.

## Genuinely new since the baseline — 22 classes not on the 40-class list at all

| class | status | ms | note |
|---|---|---:|---|
| `org.h2.test.db.TestAlterSchemaRename` | FAIL | 831 | |
| `org.h2.test.db.TestCases` | FAIL | 1944 | on baseline's wall-clock list as HANG (HotSpot 4.5s, CratonVM capped) — today it's FAIL instead, a different failure shape worth a look |
| `org.h2.test.db.TestCluster` | FAIL | 887 | |
| `org.h2.test.db.TestFullText` | FAIL | 21442 | |
| `org.h2.test.db.TestReadOnly` | FAIL | 976 | |
| `org.h2.test.db.TestSpatial` | FAIL | 4263 | |
| `org.h2.test.db.TestTriggersConstraints` | FAIL | 2640 | |
| `org.h2.test.db.TestView` | FAIL | 2020 | |
| `org.h2.test.jdbc.TestJavaObjectSerializer` | FAIL | 1260 | |
| `org.h2.test.jdbc.TestUrlJavaObjectSerializer` | FAIL | 1416 | |
| `org.h2.test.server.TestAutoServer` | FAIL | 3589 | |
| `org.h2.test.unit.TestAutoReconnect` | FAIL | 997 | |
| `org.h2.test.unit.TestFileSystem` | FAIL | 943 | on baseline's wall-clock list as HANG (HotSpot 7.9s) — today it's FAIL, same note as `TestCases` above |
| `org.h2.test.unit.TestJakartaServlet` | FAIL | 645 | |
| `org.h2.test.unit.TestPageStoreCoverage` | FAIL | 2696 | |
| `org.h2.test.unit.TestPgServer` | FAIL | 1604 | |
| `org.h2.test.unit.TestSampleApps` | FAIL | 2925 | |
| `org.h2.test.unit.TestServlet` | FAIL | 685 | |
| `org.h2.test.unit.TestUpgrade` | FAIL | 3221 | |
| `org.h2.test.synth.TestBtreeIndex` | HANG | 300014 | |
| `org.h2.test.unit.TestFtp` | HANG | 300008 | |
| `org.h2.test.db.TestTransaction` | FAIL | 10400 | baseline §2b disproved this as a correctness bug (lock-timeout arithmetic, not "half the rows") and reclassified it as wall-clock (§2a) — recurs today, consistent with that reading, not a new finding |

None of these 22 have been investigated yet. They are the actual new territory this page adds — everything else above is either already-explained noise or an already-tracked, still-open bug.

## Open items, in priority order

1. **The 22 genuinely-new classes need their own triage** — at minimum, a HotSpot control run (same host, same cap) the way the baseline did, to sort them into "not ours" / "wall-clock" / "real defect" the same way. Without that control, none of these 22 can be called a CratonVM bug yet.
2. `TestCases` and `TestFileSystem` changed FAILURE SHAPE from the baseline (HANG → FAIL) — worth understanding why before assuming it's the same underlying wall-clock gap.
3. `TestBenchmark`'s fix status is ambiguous — HANGing again, but not with the original OOM signature. Confirm with a direct rerun before treating the 2026-08-18 fix as regressed.
4. The two confirmed real correctness bugs (`TestBnf`, `TestWeb`) are still open a month later — no new information here, just confirming they haven't silently been fixed.
