# `TestLob` fails past the LOB pipe-stream fix: `MVStoreException: Chunk N not found` and `OverlappingFileLockException`

## Status
**Split into two independent bugs by this session (2026-07-22):**

1. **`MVStoreException: Chunk N not found` — ROOT-CAUSED, and CONFIRMED
   NOT a CratonVM correctness bug.** Reproduced with `RETENTION_TIME` set
   to an extreme value (disables background chunk reclaim entirely) and
   the symptom **never recurred across two independent trials, ~35
   total pass-attempts** (vs. reliably recurring within 1-4 passes at
   default settings) — see "Confirmed root cause" below. This is a
   genuine (if narrow) time-of-check/time-of-use race in H2's own
   MVStore retention design, made practically reachable by CratonVM's
   documented raw interpreter-throughput gap vs HotSpot (see
   `bug-h2-mvstore-insert-loop-perf-hang.md`) rather than a CratonVM
   defect to fix directly. Downgrading/tracking as a known limitation
   rather than an open CratonVM bug — see that section for what would
   need to change (H2 upstream, or closing CratonVM's general perf gap)
   to eliminate it outright.
2. **`OverlappingFileLockException` — still OPEN, confirmed to be a
   SEPARATE mechanism.** The same `RETENTION_TIME` experiment that fully
   eliminated symptom 1 did **not** reduce this symptom's frequency at
   all (still occurred in both trials, always this exception alone,
   never alongside "Chunk not found"). Two genuine FileChannel
   fd-lifecycle bugs were found and FIXED in this session
   (`dev@7ca5a8a4a`) while investigating this — verified via debug
   tracing to close a real, near-total fd leak — but they did **not**
   reduce this symptom's frequency either. The actual trigger remains
   unidentified; see "Still open" below for what's been ruled out and
   the state of the investigation.

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

### Confirmed root cause: `Chunk N not found` (chunk reclaimed mid-rollback via `pinCount`)
Added debug logging directly to H2's own
`FileStore.saveChunkMetadataChanges()` (prints thread, lock-held,
chunk id, the exact serialized metadata string, and an immediate
`layout.get()` readback right after the `put()`) and reproduced a
"Chunk 8 not found" failure with it active. Findings:

- **The chunk's metadata registration is not the problem**: every single
  `saveChunkMetadataChanges(chunk=8)` call in the failing pass shows a
  perfect immediate readback (the `layout.get()` right after `put()`
  always returns exactly what was just written). The persisted layout
  entry for chunk 8 genuinely existed at multiple points during this
  same pass — it isn't a bit-packing/corruption issue in `pos` or a
  `layout.put()`/read consistency bug (this rules out the previous
  session's leading hypothesis about `DataUtils.getPageChunkId`'s
  `pos >>> 38` bit-packing, or a `layout` map read/write bug — worth
  cutting that avenue short for a future session).
- **The chunk's own metadata visibly transitions towards
  reclaim-eligible within the SAME failing pass, shortly before the
  exception**: chunk 8's serialized string (`chunk:8,...`) carries a
  `pinCount:1` field in earlier registrations during the pass, but the
  LAST registration before "Chunk 8 not found" throws has **no
  `pinCount` field at all** (i.e. `pinCount` reached 0 — see
  `Chunk.asString()`/`Chunk.java`'s `ATTR_PIN_COUNT`, only appended when
  `pinCount > 0`). `pinCount` tracks live pages belonging to
  "single-writer" maps (H2's own comment: this specifically covers the
  transaction/undo-log map, `TransactionStore.UNDO_LOG_NAME_PREFIX` —
  see `MVStore.compact()`'s `createGenericMapBuilder`). `pinCount==0`
  makes `Chunk.isRewritable()` return `true` (`Chunk.java:429`) — the
  chunk becomes eligible for background auto-compaction/rewrite even
  though it *still has 1 live (non-pinned) page* (`livePages:1` in the
  same log line) that the in-progress rollback traversal may still need
  to walk to.
- **Leading theory**: the in-doubt-transaction rollback
  (`TransactionStore.rollbackTo` walking the undo log) processes undo
  entries one at a time; each processed entry supersedes a page in
  chunk 8, decrementing `pinCount` via `Chunk.accountForRemovedPage`.
  Once `pinCount` hits 0 mid-rollback, chunk 8 becomes a legitimate
  rewrite/reclaim candidate to *whatever* background compaction
  mechanism is watching for that (auto-commit background thread,
  `findOldChunks`/fill-rate-triggered rewrite, etc.) — if that
  mechanism runs concurrently with the still-in-progress rollback on
  the main thread and wins the race, it can rewrite/relocate chunk 8's
  last live page and remove chunk 8's own metadata entry from `layout`
  **before** the rollback traversal gets back to it for a different
  (non-undo-log) page reference still living in that same chunk. This
  reads as a genuine (if narrow) race in H2's own retention design —
  presumably present on HotSpot too in principle, but likely never
  wins there because HotSpot's rollback traversal is fast enough
  (matching the *raw interpreter throughput gap* documented in
  `bug-h2-mvstore-insert-loop-perf-hang.md` — same general "CratonVM is
  slow enough to blow open a race window that's negligible on HotSpot"
  theme as this session's earlier, now-fixed FileChannel bugs, just a
  different specific mechanism).
- **Confirmed** by directly testing the theory's prediction: reran the
  fast repro with the JDBC URL's `RETENTION_TIME` set to an extreme
  value (`2000000000` ms, i.e. background chunk reclaim effectively
  disabled for the whole run) — **two independent 15-20 pass trials, 0
  occurrences of "Chunk N not found" in either** (previously reliably
  recurring within 1-4 passes at default `RETENTION_TIME`). Both trials
  *did* still hit `OverlappingFileLockException` at a similar rate to
  before, confirming that symptom is a genuinely separate mechanism
  unaffected by retention/reclaim (see "Still open" below).
- **Conclusion**: this is a genuine H2-level time-of-check/time-of-use
  race — a chunk's `pinCount` reaching 0 makes it reclaim-eligible
  without regard for whether a currently-in-progress, multi-page
  traversal (the in-doubt-transaction rollback) still needs *other*,
  non-pinned data in that same chunk. It most likely exists on HotSpot
  too in principle, but the rollback traversal there is fast enough
  that the background reclaim mechanism essentially never wins the
  race in practice; CratonVM's raw interpreter-throughput gap
  (documented in `bug-h2-mvstore-insert-loop-perf-hang.md` — the same
  workload class, MVStore per-operation overhead) is wide enough to
  flip the odds. **Not something to "fix" directly in CratonVM** short
  of closing that general performance gap or an upstream H2 fix to its
  own retention logic (e.g. pinning a chunk for the full duration of an
  in-progress multi-page rollback, not just for its still-pinned
  single-writer pages) — recorded here as a known, understood,
  practically-unavoidable-at-current-performance limitation rather than
  an open CratonVM defect.

### Still OPEN: `OverlappingFileLockException` (confirmed separate mechanism)
Both `RETENTION_TIME` trials above still hit this exception (never
"Chunk not found") at a similar rate to the pre-fix baseline, so it does
**not** share the chunk-reclaim root cause above, and the two FileChannel
fd-lifecycle fixes landed this session (`dev@7ca5a8a4a`) — verified to
close a real, near-total fd leak — did not reduce its frequency either.
It always occurs alone (in the two logs examined) and always at
`SingleFileStore.lockFileChannel`'s `tryLock` call, i.e. the *Java-level*
`FileLockTable.checkList()` throws it before any native `lock0` call is
even reached (confirmed via `[fdrace]` tracing: the failing `tryLock`
attempt has no corresponding native `lock0` line at all). This means the
JVM's own static, per-file-identity `FileLockTable` still holds a stale
entry from an earlier, supposedly-already-released lock on the *same*
file identity (`FileKey`, i.e. `st_dev`/`st_ino` — confirmed sound
earlier in this doc's investigation history, see the parent NSME doc).
**A follow-up trace run (after the two fixes landed) captured a failing
sequence directly**, narrowing it further:
```
open_read_write(fd=24, ..., create=true)   # rollback connection's store
lock0(fd=24) ... Ok(true)                  # acquires the exclusive lock
pwrite_at(fd=24, ...) x2 [non-main thread] # CHECKPOINT SYNC's write, on H2's own writer thread
close(fd=25)                                # *** no release0/lock0 for fd=24 ever appears ***
[timing] rollback+checkpoint: 1573ms        # the rollback connection's try-with-resources
                                             #   block has just exited -- Connection.close()
                                             #   should have run fileLock.release() by HERE
open_read_write(fd=26, ..., create=true)   # SHUTDOWN COMPACT's own store, SAME path
close(fd=27)
EXCEPTION: OverlappingFileLockException     # no lock0(fd=26) attempt appears at all --
                                             #   the Java-level FileLockTable.checkList()
                                             #   threw before reaching native code
```
`fd=24` (the connection that actually holds the exclusive lock) never
shows a corresponding `release0` anywhere in the trace, even though the
log's own timing print confirms its try-with-resources block *has*
already exited by the time the next connection opens. This means
`SingleFileStore.close()`'s explicit, supposed-to-be-synchronous
`fileLock.release()` call either isn't running for this connection, or
runs but doesn't reach the native `release0` this session's fix wired
up — i.e. the JVM-level `FileLockTable` removal for this specific lock
is not happening via the deterministic `Connection.close()` path, and
(per this session's *other* fix, the `Cleaner`/`Closer.run()` one) may
only be getting cleared later, asynchronously, whenever the
Cleaner-triggered fd close eventually runs — which can lose the race
against the very next `tryLock()` moments later. `fd=25`/`fd=27` (both
closed but never seen in the `open_read_write` trace at all) are
unexplained — likely allocated via a different, untraced fd-allocating
code path, and may themselves be part of the answer (e.g. a
retried/duplicate open attempt for the `SHUTDOWN COMPACT` connection
after its own first `tryLock()` also failed against fd=24's
still-held lock).

**Next steps for whoever continues, not yet tried**:
1. Add H2-side Java debug prints directly to `SingleFileStore.close()`
   and `FileLock.release()` (the same technique that successfully
   root-caused the chunk-not-found symptom) to confirm definitively
   whether the rollback connection's explicit, synchronous release path
   runs at all, and if so, whether it actually reaches the native
   `release0` this session wired up.
2. Trace ALL fd-allocating functions simultaneously (not just
   `open_read_write` — also `open_random_access`, `open_read`,
   `open_write`, etc.) to resolve what `fd=25`/`fd=27` actually are.
3. Revisit whether `MVStore.FileStore.stopBackgroundThread(waitForIt=false)`
  (used on every *normal* connection close, not just `SHUTDOWN COMPACT`
  — see `MVStore.closeStore()`) still leaves a background writer thread
  racing a subsequent open/lock/close cycle, now that the fd itself
  reliably closes (this session's fix) — i.e. whether the *Java-level*
  `FileLockTable` bookkeeping (`fileLockTable().remove(fli)`, called from
  `FileChannelImpl.release()` only *after* `nd.release()` succeeds) can
  still be skipped or raced by a background thread's own, separate
  `FileLock`/`FileChannel` instance if MVStore ever opens more than one
  `FileChannel` on the same path concurrently (worth grepping for that).

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
