# `TestLob` fails past the LOB pipe-stream fix: `MVStoreException: Chunk N not found` and `OverlappingFileLockException`

## Status
**OPEN** — new finding, 2026-07-22, uncovered by
`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md`'s Cluster A fix.
Before that fix, `TestLob` crashed early (in LOB pipe-stream setup, via a
`NoSuchMethodError`) and never reached the code paths described here. Not
yet root-caused — filed for tracking, not a confirmed single defect.

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

Both symptoms are consistent with an MVStore file-handle/lock lifecycle
issue rather than genuine data corruption (case 1's specific chunk-tree
inconsistency could itself be a downstream effect of case 2's class of
problem — an earlier `Store`/`FileStore` not being fully released before a
later one in the same test run reopens the same file) but this has not been
confirmed.

## Scope / what's ruled out
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

## Repro
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

## Related
- `docs/internal/h2-suite-bugs/bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` — the fix that first let `TestLob` run far enough to reach this.
- `docs/internal/h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang.md` — a different, already-tracked MVStore issue in this same suite; not confirmed related, worth checking for a shared cause by whoever picks this up.
