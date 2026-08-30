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
quantified in §5. **All 38 were subsequently re-run at 600 s and §6 has their
verdicts**; the totals in this table are the 90-second ones and §6 restates them.


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

### RE-CHECKED 2026-08-30 — four of the five still reproduce, and one is GONE

That list was a day and ~150 `dev` commits old, so it was re-run rather than
quoted. One execution per arm:

```text
TestWeb          compat FAIL   14s    strict FAIL     25s
TestBnf          compat FAIL    8s    strict FAIL     11s
TestTransaction  compat FAIL   10s    strict FAIL     11s
TestDiskFull     compat FAIL    8s    strict TIMEOUT 600s
TestSQLXML       compat PASS    3s    strict PASS      3s     <- no longer reproduces
```

**Four still fail, and still fail in COMPAT**, so §3's attribution holds for
them unchanged: none is a `--jdk-only` defect.

**`TestSQLXML` no longer reproduces at all.** 44 further runs say so — 20 solo
(10 per mode) and **24 under six-way contention at load 11-15** — every one a
pass. The contention arm is there deliberately: concurrency has manufactured two
failures in this very record (§5's `ENOSPC`, §7's OOM), so "it only failed in the
sharded corpus run" was the first alternative to rule out, and it is ruled out.

It was NOT bisected across the ~150 commits. A defect that will not reproduce is
a poor bisect subject, and the searchable signature is preserved here instead:

```text
JdbcSQLNonTransientException: General error: "java.lang.NullPointerException"
```

### The synthetic layer is eliminated for the three that still fail

Each was re-run under `--jdk-only` with its own census attached, which is the
step that turned "it survives strict" into an elimination for `TestOpenClose`:

```text
TestWeb          mode=jdk-only  compatibility_classes=0  synthetic_stub_invocations=0
TestBnf          mode=jdk-only  compatibility_classes=0  synthetic_stub_invocations=0
TestTransaction  mode=jdk-only  compatibility_classes=0  synthetic_stub_invocations=0
                 all three: partial none, truncated false, dropped 0 -- totals, not floors
```

All three fail with **nothing fabricated and no synthetic stub invoked**, so none
of them is a fabricated-carrier or synthetic-stub defect. Whatever they are, they
live in the path both modes share. That is worth having on record for whoever
picks them up, because it is the cheapest question to ask and the most annoying
one to leave open.

Two smaller notes. `TestDiskFull`'s message moved from `Chunk 4 not found` to
`Chunk 3 not found`, so that row is not deterministic in its detail. **Neither is
`TestWeb`'s** — §3 quotes it as `1#_ROWID_#_ROWID_ does not contain: column_name`
and a later run of the same class in the same mode produced
` does not contain: '`. Two runs, two different assertion bodies, so no single
message from this class should be treated as its signature. And of the
five, **`TestSQLXML` was the only one with no page anywhere** — the other four
are already carried by `h2/nonpassed-40-census-20260818.md`,
`h2/correctness-issues-consolidated.md`, and in `TestDiskFull`'s case its own
`repros/h2-testdiskfull-livelock/` directory. So the one this lane could have
adopted is the one that stopped failing.

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

**The serial re-run at 600 s was stopped after 4 of 38, and one of those four
is withdrawn.**

```text
org.h2.test.db.TestIndex                  PASS      107s   (capped at 90s before)
org.h2.test.db.TestLIRSMemoryConsumption  PASS      264s   (capped at 90s before)
org.h2.test.db.TestCases                  TIMEOUT   600s   (still over, at load ~17)
org.h2.test.db.TestLargeBlob              FAIL        7s   WITHDRAWN — see below
```

### The re-run harness produced a failure the run it was re-running never had

`TestLargeBlob` "failed" in 7 seconds. It did not:

```text
Caused by: java/io/IOException: pwrite0: No space left on device (os error 28)
    at org/h2/mvstore/FileStore ... SingleFileStore.writeFully
```

The corpus runner gives each vector a working directory under `$OUT/wd` on
`/data` — 433 G, 84 G free. My re-run script took the one-line shortcut of
`mktemp -d`, which lands on `/`, and `/` on this box sits at **97 % with 868 M
free**. A test that writes a multi-gigabyte BLOB has nowhere to put it. Same
binary, same class, same flags, different filesystem — and the difference is a
`FAIL` that looks exactly like a defect.

That is the trap `docs/known-issues/` already records as "a full disk reads as a
set of failed vectors", arriving by a new door: not a disk that filled during a
run, but **a re-run harness that did not reproduce where the original run put
its files**. A re-run has to copy the original's working-directory placement,
not just its command line.

**The corpus run itself is clean of this.** `No space left on device` appears in
exactly ONE log across both arms' 436 vectors, and that one is the file this
re-run overwrote. §3 and §4 are untouched.

### What is left

**34 are UNMEASURED, and are counted as unmeasured everywhere above** — not as
passes and not as failures. Finishing them is cheap on a quiet host with a
working directory on `/data`, and the command is in Reproduce with `TMO=600`;
it was not worth several hours of a box three other lanes are using for a
completeness footnote to a result whose substance is in §3 and §4.

The three usable verdicts say the cap was the binding constraint for at least
some of them — `TestIndex` at 107 s and `TestLIRSMemoryConsumption` at 264 s are
both ordinary passes that a 90 s cap called timeouts. What they do **not**
license is the claim that the other 34 are all passes: an unmeasured vector has
no verdict, and `TestCases` is still over at 600 s.

## 6. FINISHED 2026-08-30: the 38 were re-run, and the headline survives it

§5 left 34 of the 38 capped vectors UNMEASURED and said so. They are measured
now — all 38, at a 600-second cap, three shards, **working directory on `/data`**
so the instrument fault §5 describes cannot recur.

```text
re-run of the 38          PASS 13    FAIL 4    TIMEOUT(600s) 21
  rows carrying ENOSPC     0                   <- the §5 fault is gone
```

Folding that into §2 gives the corpus its complete strict-mode picture:

```text
                 PASS   FAIL   TIMEOUT
--jdk-only, was   166     14     38  (90s)
--jdk-only, now   179     18     21  (600s)      179+18+21 = 218
```

### The four new failures do not change §3's answer, and two of them needed the compat arm to say so

```text
                              HotSpot        CratonVM compat   CratonVM strict
TestOutOfMemory               FAIL   17s     -                 FAIL     6s
TestMvccMultiThreaded         FAIL    2s     -                 FAIL    10s
TestOpenClose                 PASS   18s     FAIL   346s       FAIL   451s
TestRandomMapOps              PASS  143s     FAIL   480s       TIMEOUT 600s
```

* The first two **fail on HotSpot too** — environment and fixture, like the nine
  in §3.
* The last two pass on HotSpot, so each got the third run that decides
  attribution, and **both fail in CratonVM's COMPATIBLE mode as well**. They are
  CratonVM defects, not `--jdk-only` defects.

So after adjudicating every one of the 38, **`--jdk-only` still produces ZERO
failures that compatible mode does not** — now over 197 adjudicated vectors
rather than 180, which is the claim P4-A exists to make and it got stronger, not
weaker, by being finished.

`TestRandomMapOps` is worth one note: its HotSpot verdict was `TIMEOUT` at 90 s
in §2, which is **not** "HotSpot fails". Re-run at 600 s it PASSES in 143 s. A
cap on the oracle side is an absent oracle, and comparing against one would have
mis-attributed this row in either direction.

### Two defects to hand on

Both are mode-independent CratonVM defects on classes HotSpot passes, and
neither belongs to this lane:

* **`org.h2.test.db.TestOpenClose`** — fails in both modes after ~350–450 s,
  against an 18 s HotSpot pass. The 20x wall-clock gap is its own question.
* **`org.h2.test.store.TestRandomMapOps`** — compatible mode dies with
  `seed:3698333351056078266 op:1571 java.lang.NullPointerException`.
  **Already owned:** `h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`,
  whose own history records that the printed seeds do NOT replay, so that number
  is not the lead it looks like. What this lane's measurement did add is on that
  page as an addendum: the defect is **not** confined to the small heap the page
  studies — 1g fails 4 of 4 and **4g fails too**, heap buying latency rather than
  safety, and at 1g and above the dominant face is a WRONG ANSWER
  (`Expected: 247 actual: 198`, a map short of entries) rather than the crash the
  page opens with.

### The 21 that still do not finish

Still `TIMEOUT`, now at a cap **6.7x larger**, and still counted as UNMEASURED
rather than as failures. The re-run itself ran under a host load between 12 and
27 with other lanes active, so these remain load-qualified: a 600 s cap on a
box at load 27 is not the same instrument as a 600 s cap on an idle one. What
can be said is that they are the slow tail §5 predicted from the ratio data, and
that nothing in the 17 that did resolve turned out to be a `--jdk-only` defect.

## 7. The 21, given an ORACLE first — and the one strict-only failure was my own harness

§6 left 21 vectors capped at 600 s and called them unmeasured. Raising the cap
again would have been the obvious next move and would have been wrong for a
third of them, because **seven of the 21 had no HotSpot verdict either** — they
were `TIMEOUT` at 90 s on the oracle side too, and §6 had just finished
recording that a capped oracle is an ABSENT oracle, not a failing one.

### Step 1 — buy an oracle before spending anything on the subject

HotSpot, 1800 s, on the seven:

```text
TestLob          PASS  138s     TestKill         TIMEOUT 1800s
TestBenchmark    PASS  158s     TestPowerOffFs   TIMEOUT 1801s
TestSimpleIndex  PASS  111s     TestPowerOffFs2  TIMEOUT 1802s
                                TestSynth        TIMEOUT 1801s
```

Three were ordinary HotSpot passes hidden by the 90 s cap. **Four do not finish
on HotSpot at twenty times that cap**, so no CratonVM verdict on them can mean
anything, at any cap, ever. They are not slow-under-CratonVM; they are long.

That leaves the set worth spending time on: **15 vectors with a HotSpot PASS**
(12 already had one, plus the three just bought). The other six are 4 with no
oracle and 2 that FAIL on HotSpot.

### Step 2 — `--jdk-only` on the 15, at 1800 s

```text
PASS 5     TestLob 774s · TestKillProcessWhileWriting 456s
           TestMVStoreCachePerformance 911s · TestBtreeIndex 479s · TestPerfectHash 249s
FAIL 2     TestCachedQueryResults 1365s · TestMVStoreTool 160s
TIMEOUT 8
```

### Step 3 — and this is where the corpus nearly got its first strict-only defect

`TestMVStoreTool` failed under `--jdk-only` and PASSED in compatible mode, both
at `--Xmx 1g`, against a 32 s HotSpot pass. That is the exact shape §3 says does
not exist in this corpus, and it would have falsified the headline.

**It does not reproduce.** Re-run alone on a quiet box:

```text
sharded, 3 concurrent shards   strict 1g   FAIL 160s   OutOfMemoryError: Java heap space
alone                          strict 1g   PASS 704s
alone                          strict 2g   PASS 708s
alone                          compat 1g   PASS 557s
```

The failure was `OutOfMemoryError`, and the harness was running **three shards
of `--Xmx 1g` concurrently** while other lanes used the same 31 GB box. Under
that pressure the OOM landed on the strict arm; on a quiet box the same command
passes with the same heap. **My own runner manufactured a mode-specific failure,
for the second time in this page** — §5 was an `ENOSPC` from a working directory
on the wrong filesystem, and this is the same species: a harness artefact that
wears a defect's clothes and points at the mode you are studying.

The rule this earns: **a candidate mode-specific failure is re-run ALONE before
it is believed.** Concurrency is fine for finding candidates and worthless for
confirming them, because the resource that decides the verdict is shared and the
arm it lands on is luck.

`TestCachedQueryResults` got the same treatment and is real, but not
`--jdk-only`'s: **compat FAILs it too**, in 1078 s, so it is a mode-independent
CratonVM defect on a class HotSpot passes in 9 s.

### Where the corpus stands, complete

```text
                 PASS   FAIL   unresolved
--jdk-only        185     19       14        = 218
```

The 14 unresolved are **8 that exceed 1800 s under `--jdk-only`, 4 that exceed
1800 s on HOTSPOT as well, and 2 that HotSpot FAILs** — and only the first eight
are a statement about this VM at all.

**Across every vector this corpus can adjudicate, `--jdk-only` still produces
zero failures that compatible mode does not.** The one candidate was the
measuring instrument.

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

Each vector needs its own working directory, **on a filesystem with room**.
H2 tests open files by CWD-relative path and will collide otherwise, and some of
them write gigabytes — `scripts/jdk-only-corpus-run.sh` puts them under `$OUT`
for exactly that reason. `mktemp -d` lands on `/`, which on this box is at 97 %,
and that turns `TestLargeBlob` into a 7-second `FAIL` that is really an
`ENOSPC` (§5). The runner and the census tool are
carried by this commit, and they live in `scripts/` rather than `probes/`
because they are tooling over a corpus, not a probe program.
