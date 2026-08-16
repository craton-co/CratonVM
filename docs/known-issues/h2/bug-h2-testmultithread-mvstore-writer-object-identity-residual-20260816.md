# `TestMultiThread` — MVStore background writer sees an object of the wrong class (residual after the reference-processor shape guard)

## Status
**OPEN, CratonVM-specific, present on pristine `origin/dev` as well as on the
fixed build** — found 2026-08-16 on Azure host `azureuser@20.80.105.49` while
discharging the retired `bug-h2-testmultithread-npe-array-length-null-local-concurrent-connect`
write-up. HotSpot JDK 25 passes the same class on the same classpath in the same
sessions.

This is **not** the defect that record was about. That one was the reference
processor publishing a reclaimed-and-reused address into a `ReferenceQueue` (and
nulling field 0 of whatever now occupied it); it is fixed, and its two
signatures — `ClassCastException: ... cannot be cast to class
org.h2.util.CloseWatcher` / `... FileLockTable$FileLockReference` out of
`ReferenceQueue.poll()` — no longer occur. What is left is a *different*
wrong-object-at-an-address failure, in a different phase, with no `java.lang.ref`
frame anywhere in it.

## The failure

Both witnesses are the MVStore **background writer** thread committing
`.../data/test/lockMode.mv.db`, inside `TestMultiThread.testConcurrentUpdate`,
`--nojit`, `--Xmx 1g`:

```
[cratonvm] WARN vm_exec: NoSuchMethodError
    method="java/lang/String.toByteArray()[B"
    caller="org/h2/mvstore/db/ValueDataType.write(Lorg/h2/mvstore/WriteBuffer;Lorg/h2/value/Value;)V @pc=529"

Exception in thread "MVStore background writer .../lockMode.mv.db"
  org.h2.mvstore.MVStoreException: java.util.concurrent.ExecutionException:
  org.h2.mvstore.MVStoreException: java.lang.NoSuchMethodError:
  'byte[] java.lang.String.toByteArray()' [2.4.249/3]
	at org.h2.mvstore.MVStore.panic(MVStore.java:515)
	at org.h2.mvstore.MVStore.storeNow(MVStore.java:992)
	at org.h2.mvstore.FileStore$BackgroundWriterThread.run(FileStore.java:2266)
```

`ValueDataType.write` reaches `toByteArray()` on the `BigInteger` it takes from a
numeric `Value`. The receiver at that call site is a `java.lang.String`. Nothing
in H2 can put one there.

The second witness is the same thread, same file, same panic path, with the
wrong object arriving as a null instead:

```
Exception in thread "MVStore background writer .../lockMode.mv.db"
  ... java.lang.NullPointerException: Cannot invoke
  "org.h2.value.Value.getValueType()" because "v" is null [2.4.249/3]
	at org.h2.mvstore.MVStore.panic(MVStore.java:515)
```

## Why it is a residual and not a regression

Measured in one ABBA-interleaved sequence (`A B B A A B B A`, 8 runs, same host,
same session, `A` = pristine `origin/dev` @ `ecc09d40d`, `B` = the same tree plus
the reference-queue fixes):

| arm | result |
|---|---|
| A (pristine dev) | 4/4 FAIL — 3× `ClassCastException` out of `ReferenceQueue.poll()`, **1× this defect** (run 8, the `Value ... "v" is null` form) |
| B (fixed) | 3/4 PASS, **1× this defect** (run 3, the `String.toByteArray()` form) |

So it fires on the pristine build too; on `A` it is usually pre-empted by the
louder reference-queue bug, which is why it had not been seen on its own before.

## What is known about the mechanism

* It is an **object-identity** failure, not a field-value failure: the receiver
  is a whole object of an unrelated class, exactly the shape a reclaimed slot
  reused by a later allocation produces.
* It is **not** reached through `java.lang.ref` — no `ReferenceQueue`,
  `CloseWatcher` or `Cleaner` frame appears, and it survives the shape guard
  that now screens every reference-processor write
  (`vm/src/runtime/interpreter/gc_and_alloc.rs`).
* A synthetic probe of the reference machinery still shows a small same-family
  residue that may share a root cause: over 3200 phantom references through one
  queue on 8 threads, the fixed VM delivers 3197 distinct references with zero
  duplicates (HotSpot: 3185/3185/0) — but 2–5 of them arrive as a *different,
  half-constructed instance of the same class* (a `final int` field assigned in
  the constructor reads `0`). Same-class reuse is precisely the case a
  class-shape guard cannot see. That points at the reference processor holding a
  pre-GC address for an object that is alive but was relocated by a collector
  whose `pointer_map` did not name it — in which case `update_after_gc` cannot
  repair the record either.
* Not collector-specific: seen under both the default ZGC and
  `-XX:+UseGenerationalGC`.

## Next steps

* The precise fix for the same-class hole is an **identity stamp** rather than a
  shape test: have `ReferenceProcessor` record each entry's `ClassId` (and,
  better, a monotonic registration id written into the object) at
  `discover_reference` time and refuse any write whose stamp no longer matches.
  That closes both the shape case and the same-class case, and would replace the
  four `num_fields >= 2` / class-assignability guards in
  `process_references_after_gc` with one exact one.
* Before that, find out **why a live, strongly-reachable object's address goes
  stale at all**. `watched_pre_gc_addr_survived(addr, pointer_map)` answering
  "survived" for a pre-GC address the collector actually moved is the only way
  the existing guards let one through; a probe that registers N references,
  forces a moving collection, and compares the processor's recorded addresses
  against the post-GC ones would say directly whether the map or the predicate is
  at fault.
* Since the two witnesses here are both the MVStore background writer, a cheaper
  first cut is to run `TestMultiThread` with the background writer disabled
  (`autoCommitBufferSize`/`AUTO_COMMIT` off in the test URL) and see whether the
  failure moves to another thread or disappears — that separates "a background
  thread's roots are not being scanned" from "any thread can see this".

## Repro

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Roughly 1 run in 4 on a loaded host; run at least 6 before calling it absent.
Two greps discriminate it from the retired reference-queue bug, and both must be
empty for this record to be the one you are looking at:

```bash
grep -c "cannot be cast to class org.h2.util.CloseWatcher" err.log   # retired bug
grep -c "Cannot read the array length"                     err.log   # retired bug
```
