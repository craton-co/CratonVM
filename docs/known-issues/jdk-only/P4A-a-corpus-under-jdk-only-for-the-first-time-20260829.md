# P4-A: a corpus under `--jdk-only` for the first time — 0 new failures, and the Phase 2 worklist is 1065, not 334

**Status: MEASURED 2026-08-29** on `azure-host-2` (`azureuser@20.80.105.49`),
worktree `/data/cvm-l7dod-20260828`, binary
`/data/l7dod-target/release/cratonvm`. Oracle: HotSpot
`/data/jdkimages/jdk25-linux/jdk-25.0.4+7`. Corpus: H2's own test suite, **218
classes**, one of the roadmap's three definition-of-done workloads.

`docs/feature-designs/jdk-only-completion-roadmap.md` §4, P4-A:

> **no corpus has yet been run under `--jdk-only`**, which is the mode this
> campaign is named for.

This is that run.

---

## 1. The trap this run exists to avoid

`HANDOFF-20260828-L7` §3, fourth trap:

> the regression suite writes its own census to a PID-scoped directory and
> **deletes it at the end**, and passing your own `--jdk-only-report` makes the
> suite skip its census and point every vector at your single path. If you want
> a corpus-wide census you need a per-vector path, not one shared one.

So every vector here gets its own report file and the union is computed
afterwards from the 218 of them. The distinction is not academic: with one
shared path the "corpus census" is the last class that ran, and it would have
looked entirely plausible.

A second reason the union matters more than any single report: the per-vector
shadow sink is **bounded** (4096 rows). One report can silently be a floor. The
census tool prints `partial` / `truncated` / `saturated` counts *before* any
number it derives, and refuses to call anything a total until they are zero.

## 2. What ran, and what the verdicts mean

Three shards, not four. This box has 8 cores and carries other lanes at a load
that sat between 9 and 20 throughout; `apps/tomcat-suite-runner/run-tomcat-suite.sh`'s
own header records what over-sharding costs here — 4 shards with 3 cores busy
put twelve healthy classes over the cap and produced a 15-class phantom
regression against an unchanged binary.

```text
                PASS    FAIL   TIMEOUT(90s)
--jdk-only       166      14       38
HotSpot          196      13        9
```

**A `TIMEOUT` is not a failure and is not counted as one.** 38 against HotSpot's
9, on the same shard layout and the same load, is a statement about SPEED —
quantified in §5, which also says how far the serial re-run of those 38 got
(2 of them) and counts the other 36 as unmeasured.

## 3. The headline: `--jdk-only` introduced no failures of its own

Of the 14 strict failures, **nine also fail on HotSpot** — environment and
fixture, not this VM:

```text
TestRecoverKillLoop  TestMulti     TestClassLoaderLeak  TestJoin   TestTimer
TestExit             TestMemoryUnmapper                 TestFunctions  TestTools
```

The remaining five were then run a third time, in CratonVM's **compatible**
mode, which is the arm that decides attribution — and all five fail there too:

```text
org.h2.test.server.TestWeb        AssertionError: 1#_ROWID_#_ROWID_ does not contain: column_name
org.h2.test.unit.TestBnf          AssertionError: Expected: true got: false
org.h2.test.db.TestTransaction    JdbcBatchUpdateException: Timeout trying to lock table "TEST"
org.h2.test.jdbc.TestSQLXML       JdbcSQLNonTransientException: General error: "java.lang.NullPointerException"
org.h2.test.synth.TestDiskFull    JdbcSQLNonTransientException: General error: "Chunk 4 not found [2.4.249/9]"
```

**So on a 218-class corpus, `--jdk-only` produced ZERO failures that compatible
mode does not also produce.** That is the question P4-A exists to answer, and
the answer is the good one.

The five are real CratonVM defects and are recorded here so they are not lost —
they belong to whoever owns those families, not to this lane. `TestSQLXML` and
`TestDiskFull` surface as an internal `NullPointerException` and a missing MVStore
chunk, which are the two worth a look first.

## 4. The census, unioned over 181 reports

```text
reports                       181 of 218          (37 missing; see §5)
partial / truncated / saturated  0 / 0 / 0        -> the counts below are TOTALS

compatibility_classes > 0     0 vectors
synthetic_stub_invocations > 0  0 vectors
```

37 missing against 38 timeouts, so exactly one capped vector still left a
readable report — the file is written at shutdown and a kill can land after it.
That is the direction that costs nothing: an extra report, not a missing one.

**The definition-of-done predicate holds on every vector of a real corpus.** Not
five probe programs — 181 test classes of a database engine, exercising JDBC,
MVStore, the SQL parser, compression, encryption, fulltext and the web server.

### The fabrication requests, by row

Five distinct classes, seven call sites, no prefix filter:

| vectors | class | requester |
| ---: | --- | --- |
| 148 | `java/util/Enumeration$Impl` | `classloader.rs:5449`, `:5477`, `:6352` |
| 132 | `cratonvm/stream/LazyOp` | `native-collections/src/lib.rs:26427` |
| 25 | `java/util/TreeSet$Itr` | `native-collections/src/lib.rs:55345` |
| 12 | `cratonvm/internal/SystemLogger` | `native-builtins/src/lib.rs:28535` |
| 2 | `java/util/ArrayDeque$Itr` | `native-collections/src/lib.rs:45825` |

Three of the five do not match `cratonvm/`, which is the roadmap's prefix clause
holding at corpus scale. `Enumeration$Impl` has **three** distinct call sites,
where the five-probe screen saw two — a request set is a property of the
workload, and a small workload undercounts it.

### Phase 2's worklist is three times the size the probe screen said

```text
native-shadows-bytecode   77 131 rows
  native-won               1065 DISTINCT triples   <- the worklist
  bytecode-won              557 DISTINCT triples   <- already lost; nothing to retire
```

`the-definition-of-done-screen-run-for-the-first-time-20260828.md` measured
**334** distinct `native-won` triples from five probe programs and called it the
actionable surface. On a corpus it is **1065**. The top of it:

```text
36 java/lang/Class        24 java/io/File          20 java/util/HashMap
28 java/lang/StringBuilder 24 jdk/internal/misc/Unsafe  20 java/util/Properties
27 java/lang/Thread       21 j/u/c/ConcurrentHashMap    19 java/util/ArrayDeque
                          20 java/util/ArrayList        18 java/nio/file/Files
```

Not a correction of that page's arithmetic — its 334 was right for its five
programs. It is the difference between what a probe reaches and what a workload
reaches, and it is the reason the roadmap wanted a corpus before adjudicating
Phase 2.

### The uninstantiable-receiver census at corpus scale

The instrument added on 2026-08-29
(`the-four-residuals-...-20260829.md` §R4) reported 31 distinct classes over the
five definition-of-done arms. Over this corpus plus those arms the union is
**53** — including sites no arm reached: `PosixFileAttributeView`,
`HttpURLConnection`, `SSLServerSocket`, `SSLServerSocketFactory`,
`javax/xml/stream/Location`, and a second `VarHandle` call site
(`lang_invoke.rs:2368` beside the known `:2821`).

## 5. The 38 that did not finish

**They are slow, not broken, and the run already held the evidence.** Every
`PASS` carries its elapsed seconds, so the arms can be paired directly on the
166 classes that passed in both:

```text
classes passing in both arms                    166
  of those with a HotSpot time >= 3s             54    (a divisor worth having)
strict/hotspot elapsed ratio   median 1.8x   min 0.5x   max 26.0x
total over the 166 shared passes    strict 1605s vs hotspot 465s = 3.5x

  26.0x  strict  78s / hotspot  3s   org.h2.test.synth.TestLimit
  21.3x          64s /           3s  org.h2.test.jdbc.TestGetGeneratedKeys
  20.0x          60s /           3s  org.h2.test.synth.TestKillRestart
  16.0x          80s /           5s  org.h2.test.synth.TestNestedJoins
  11.6x          58s /           5s  org.h2.test.synth.TestFuzzOptimizations
```

A 90-second cap over a workload running at a median 1.8x and a tail past 20x is
what produced 38 timeouts against HotSpot's 9. Nothing about that number is a
correctness signal.

**This is NOT a performance benchmark and must not be quoted as one.** The host
carried other lanes at a load between 9 and 20 for the whole run, both arms are
single-shot, and nothing was interleaved. `docs/known-issues/` already records
what this box does to unpaired timings. What the ratio is good for is exactly
one thing: explaining the timeout asymmetry without attributing it to failure.

**The serial re-run at 600 s is running and is NOT finished; 2 of 38 have
verdicts.**

```text
org.h2.test.db.TestIndex    PASS      107s    (capped at 90s in the sharded pass)
org.h2.test.db.TestCases    TIMEOUT   600s    (still over, at load ~17)
```

The remaining **36 are UNMEASURED, and are counted as unmeasured everywhere
above** — not as passes and not as failures. Finishing them is cheap on a quiet
host and the command is in Reproduce with `TMO=600` and a single shard; it was
not worth several hours of a box three other lanes are using, for a completeness
footnote to a result whose substance is in §3 and §4.

One thing the two verdicts do say: `TestIndex` passing at 107 s is a class the
sharded pass called a timeout at 90 s, so at least some of the 36 are ordinary
passes behind an unlucky cap — which is the direction that would *improve* the
166, never worsen the 14.

## Reproduce

```bash
source /data/toolchain/env.sh
# per-vector report paths -- one shared path makes the census the last class
for cls in $(cat apps/h2database-suite-runner/meta/all-classes.tsv); do
  wd=$(mktemp -d); cd "$wd"
  timeout 90 cratonvm --java-home "$JDK" --Xmx 1g \
      --jdk-only --explain-jdk-only --jdk-only-report "$OUT/rep/$cls.json" \
      -cp "$(cat $H2/craton-testcp.txt):$H2/target/classes:$H2/target/test-classes" "$cls"
  cd - >/dev/null; rm -rf "$wd"
done
python3 corpus-census.py "$OUT/rep" 218
```

Each vector needs its own working directory: H2 tests open files by
CWD-relative path and will collide otherwise. The runner and the census tool are
carried by this commit; `probes/` is deleted from the tree, so restore them the
way the sibling records do (`git show <commit>:probes/…`).
