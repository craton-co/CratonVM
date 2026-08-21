# `TestLob` fails past the LOB pipe-stream fix: `MVStoreException: Chunk N not found` and `OverlappingFileLockException`

## Status

**HotSpot control, 2026-08-10 — the H2 race fires on stock HotSpot too.**
This page attributes symptom 1 to a real H2-level chunk-reclaim race "made
practically reachable by CratonVM's documented raw interpreter-throughput gap".
A stock HotSpot 25 control over the H2 non-passing set on the same host, same
build, same classpath, same 300 s cap now shows **HotSpot failing `TestLob`
with the same `MVStoreException: Chunk 6 not found`**. So the race does not
need CratonVM's throughput gap to be reachable — it is reachable on this host
under load on the reference VM. That strengthens the page's conclusion (not a
CratonVM defect) and weakens only its explanation of *why* it shows up. Caveat
worth carrying: that control shared the host with three concurrent CratonVM
suite runs, so it establishes "reachable under load on HotSpot", not
"reachable on an idle HotSpot".
See `internal/fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` §4.

**CLOSED (2026-07-23 follow-up session) — both symptoms are now understood
to be the SAME underlying H2-level mechanism, and are not open CratonVM
defects.** One genuine, independent CratonVM bug was found and FIXED along
the way. See "2026-07-23 follow-up: unifying the two symptoms" below for
the full writeup; the 2026-07-22 session's findings are preserved beneath
it for history.

1. **`MVStoreException: Chunk N not found` — ROOT-CAUSED, and CONFIRMED
   NOT a CratonVM correctness bug.** A genuine (if narrow)
   time-of-check/time-of-use race in H2's own MVStore retention design
   (a chunk's `pinCount` reaching 0 makes it reclaim-eligible without
   regard for an in-progress multi-page traversal still needing other
   data in that chunk), made practically reachable by CratonVM's
   documented raw interpreter-throughput gap vs HotSpot (see
   `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`) rather than a CratonVM
   defect to fix directly. Tracked as a known limitation — see "Confirmed
   root cause" below for the full analysis, and that section for what
   would need to change (H2 upstream, or closing CratonVM's general perf
   gap) to eliminate it outright.
2. **`OverlappingFileLockException` — ROOT-CAUSED, and CONFIRMED NOT a
   CratonVM correctness bug (2026-07-23 follow-up).** Turns out to be a
   downstream CONSEQUENCE of the exact same chunk-reclaim race as symptom
   1, just reached via a different call path: H2's own
   `Database.closeOpenFilesAndUnlock()` calls `lobStorage.close()`
   *before* `store.close()` with no exception isolation between them; when
   `LobStorageMap.cleanup()`'s stale-LOB walk (invoked from
   `lobStorage.close()`) hits the same "Chunk N not found" race, the
   `MVStoreException` propagates out and is silently swallowed by
   `Database.closeImpl()`'s catch block — so `store.close()` (which
   releases the MVStore's `FileLock`/`FileChannel`) never runs for that
   connection. The connection's own `close()` call returns normally; the
   *next* connection to open the same file then hits
   `OverlappingFileLockException` because the JVM-level `FileLockTable`
   entry was never removed. Confirmed directly via a captured
   `lobrepro2.trace.db` stack trace (H2's own trace log) showing exactly
   this call chain. Not a CratonVM defect — the same H2-level race as
   symptom 1, just observed through a second code path.
   - **One genuine, independent CratonVM bug WAS found and FIXED along the
     way** (`FileChannel.close()`/`isOpen()` native-dispatch shadowing —
     see below): real, worth keeping, measurably reduced (but did not
     eliminate) this symptom's observed frequency, since the residual
     mechanism is the H2-level race above, not this bug.

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
  `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` — same general "CratonVM is
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
  (documented in `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` — the same
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

## 2026-07-23 follow-up: unifying the two symptoms

Picked up the doc's own "Next steps for whoever continues" list from the
2026-07-22 session. Worked in the SAME worktree/branch
(`/data/wt-h2-testlob-mvstore-20260722`, `fix/h2-testlob-mvstore-20260722`
on the Azure host), rebased onto latest `dev`.

### Genuine CratonVM bug found and FIXED: `FileChannel.close()`/`isOpen()` native-dispatch shadowing

Per the previous session's step 1 ("add H2-side debug prints to
`SingleFileStore.close()`/`FileLock.release()`"), added exactly that
(gated on `CRATONVM_DBG_FDRACE`, throwaway H2-side prints — `apps/` is
gitignored, not committed) plus native-side tracing across every
fd-opening function (`open_random_access`, `open_read`, `open_write`,
`open_read_write`, not just the previously-traced `open_read_write`) and
`lock0`/`release0`/`Closer.run()`. First finding: a minimal
`FileChannel.open(path,...); fc.close();` repro showed `fc.isOpen()`
still returning **`true`** after `close()`, and the underlying fd/lock
were only released later, asynchronously, on a *different* thread — a
background JVM Cleaner thread, not the calling thread.

Root cause: `close()V` is native-registered on the literal abstract class
`java/nio/channels/FileChannel` (`native-io/src/lib.rs`,
`native-builtins/src/phases_late.rs`, two separate registrations found —
see "duplicate-native-registrations" family of pre-existing issues) to
service a fully-synthetic single/2-field fallback `FileChannel` object
that a couple of code paths construct directly as an instance of that
literal class. But `close()` is declared `final` in the *grandparent*
`AbstractInterruptibleChannel` (`FileChannelImpl extends FileChannel
extends AbstractInterruptibleChannel`) — `FileChannel` itself has no
`close()` bytecode of its own, it's pure inheritance. The VM's method
resolution, when walking up the hierarchy to resolve this inherited
method for a REAL `sun/nio/ch/FileChannelImpl` instance (built by
`native_fcimpl_open` in real-JDK mode — i.e. every H2 MVStore file),
stops at the first ancestor with ANY native registered under the same
name+descriptor, even though `FileChannel`'s own classfile never declares
`close()` — so it never reaches the real
`AbstractInterruptibleChannel.close()` bytecode. This silently skipped
`implCloseChannel()` entirely: the `fileLockTable` release loop and the
registered Cleaner cleanup both never ran on the calling thread, `closed`
never flipped to `true`, and the underlying fd/OS lock were only actually
released whenever the JVM's background Cleaner thread happened to notice
the object was unreachable and run its phantom-cleanup action —
completely decoupled from when Java code believed the channel was closed.

**Fixed** (`dev` — commit lands via this doc's branch merge) by having
both `close()`/`isOpen()` registrations detect a real instance (any
runtime class other than the literal synthetic `java/nio/channels/FileChannel`)
and replicate `AbstractInterruptibleChannel.close()`/`isOpen()`'s exact
contract instead: idempotent on a `closed` field, then invoke the real
`implCloseChannel()` bytecode (never itself intercepted by a native) via
`ctx.invoke_virtual`. **Verified**: a direct synchronous-close repro
(`FileChannel.open()`/`RandomAccessFile.getChannel()` + immediate
`close()`) now shows the Cleaner action firing synchronously on the
calling thread and `isOpen()` correctly reporting `false` immediately
after `close()` returns, in both the `hc0053dbg` iteration profile and a
final `release` build (`CARGO_PROFILE_RELEASE_LTO=off -j4` per the
fat-LTO-OOM mitigation for this shared host).

This is a real, independent resource-lifecycle correctness bug — kept
regardless of its effect on this doc's specific symptom, matching this
doc's own precedent from the 2026-07-22 session's two FileChannel
fd-leak fixes.

### Why `OverlappingFileLockException` still recurred after the fix — and its real root cause

Re-ran the fast repro (with and without `RETENTION_TIME=2000000000`) for
dozens of independent trials after the close() fix landed. The exception
still recurred, but at a MUCH lower observed rate than the 2026-07-22
baseline ("reliably recurring within 1-4 passes") — roughly 1-in-10-to-20
single-pass trials in this session's sampling, consistent with the fix
closing one real contributing timing window without being the doc's
primary cause.

Added debug prints (H2-side, throwaway, `apps/` gitignored) to
`Database.removeSession()`/`closeImpl()`/`closeOpenFilesAndUnlock()` and
`org.h2.mvstore.db.Store.close()`/`MVStore.closeStore()` to trace the
full close call chain. Captured a failing sequence showing
`Database.closeImpl()` entering, removing the system/lob sessions, and
then — for the specific connection that goes on to cause the next
connection's `OverlappingFileLockException` — **never** reaching
`Store.close()`/`MVStore.closeStore()`/`SingleFileStore.close()` at all,
with no exception visible on stderr (successful runs, by contrast, show
the full `storeclose`→`mvstoreclose`→`sfsclose` chain completing every
time).

The missing piece was H2's own trace log
(`lobrepro2.trace.db`, produced because `Database.closeImpl()`'s outer
`catch (DbException | MVStoreException e) { trace.error(e, "close"); }`
logs — but does not rethrow — anything caught there). Reading it directly
after a run classified as a plain "Chunk not found" crash (the JVM
process exits before the *next* connection would ever attempt the lock
that would have surfaced as `OverlappingFileLockException`) turned up the
exact mechanism:

```
2026-07-23 ... database: close
org.h2.mvstore.MVStoreException: Chunk 15 not found [2.4.249/9]
	at org.h2.mvstore.MVStoreException.<init>(MVStoreException.java:18)
	at org.h2.mvstore.DataUtils.newMVStoreException(DataUtils.java:996)
	at org.h2.mvstore.FileStore.getChunk(FileStore.java:2042)
	at org.h2.mvstore.FileStore.readPage(FileStore.java:2007)
	at org.h2.mvstore.MVStore.readPage(MVStore.java:1173)
	at org.h2.mvstore.MVMap.readPage(MVMap.java:632)
	at org.h2.mvstore.Page$NonLeaf.getChildPage(Page.java:1178)
	at org.h2.mvstore.CursorPos.traverseDown(CursorPos.java:90)
	at org.h2.mvstore.MVMap.operate(MVMap.java:1893)
	at org.h2.mvstore.MVMap.remove(MVMap.java:517)
	at org.h2.mvstore.db.LobStorageMap.doRemoveLob(LobStorageMap.java:478)
	at org.h2.mvstore.db.LobStorageMap.cleanup(LobStorageMap.java:447)
	at org.h2.mvstore.db.LobStorageMap.close(LobStorageMap.java:436)
	at org.h2.engine.Database.closeOpenFilesAndUnlock(Database.java:1335)
	at org.h2.engine.Database.closeImpl(Database.java:1296)
	at org.h2.engine.Database.close(Database.java:1204)
	...
	at org.h2.jdbc.JdbcConnection.close(JdbcConnection.java:390)
```

`Database.closeOpenFilesAndUnlock()`'s own source (unchanged H2 code):

```java
private synchronized void closeOpenFilesAndUnlock() {
    try {
        if (lobStorage != null) {
            lobStorage.close();               // <-- can throw MVStoreException
        }
        if (store != null && !store.getMvStore().isClosed()) {
            ...
            store.close(allowedCompactionTime);   // <-- releases the FileLock; SKIPPED if the line above threw
            ...
        }
    } finally {
        if (lock != null) { lock.unlock(); lock = null; }   // the OLD-STYLE .lock.db mechanism, NOT the MVStore FileLock
    }
}
```

**Root cause, fully unified**: `lobStorage.close()` → `LobStorageMap.cleanup()`'s
stale-LOB-removal walk hits the exact same H2-level MVStore
`pinCount`-based chunk-reclaim race already root-caused for symptom 1
(`Chunk N not found` — a chunk becomes reclaim-eligible while a
still-in-progress multi-page traversal needs other data in it). When it
strikes here specifically (during a connection's close sequence, in
`lobStorage.close()`, which runs *before* `store.close()` with no
exception isolation between them), the resulting `MVStoreException`
propagates out of `closeOpenFilesAndUnlock()` and is silently caught and
logged (not rethrown) by `Database.closeImpl()`'s outer catch — so
`store.close()` (and therefore `SingleFileStore.close()` → `fileLock.release()`
→ the JVM-level `FileLockTable.remove()`) never runs for that connection.
The connection's own `Connection.close()` call returns *normally* — no
exception reaches the test/caller — while the underlying MVStore file and
its `FileLock` are left open. The next connection that opens the same
file then hits `OverlappingFileLockException`, because the stale
`FileLockTable` entry for that file identity was never removed.

This is **the same race as symptom 1**, just reached via a second,
independent call path (`LobStorageMap.cleanup()`'s traversal during
close, instead of `TransactionStore.rollbackTo()`'s traversal during an
explicit rollback) — not a second, distinct CratonVM defect. Per symptom
1's disposition, it is a genuine (if narrow) gap in H2's own design (both
the underlying retention race, and `closeOpenFilesAndUnlock()`'s lack of
exception isolation between `lobStorage.close()` and `store.close()` —
even on HotSpot, if this race fired here, the same `FileLock` leak would
occur; it just wins astronomically less often on HotSpot given
CratonVM's documented interpreter-throughput gap widening the window).
**Not something to fix in CratonVM directly** — patching H2's own
`Database.java` is out of scope for a CratonVM fix (and `apps/h2database`
isn't part of this repo regardless, per `.gitignore`); the one concrete,
CratonVM-side action item (closing the general interpreter-throughput
gap) is already tracked separately in `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md`.

**Disposition**: both documented symptoms are CLOSED as understood,
non-CratonVM-bug H2-level limitations. The one genuine CratonVM defect
uncovered in the process (`FileChannel.close()`/`isOpen()` native-dispatch
shadowing) is fixed and merged.

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
- Checked `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` for a shared root
  cause (its own "Related" section asked whoever picks this up to check)
  — **not the same root cause**. That doc is pure per-operation
  interpreter throughput (`TransactionMap`/`MVMap.operate` overhead,
  ~90x vs HotSpot); this doc's bugs are file-lifecycle correctness gaps
  in specific native bridges. Both are ultimately downstream of
  CratonVM's general performance gap vs HotSpot widening otherwise-narrow
  race windows, but they are different defects in different code paths.

## Related
- A cross-class-dispatch `NoSuchMethodError` fix (since archived) is what first let `TestLob` run far enough to reach this.
- A separate, already-tracked MVStore issue in this same suite — pure per-operation interpreter throughput (`TransactionMap`/`MVMap.operate` overhead, ~90x vs HotSpot) — was checked in this session and confirmed NOT the same root cause (see above).
