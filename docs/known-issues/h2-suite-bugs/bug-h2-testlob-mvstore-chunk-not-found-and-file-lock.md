# `TestLob` fails past the LOB pipe-stream fix: `MVStoreException: Chunk N not found` and `OverlappingFileLockException`

## Status
**OPEN — two contributing bugs found and FIXED (`dev@7ca5a8a4a`,
2026-07-22), but the same two symptoms still recur at a similar rate**,
so the actual trigger is not yet pinned down. See "Investigation
findings" below for a detailed account of what's been ruled out, what's
confirmed, and the most promising remaining lead — this session made
substantial progress narrowing the search space and any continuation
should start from there rather than from scratch.

## Severity
**MEDIUM** — blocks `org.h2.test.db.TestLob` from a full pass. Two distinct
symptoms seen across separate runs (both after the pipe-stream fix, isolated
`data/` directory, no concurrent access):

1. `testReclamationOnInDoubtRollback` (`TestLob.java:175`):
   ```
   org.h2.jdbc.JdbcSQLNonTransientException: General error: "org.h2.mvstore.MVStoreException: Chunk 18 not found [2.4.249/9]"
   ```
   at `org.h2.mvstore.FileStore.getChunk` / `MVStore.readPage` /
   `MVMap.readPage` / `Page$NonLeaf.getChildPage` /
   `CursorPos.traverseDown` / `RollbackDecisionMaker.decide` /
   `TransactionStore.rollbackTo` — an MVStore on-disk chunk the map's page
   tree still references can't be found, during this test's deliberate
   in-doubt-transaction-rollback scenario.

2. In a separate rerun (same class, fresh `data/` dir): a different, later
   failure —
   ```
   java.nio.channels.OverlappingFileLockException
     at sun/nio/ch/FileLockTable.checkList / .add
     at sun/nio/ch/FileChannelImpl.tryLock
     at org/h2/mvstore/SingleFileStore.lockFileChannel
   ```
   — a second attempt to lock the same MVStore file channel while an
   earlier lock (from the same process) is apparently still held.

Both symptoms occur intermittently across passes of a repeated
insert+shutdown+reopen scenario (see "Fast repro" below) — a probabilistic
race, not a deterministic single-cause bug. Either symptom can occur in the
same run, at roughly the same combined frequency, seemingly interchangeably.

## Fast repro (much faster than the full `TestLob` suite class)

`org.h2.test.db.TestLob.main()` runs its whole `test()` method (~40
sub-tests) in a loop of 10 passes within one JVM process — the doc's
original per-class repro below reproduces this, but a **much faster**
standalone harness isolates just the two methods that matter and
multiplies passes explicitly, letting you hit a failure in ~1-4 passes
(~20-90s) instead of running the whole suite class:

```java
// MinimalTestLobRepro2.java — combines TestLob's testConcurrentSelectAndUpdate()
// (10s of concurrent update+select stress on the SAME db, via two JDBC
// connections and a background thread) followed by
// testReclamationOnInDoubtRollback() (100x 1MB blob insert, autocommit=false,
// PREPARE COMMIT + SHUTDOWN IMMEDIATELY, reopen + ROLLBACK TRANSACTION +
// CHECKPOINT SYNC, reopen + SHUTDOWN COMPACT), run N times in one process
// against jdbc:h2:<dir>/lobrepro2;MV_STORE=TRUE;MAX_COMPACT_TIME=0;LOCK_TIMEOUT=50
// (the exact options org.h2.test.TestDb.getURL() adds — a plain
// DriverManager.getConnection() WITHOUT these does NOT reproduce, even
// after 20 passes; MAX_COMPACT_TIME=0 in particular changes MVStore's
// close-time compaction path, see below).
```
Full source was written to (and can be recreated at)
`apps/h2database/h2/MinimalTestLobRepro2.java` in the investigation
worktree; not committed here since it's a throwaway harness, not product
code — recreate it from this description if picking this up again, or
check for a lingering worktree under `/data/wt-h2-testlob-mvstore-*` on
the Azure host.

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c ".:target/classes" MinimalTestLobRepro2 ./repro-data2 10
```

## Original repro (the full suite class)
```bash
cd apps/h2database/h2
rm -rf data/test*    # ensure an exclusive, empty data dir
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestLob
```
Reproduces both under `--nojit` and with JIT enabled; timing-sensitive
enough that a heavily-loaded host may need >90s (observed real time ranged
25s–180s+ for the same binary/repro across runs under host contention —
account for this before concluding a run "hung").

## Investigation findings (2026-07-22 session)

### Two genuine bugs found and fixed (`dev@7ca5a8a4a`)
Both are real, verified correctness gaps in CratonVM's `FileChannel`
native bridge, independent of H2, but they do **not** fully explain
either symptom (see "Still open" below) — worth keeping regardless.

1. **`native-io/src/file_channel.rs`'s `native_fcimpl_open`** (the shim
   backing `FileChannel.open(Path, Set<OpenOption>, FileAttribute[])`,
   used for every H2 database file open) left the `closer` field
   unconditionally `null`. Real JDK 25's `FileChannelImpl` private
   constructor (confirmed via `javap -c` against this host's real JDK)
   **always** registers a `Cleaner.Cleanable` there when `parent == null`
   (`closer = cleaner.register(this, new Closer(fd))`), which is always
   our case. `FileChannelImpl.implCloseChannel()` unconditionally calls
   `closer.clean()` on that branch, so a null `closer` meant the
   underlying fd was **silently never released** — `invokeinterface` on a
   null receiver doesn't throw here, it just silently no-ops (confirmed
   via debug tracing: `fd_table::close()` fired essentially once across a
   20-pass run with ~28 opens, i.e. a near-total leak). Fixed by
   registering the real Cleaner action.
   - That alone wasn't sufficient: real `FileChannelImpl$Closer.run()`
     calls `FileChannelImpl.fdAccess.close(fd)`, a `SharedSecrets`-style
     static field populated by `java.io.FileDescriptor`'s own class
     initializer — not reliably populated in this bridge's shortcut
     construction path (another silent no-op on a null receiver). Added a
     direct native override for `FileChannelImpl$Closer.run()V`
     (`native_fcimpl_closer_run`) that closes the fd through our own fd
     table instead of relying on that indirection.
   - **Verified**: fd `close()` now fires ~1:1 with `open_read_write()`
     calls (was ~1:14–1:28 before).
2. **`native-api/src/fd_table.rs`'s `close()`**: `FileReadWrite`/`FileRead`
   entries were removed from the table without synchronizing against an
   in-flight read/write that had already cloned the entry's `Arc` before
   the `remove()` (invisible to the table lock) — only `FileWrite`/pipe
   entries got a flush-and-implicit-wait. A caller that doesn't join a
   writer thread before closing (H2 MVStore's
   `FileStore.stopBackgroundThread(waitForIt=false)` on its
   *normal*-shutdown path — used by every ordinary connection close, not
   just `SHUTDOWN COMPACT`) could otherwise race a stray write against
   the file being closed/truncated/reopened. Fixed by acquiring (and
   dropping) the entry's own lock for these two variants too.

### Ruled out (with evidence, so a future session doesn't re-tread this)
- **Retention-time-based premature chunk reclaim**: forced
  `RETENTION_TIME=0` via `SET RETENTION_TIME 0` in a single-pass
  standalone repro — still succeeded. Rules out "CratonVM is slow enough
  that MVStore's wall-clock retention window elapses mid-test."
- **A pure single-pass timing issue**: a minimal single-pass repro of
  just `testReclamationOnInDoubtRollback` (no preceding stress test, no
  multi-pass loop) succeeded 8/8 times, including at default retention
  and at `RETENTION_TIME=0`. The bug needs BOTH the preceding
  `testConcurrentSelectAndUpdate()` stress test AND multiple passes in
  the same process — a single connection's own insert+shutdown+rollback
  lifecycle in isolation is clean.
- **A general/simple virtual-dispatch-to-base-class bug** (matching the
  already-fixed `jit-virtual-dispatch-bail-static-class-bug` family,
  where a JIT MIC-overflow bail path ran a base class's concrete method
  instead of the receiver's override): wrote two escalating synthetic
  repros (plain classes, then generics + a field typed as the abstract
  base + a double-close pattern matching `MVStore.closeStore()`'s own
  compact()-then-finally shape) at up to 3.2M calls under 8 threads —
  dispatch was correct 100% of the time in both. This was the initial
  hypothesis after seeing `close(fd=X)` fire in the native trace with NO
  preceding `release0(fd=X)` (looked exactly like "wrong-method-dispatch
  skipped the subclass's `fileLock.release()` call") — that specific
  observation's real explanation turned out to be the `closer`/fd-leak
  bug above (fixed), not a dispatch bug.
- **`MVStore.compact()`'s own close→reopen-readonly sequence racing
  itself**: traced a full **successful** compact() sequence byte-for-byte
  (`open temp file → one write → release0(original) → open readonly
  reopen of original path → lock succeeds shared`) — release0 always
  completes before the reopen in a clean run, and it's all on one thread
  (no cross-thread race in this specific sequence). Doesn't rule out a
  race in a *different* run, but the mechanism isn't "compact() ordering
  is inherently wrong."
- **Chunk-id allocation race**: added debug logging directly to H2's own
  `FileStore.findNewChunkId()` (`private final ReentrantLock
  serializationLock` protecting `int lastChunkId`) printing
  thread+lock-held+before/after on every call. Across multiple runs
  including ones that hit "Chunk N not found", the chunk-id sequence was
  **always** strictly sequential (1,2,3,...) with `lockHeld=true` on
  every single call, across `H2-serialization` and `main` threads with no
  gaps or duplicates. Rules out "two threads race `++lastChunkId`."
- **A missing/short/overlapping write**: traced every `pwrite_at` call
  (position + length) for the specific connection whose chunk later went
  missing — the write positions were **fully contiguous**
  (`pos[i+1] == pos[i] + len[i]` for every single write, covering byte 0
  through EOF with no gaps) and all landed on one thread. The file's raw
  bytes are complete; this rules out "a chunk's bytes never got written"
  as the direct cause of "Chunk N not found" — whatever's wrong is at the
  metadata/bookkeeping level (`FileStore.getChunk()`'s fallback to
  `layout.get(Chunk.getMetaKey(chunkId))`, which ALSO misses — so it's
  not just an in-memory cache gap either, the persisted layout metadata
  entry for that chunk id is genuinely absent by the time it's queried).
- **The already-fixed FileChannel leak (fixes 1+2 above) as sole cause**:
  after landing both, ran a 20-pass trial (0 failures through pass 3,
  where an earlier build without the fixes reliably failed) then several
  more 5-8 pass trials — both symptoms (`Chunk N not found` at chunks
  8/10/12, `OverlappingFileLockException`) still occurred, at what looks
  like a similar rate to before. **The leaked fd was not the trigger for
  either symptom** — it was a real bug, worth fixing, but a different
  (still unidentified) mechanism is responsible for the doc's actual
  symptoms.

### Most promising remaining lead (not yet pursued to a conclusion)
`FileStore.getChunk(pos)` (`FileStore.java` around line 2018) computes
`chunkId = DataUtils.getPageChunkId(pos) = (int) (pos >>> 38)` from a
packed `long` position value that was written into a page/pointer
somewhere earlier (e.g. the transaction undo log entries that
`TransactionStore.rollbackTo` walks). Given: (a) the chunk-id counter
itself is proven race-free, (b) the file's raw bytes are proven complete
and contiguous, and (c) the persisted layout metadata for the missing
chunk id is genuinely absent (not just an in-memory miss) — the next
thing to check is whether the **`pos` value itself** gets corrupted
somewhere between being computed (`DataUtils.getPagePos`, which packs
chunk id + offset + `encodeLength()`-encoded length into one `long` via
shifts) and being read back (`getPageChunkId`'s `>>> 38`), OR whether the
chunk's own **metadata registration into the `layout` map** (a *separate*
step from writing the chunk's bytes and from allocating its id — look
for where `layout.put(Chunk.getMetaKey(id), ...)` happens, likely during
commit/checkpoint) is what's actually failing to persist for this
specific chunk. This smells like the same general "long/bit-packing
corrupts a specific value" bug family as the already-known
`StringBuilder.append(long)` NaN-bitpattern issue (see
[[h2-residual-triage-20260722-fixes]]/that finding's own doc), though
this case is a *primitive* long the whole way through (not
boxed/generic-dispatched), so that exact mechanism may not directly
apply — worth checking `DataUtils.getPagePos`/`encodeLength` and the
`layout.put` call sites with the SAME kind of targeted Java-side debug
logging technique used above for chunk-id allocation (add a print at
`layout.put(Chunk.getMetaKey(...), ...)` printing the exact key/value,
and cross-reference against what `getChunk()` sees when it fails).

## Scope / what's ruled out (original)
- Not the same mechanism as this doc's parent
  (`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`): no
  `NoSuchMethodError`, no native object field-layout collision signature.
- Not caused by shared-host test contention: reproduced with a private,
  freshly-emptied `data/` directory exclusive to one run (copied the H2
  fixture into an isolated worktree specifically to rule this out — see
  that doc's verification section for the concurrent-host-access gotcha
  this ruled out first).
- The class passes on HotSpot JDK 25 (per the parent doc's original
  Cluster A description) — CratonVM-specific.
- Checked `bug-h2-mvstore-insert-loop-perf-hang.md` for a shared root
  cause (its own "Related" section asked whoever picks this up to check)
  — **not the same root cause**. That doc is pure per-operation
  interpreter throughput (`TransactionMap`/`MVMap.operate` overhead,
  ~90x vs HotSpot); this doc's bugs are file-lifecycle correctness gaps
  in specific native bridges. Both are ultimately downstream of
  CratonVM's general performance gap vs HotSpot widening otherwise-narrow
  race windows, but they are different defects in different code paths.

## Related
- `docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — the fix that first let `TestLob` run far enough to reach this.
- `docs/internal/h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang.md` — a different, already-tracked MVStore issue in this same suite; checked in this session and confirmed NOT the same root cause (see above).
