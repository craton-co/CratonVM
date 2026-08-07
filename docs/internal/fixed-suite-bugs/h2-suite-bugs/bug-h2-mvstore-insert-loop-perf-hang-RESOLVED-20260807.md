# H2 — the MVStore insert/commit "perf hang", measured (RESOLVED 2026-08-07)

## Status
**RESOLVED / retired.** Everything this page asked has been answered, and the
two things on it that were *defects* are fixed. What remains is a constant
factor that is not this page's own — it is the general interpreter gap already
tracked on `docs/known-issues/h2/h2-update-path-throughput-20260802.md`, which
now carries the INSERT-side numbers too.

Predecessor: `docs/known-issues/h2/bug-h2-mvstore-insert-loop-perf-hang.md`
(OPEN from 2026-07-2x to 2026-08-07).

## What the page claimed, and what is true

> `org.h2.test.db.TestTempTables` … HotSpot completes the whole class in ~2 s.
> CratonVM exceeds 180 s (≈ ≥90× slower **for this workload**, versus the ~5×
> average on passing classes).

The `≥90×` was a **timeout lower bound**, not a measurement: nothing had run
the class to completion, so "exceeds 180 s" could have meant 200 s or 20 000 s,
and the "cliff versus the ~5× average" framing followed from that unknown.
Run to completion it is not a cliff.

`probes/H2InsertLoopProbe.java` models `testAnalyzeReuseObjectId` exactly — one
connection, one local temporary IDENTITY table, one `PreparedStatement`, 10 000
autocommit `insert into test default values`. Phases are timed separately so
the ~40 CPU-s VM-start + H2-class-load tax is not folded into the loop.
`--Xmx 1g`, real-JDK 25, single-threaded, third rep (warm), all four arms
within one hour on an Azure host at load 8-16:

| 10 000-row insert loop | time | µs/row | vs HotSpot C2 | vs HotSpot `-Xint` |
| --- | --- | --- | --- | --- |
| HotSpot 25, C2 | **15.4 ms** | 1.5 | 1× | 0.013× |
| HotSpot 25, `-Xint` | **1 208 ms** | 121 | 78× | 1× |
| CratonVM, JIT | **8 565 ms** | 857 | **556×** | **7.1×** |
| CratonVM, `--nojit` | **12 649 ms** | 1 265 | 821× | **10.5×** |

The bottom row is the one to read first. **Interpreter against interpreter,
INSERT is 10.5×** — dead centre of the flat 9.9-10.7× band the UPDATE page
measured across MERGE / UPDATE / SELECT / INSERT. There is no insert-specific
pathology; this workload is the band.

What makes the ratio against a *default* HotSpot look like a cliff is the other
column: **C2 is worth 78× on this shape**. A tight, hot, monomorphic loop
around one prepared statement is close to the best case for a tiered JIT, and
CratonVM's JIT recovers **1.5×** of it (8.6 s against 12.6 s) — unchanged from
the UPDATE page's 1.3-1.7×. The gap is JIT reach, not interpreter cost, and it
is the same gap everywhere else. `~90×` and `~5×` were never two different
phenomena; they were one phenomenon measured against two different HotSpot
configurations, and the number moves with the host: the same four arms at load
15-20 read 74 ms / 1 919 ms / 10 449 ms / 16 615 ms, i.e. 141× and 5.4×.
**Quote the interpreter-against-interpreter ratio, not the C2 one** — it is the
stable quantity.

## The suspected hot spots: all four answered, none of them it

The page listed four suspects "for follow-up profiling" and asked for a
sampling profile over a 10 000-row insert microbench. Both instruments were
run.

**`--stack-sample-ms 20 --nojit`, 887 samples over one 10 000-row loop**,
aggregated by deepest frame:

| share | leaf |
| --- | --- |
| 4.06% | `org.h2.mvstore.RootReference.<init>` |
| 3.49% | `org.h2.mvstore.tx.CommitDecisionMaker.decide` |
| 3.38% | `org.h2.mvstore.Page.getKeyCount` |
| 3.16% | `org.h2.mvstore.Page$Leaf.getValue` |
| 2.59% | `org.h2.mvstore.tx.Transaction.markStatementEnd` |
| 2.14% | `org.h2.engine.SessionLocal.startStatementWithinTransaction` |
| … | 40+ more, none above 1.5% |
| **1.01%** | **`org.h2.mvstore.Page.clone`** — suspect 3 |
| **1.01%** | **`org.h2.mvstore.MVMap.operate`** — suspects 1 and 2 |

**The profile is flat.** The heaviest single leaf is 4%, every entry is H2's own
bytecode, and the named suspects are at 1%. There is no dominant cost to find,
which is the answer to "find the dominant cost" — the time is the per-row
transaction machinery itself, spread across dozens of small methods, executed
at the interpreter's ordinary rate.

**`--dump-native-registry`, same workload, 20 000 rows:** 5 812 476 native
invocations, i.e. **290 natives per row**. At the in-tree funnel profilers'
~120 ns per compiled-code native call that is ≈0.70 s of a ≈21 s two-rep loop —
**~3%**. Suspect 4 (boxing) is inside that: `Long.valueOf` is 79 555 calls,
under 1%. Top entries are `AtomicReference.get` (544 288), `Enum.ordinal`
(360 941), `AtomicLong.get` (346 162), `AtomicReference.compareAndSet`
(341 641) — the MVMap CAS loop, exactly where H2 puts it, and cheap.

Census artefacts: `/data/data/mvperf/natreg.json` (host-side, not committed).

## Two real defects blocked the reproduction, and both are fixed — by someone else

Neither was the throughput factor, and neither is this branch's to claim.
Reproducing this page on `origin/dev` @ `6ba350cdd` was impossible because
**every file-backed H2 test died at `getConnection`**:

    FileChannel ch = new RandomAccessFile(f, "rw").getChannel();
    ch.tryLock();
    // NPE: Cannot invoke "sun.nio.ch.FileLockTable.add(...)" because "flt" is null

Both defects are in the mark word, both arrived with the `HEADER_SIZE 24 -> 16`
merge, and both were root-caused and fixed **concurrently and independently by
another session**, landing on `dev` while this branch was measuring:

1. **`try_thin_unlock` erased the mark word's quartet.** The last thin-lock
   release stored the literal `MARK_NEUTRAL`, which is `0`, and since `kind` /
   `element_type` / `gc_age` / `gc_flags` moved into bits 48..63 that erased
   all four on every final `monitorexit`. Losing `GC_FLAG_COMPACT` means the
   object keeps its compact body but stops answering `is_compact_object`, so
   every later field access falls to the legacy 16-byte-cell path over it.
   `FileChannelImpl.fileLockTable()` is double-checked locking: the `putfield`
   inside `synchronized (this)` landed correctly at compact offset 80, the
   `monitorexit` cleared the flag, and the `return fileLockTable` one
   instruction later read offset 256 of a 96-byte body.

2. **`MARK_QUARTET_MASK` covered 14 of the quartet's 16 bits.** `gc_age` runs
   to bit 64; the mask stopped at 61. An object that had survived four young
   collections therefore failed `try_thin_lock`'s screen forever and **every
   `synchronized` on it inflated a `Monitor`** instead of doing one CAS — a
   throughput loss falling exactly on the long-lived lock-heavy objects H2
   keeps (`MVStore`, `MVMap`, `SessionLocal`).

The full write-up, including the refutation of its own first hypothesis, is
`docs/internal/fixed-suite-bugs/vm/compact-ref-field-layout-corrupts-filechannel-filelock-FIXED-20260807.md`.

This branch reached the identical two fixes and the same root cause from the
opposite end (`CRATONVM_DBG_FIELD_WATCH` on `fileLockTable` and a per-access
layout trace, rather than an allocation-site probe) and its versions were
dropped at merge time in favour of theirs, whose tests are strictly broader.
**What that cost, and what it bought:** three release builds, and the four
`TestTempTables` / `TestIndex` A/B arms below, which are the evidence that a
*third* defect from the same merge is still open. Recording it because the
lesson is cheap and recurs: `git log --oneline origin/dev | grep -i <symbol>`
before the first build, and again before each rebuild on a long task.

## The diagnostic residual is root-caused and fixed

> **Diagnostic note:** `CRATONVM_DEFAULT_WATCHDOG_SEC` set through
> `run-h2-suite.sh`'s `env VAR=... ./run-h2-suite.sh` wrapper did not produce a
> watchdog dump … the mechanism by which the env var fails to reach the child
> through that wrapper is unconfirmed and worth a follow-up.

`run_one_class` put `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` in every child's
environment unconditionally, and `vm-cli`'s `default_watchdog_env` resolves to
`None` the moment that is `1`, whatever the `_SEC` value says. One line, and
three write-ups had to say "bypassing the suite runner" because of it. Fixed
(`fix(h2-runner)`, this branch): the disable is now applied only when the
caller has not set a deadline itself, and the help text documents the
interaction. `env CRATONVM_DEFAULT_WATCHDOG_SEC=N ./run-h2-suite.sh run ...`
produces the `T19.H1` dump.

## Class list: what the classes actually do now

Sixteen of the classes this page names were swept on 2026-08-07, four shards,
300 s per class, `--Xmx 1g`, four arms. Two things have to be separated before
any of it means anything.

**Six of the sixteen fail on HotSpot too**, when invoked as `java <Class>` —
which is exactly what `run-h2-suite.sh` does. `TestOutOfMemory`, `TestKill`,
`TestNestedJoins`, `TestFunctions`, `TestCacheLongKeyLIRS` and
`TestSubqueryPerformanceOnLazyExecutionMode` need `TestAll`'s configuration,
so their CratonVM status says nothing about CratonVM. **A CratonVM status is
only interpretable next to a HotSpot control on the same invocation**, and this
page never had one. They are dropped from the table below.

The ten that HotSpot does pass (`pre` = the merge's first parent `9ddbc9c61`;
`jit` / `nojit` = dev tip with the two mark-word quartet fixes applied):

| class | HotSpot | pre (JIT) | dev tip (JIT) | dev tip `--nojit` |
| --- | --- | --- | --- | --- |
| `TestBigDb` | PASS 1 s | PASS 6 s | PASS 3 s | PASS 7 s |
| `TestCompatibility` | PASS 2 s | PASS 225 s | HANG | PASS 156 s |
| `TestIndex` | PASS 4 s | **PASS 286 s** | **OOM 71 s** | HANG |
| `TestCases` | PASS 5 s | HANG | HANG | HANG |
| `TestOptimizations` | PASS 5 s | HANG | HANG | HANG |
| `TestScript` | PASS 17 s | HANG | HANG | HANG |
| `TestPerfectHash` | PASS 18 s | HANG | HANG | HANG |
| `TestOpenClose` | PASS 21 s | HANG | HANG | HANG |
| `TestCrashAPI` | PASS 66 s | HANG | HANG | HANG |
| `TestTempTables` | PASS 3 s | HANG | **OOM 108 s** | HANG |

(Four shards on a shared host inflate every CratonVM figure; run one at a time
`TestTempTables` is 633 s `--nojit`, not a 300 s timeout. The HotSpot column is
from the same sweep, so the comparison is like-for-like about *status*, not
about the exact seconds.)

The `pre` column is this page's original claim, reproduced: everything but two
of the ten exceeds a 300 s budget where HotSpot takes 1-66 s, while
progressing. That is the throughput factor and nothing else.

**A newer regression now sits on top of it.** Three of the JIT-arm failures are
`OutOfMemoryError`, and the `--nojit` arm produces **none**. Isolated on
`TestIndex`, one class at a time:

| binary | flags | result |
| --- | --- | --- |
| `9ddbc9c61` (the merge's first parent) | default | **PASS 174 s** |
| `6ba350cdd` (the merge) | `CRATONVM_COMPACT_REF_FIELDS=0` | **OOM 79 s** |
| `6ba350cdd` + the quartet fixes | `CRATONVM_COMPACT_REF_FIELDS=0` | **OOM 127 s** |
| `6ba350cdd` + the quartet fixes | default | **OOM 110 s** |

The pre-merge binary completes the class; the post-merge one cannot — and
`TestTempTables` OOMs at `--Xmx 4g` as well as at 1 g. That is a **new defect
from the `HEADER_SIZE 24 -> 16` merge, not this page's throughput factor**, it
predates and survives the two quartet fixes, and it has its own record:
`docs/known-issues/vm/jit-young-heap-exhaustion-after-header-16-20260807.md`.

With it held off (`--nojit`), the classes behave exactly as this page described
and as the microbench predicts: too slow for any practical per-class timeout,
progressing throughout, no deadlock.

## Where the residual went

The irreducible part — CratonVM costs ~5-10× HotSpot's interpreter and its JIT
recovers ~1.5× where C2 recovers ~26× — is **not filed here any more**. It is
the same quantity `h2-update-path-throughput-20260802.md` tracks, which now
carries the INSERT-loop table and the flat profile above. Keeping a second page
for the same constant is what sent three sessions after "a cliff".

## Reproducing

```bash
javac -cp <h2>/target/classes -d probe \
    apps/h2database-suite-runner/probes/H2InsertLoopProbe.java
# rows reps [dbdir] — phases are printed separately
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" \
    H2InsertLoopProbe 10000 3 ./db
<jdk25>/bin/java       -Xmx1g -cp "<h2>/target/classes:probe" H2InsertLoopProbe 10000 3 ./db
<jdk25>/bin/java -Xint -Xmx1g -cp "<h2>/target/classes:probe" H2InsertLoopProbe 10000 3 ./db
```

The two instruments, in the order that answers the question:

```bash
<cratonvm> ... --dump-native-registry natreg.json ... H2InsertLoopProbe 10000 2 ./db
<cratonvm> ... --nojit --stack-sample-ms 20 ... H2InsertLoopProbe 10000 1 ./db 2> samples.txt
```

Aggregate `samples.txt` by the last `tid=… depth=…` line of each
`--- T19.H1 stack dump …` block; that is the deepest interpreted frame, and it
is the only reading of that file that is time-weighted. A native makes no
interpreted frame, so the sampler charges its cost to the caller — which is why
the native census has to be read beside it, not instead of it.

The whole class, when you need the real thing:

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home <jdk25> --Xmx 1g --nojit \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestTempTables
```

Drop `--nojit` and it OOMs — see the regression record above, not this page.

## Related

* `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — the surviving
  page for the constant factor. Its "flat across statement kinds" band is what
  this page's numbers fall inside.
* `docs/known-issues/vm/jit-young-heap-exhaustion-after-header-16-20260807.md` —
  the regression that currently masks the throughput factor on this class.
* `docs/known-issues/h2/bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807.md`
  — the three classes split out of this page whose stuck locus is elsewhere.
  Still OPEN, still single-sample.
* `bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md` — the
  SIGABRT whose fix turned `TestOutOfMemory` from a crash into this page's
  second confirmed instance.
