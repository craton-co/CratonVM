# Three HANGs outside the row-iteration cliff: `CacheLongKeyLIRS` eviction, `Trace.isEnabled`, and `MVStore` close-time compaction

## Status
**OPEN, single-sample** — found 2026-08-07 in a full 218-class suite sweep on a
clean host (`origin/dev` merge @ `f9315411a`, load average ~14). All three
classes are in the 41-class non-passing set, all HANG at the per-class
timeout. Split out from
[`bug-h2-mvstore-insert-loop-perf-hang.md`](bug-h2-mvstore-insert-loop-perf-hang.md)
because each is stuck at a locus that is NOT the row-iteration / commit path
that doc already characterizes — grouping them there would misattribute the
cause. Each entry below is a single `--stack-dump-on-timeout` sample; unlike
the confirmed instances in the perf-hang doc (which had many repeated dumps
showing frames vary sample-to-sample, i.e. genuinely progressing), these have
only one sample each and so "progressing but slow" vs "stuck" is **not**
distinguished yet — that is exactly the open follow-up.

## Repro
Direct binary invocation, bypassing the suite runner wrapper (which did not
reliably deliver `CRATONVM_DEFAULT_WATCHDOG_SEC` to the child — see the note
in the perf-hang doc):
```bash
cd apps/h2database/h2
env CRATONVM_DEFAULT_WATCHDOG_SEC=25 <cratonvm-bin> --java-home /home/victor/jdk25 \
  --Xmx 1g --nojit -c "<full test classpath>" <class>
```

## 1. `org.h2.test.db.TestLIRSMemoryConsumption`

```
TestLIRSMemoryConsumption.main
  TestBase.testFromMain
  TestLIRSMemoryConsumption.test
  TestLIRSMemoryConsumption.testMemoryConsumption
  CacheLongKeyLIRS.put
  CacheLongKeyLIRS$Segment.put
  CacheLongKeyLIRS$Segment.evict
  CacheLongKeyLIRS$Segment.evictBlock
  CacheLongKeyLIRS$Segment.addToQueue
```

Stuck inside the LIRS (Low Inter-reference Recency Set) cache's own eviction
bookkeeping — a `put()` triggered `evict()` → `evictBlock()` →
`addToQueue()`. Two readings, not yet distinguished:

* **Same general family**: `testMemoryConsumption` presumably drives many
  `put()` calls to probe memory behavior under cache pressure, and eviction
  cost per put is just the row-iteration-style throughput cliff showing up
  in a different H2 subsystem.
* **Possible genuine algorithmic loop**: LIRS eviction is a linked-queue
  algorithm with specific invariants (hot/cold demotion, queue pruning); if
  any CratonVM-side data-structure or GC interaction violates one of those
  invariants, `evictBlock`/`addToQueue` could cycle without making progress
  rather than merely being slow. Only one dump was captured before the
  process aborted (the log is 41 lines total, versus 200K+ for the other two
  classes below, which fired the watchdog and kept running until the
  external `timeout` — i.e. did not exit as promptly), which is itself mildly
  suggestive that this one may not have been "still going" in the same way.
  Not conclusive either way — needs a repeat run with a longer watchdog
  deadline and multiple dumps to see if the frames vary.

## 2. `org.h2.test.synth.TestBtreeIndex`

```
TestBtreeIndex.main
  TestBtreeIndex.test
  TestBtreeIndex.testAddDelete
  JdbcResultSet.getInt
  TraceObject.debugCodeCall
  Trace.isEnabled
```

Stuck inside H2's own **debug-code-generation tracing path**
(`TraceObject.debugCodeCall` → `Trace.isEnabled`), reached from
`ResultSet.getInt()` — not inside query execution or row storage at all.
`debugCodeCall` is H2's mechanism for optionally logging a Java-source-style
trace of every JDBC API call when trace level is high enough; the entry
point is `Trace.isEnabled(level)`, which should be a cheap field/threshold
check. If `isEnabled()` (or something it calls) is unexpectedly costly under
CratonVM per invocation, and `testAddDelete` calls `getInt()` in a tight
loop (per its name, over many index add/delete operations), the aggregate
cost could dominate even though each individual check "should" be trivial.
Worth checking whether `Trace.isEnabled` bottoms out in a native call, a
volatile/atomic read, or something heavier on the CratonVM side — this
locus is a plausible discrete inefficiency distinct from the MVStore
insert/commit cliff, not just the same cliff wearing a different hat.

## 3. `org.h2.test.synth.sql.TestSynth`

```
TestSynth.main → TestBase.testFromMain → TestSynth.test → testCase → testRun
  → process → Command.run → DbConnection.disconnect → JdbcConnection.close
  → SessionLocal.close → Database.removeSession → Database.close(2 frames)
  → Database.closeImpl → closeOpenFilesAndUnlock → Store.close
  → MVStore.close → MVStore.closeStore → FileStore.stop → FileStore.compactStore
  → RandomAccessStore.compactStore → FileStore.compact(...)
```

Stuck during **database-close-time compaction**, not live query work —
`TestSynth`'s random SQL fuzzing presumably leaves a heavily fragmented
MVStore file, and `Database.close()` triggers a full `compactStore()` /
`FileStore.compact()` pass over it. This is the same MVStore machinery
touched by the (separately fixed) old-generation coalescing issue in
[`bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md),
but reached through H2's own compact-on-close call path rather than GC. Not
yet established whether this is (a) H2's compaction algorithm being
inherently expensive against a highly fragmented fuzzed store (would also be
slow, just less so, on HotSpot), or (b) a CratonVM-side cost specific to the
`FileStore.compact`/`RandomAccessStore.compactStore` call chain.

## Next steps
* Repeat each with a longer `--stack-dump-on-timeout` interval and let the
  process keep running past one dump (drop the outer `timeout` or raise it
  well past the watchdog deadline) to get multiple samples per class and
  settle "progressing" vs "genuinely stuck" the same way the confirmed
  instances in the perf-hang doc were settled.
* For `TestBtreeIndex`: microbenchmark `Trace.isEnabled` in isolation to
  quantify its actual per-call cost on CratonVM vs HotSpot.
* For `TestSynth`: reproduce `FileStore.compact()` standalone against a
  pre-fragmented store file of known size, timed against HotSpot, to
  establish whether the compaction itself is the CratonVM-specific
  bottleneck.

## Related
* [`bug-h2-mvstore-insert-loop-perf-hang.md`](bug-h2-mvstore-insert-loop-perf-hang.md)
  — the row-iteration/commit throughput cliff; the working hypothesis this
  doc exists to rule in or out for these three classes.
