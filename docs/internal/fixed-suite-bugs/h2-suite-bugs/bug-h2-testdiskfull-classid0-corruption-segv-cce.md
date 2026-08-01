# `TestDiskFull` — `class_id=ClassId(0)`/`num_slots=0` guard burst, `SIGSEGV`, `ClassCastException`, and the "hang"

## Status
**✅ RESOLVED / RETIRED (2026-08-01).** All three symptoms this report filed are
closed, two of them as *fixed* and one as *not a CratonVM defect*:

| symptom | disposition |
| --- | --- |
| 1 — `gen_heap::set_field` guard burst then `SIGSEGV` | **FIXED** on `dev` by the post-GC reference-processing fix (`c0d09e2451` + `b86945eafe`), which landed **after** this report was written. Not reproducible in 265 runs. |
| 2 — `cratonvm.synthetic.AnonymousObject$3` cast to `[Ljava/lang/String;` | **Not reproducible** in the same 265 runs (0 `ClassCastException`, 0 `AnonymousObject`, 0 `corrupt Value cell`). Same guard, same cause as (1). |
| 3 — >300 s "hang" | **Root-caused, and it is not a CratonVM bug.** An upstream H2 transaction-recovery livelock, reached far more often on CratonVM because CratonVM issues ~3.5× more file write operations for the same logical work. Recorded, with the controls, in `docs/known-issues/h2/h2-testdiskfull-upstream-transaction-recovery-livelock.md`. |

The `Chunk N not found` failures this report already attributed to upstream H2
fault-injection flakiness are confirmed again here: stock HotSpot JDK 25 hit
them in 58 of 150 runs under the same settings.

---

## Symptoms 1 and 2 — gone

The measurements in the original report were taken on 2026-07-31 with a binary
built from `dev` plus the `TestGetGeneratedKeys` wrapper-`equals` fix. The two
commits that actually fix this guard signature were authored at 15:51 and 17:21
UTC that day and merged into `dev` at **17:49 UTC** — i.e. *after* the runs
recorded here (this file was committed at 16:53 UTC). The report's arms
therefore never contained the fix.

Re-measured on `dev@c8a3ba181d` (2026-08-01), release build, JDK 25,
`--Xmx 1g`, Azure host:

| | runs | `set_field`/`get_field` OOB guard hits | `SIGSEGV` (rc=139) | `ClassCastException` | `corrupt Value cell` | `AbstractMethodError` |
| --- | --- | --- | --- | --- | --- | --- |
| CratonVM, JIT on and `--nojit`, with and without the diagnosis overlay | **265** | **0** | **0** | **0** | **0** | **0** |

Exit-code census over those 265 runs: 160×0, 20×1 (`Chunk N not found`),
61×137 (killed at the harness timeout — see symptom 3), 24×134. All 24 of the
`134`s are `SIGABRT` raised **by our own watchdog** (`CRATONVM_DEFAULT_WATCHDOG_SEC=240`,
set deliberately in three arms to dump stacks on the livelock); the fatal-error
banner appears in exactly those 24 logs and nowhere else. There was no
spontaneous crash of any kind.

Both fixes are described in the retired
`bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family` write-up: post-GC
reference processing was writing referent pointers through **stale old-generation
addresses**, which is precisely the "receiver reads back as a bare
`java/lang/Object` with zero fields" shape reported here (`index=0`,
`num_slots=0`, `class_id=ClassId(0)`, value an `ObjectRef` a fixed distance
below the receiver — a `Reference` and its referent allocated back to back).
That also disposes of this report's third "suggested next step": the constant
`0xe0` delta was the referent/reference pair spacing, not a miscomputed field
offset.

The `AbstractMethodError` this report explicitly declined to carry forward
(`org/h2/value/Value.getValueType()I has no Code attribute`) did not appear
either.

---

## Symptom 3 — the "hang" is a real livelock, in upstream H2

Root-caused in full on 2026-08-01. Full write-up, evidence and controls:
`docs/known-issues/h2/h2-testdiskfull-upstream-transaction-recovery-livelock.md`.
Summary:

1. H2's MVStore background writer (`WRITE_DELAY=10`, set by this test's URL)
   flushes the store **while a transaction commit is in flight** — after
   `TransactionStore.markUndoLogAsCommitted` has marked the undo log committed
   but before the log has been applied and cleared.
2. `TestDiskFull`'s injected disk-full failure then makes the following writes
   fail, so the persisted image keeps that undo log in the marked-committed,
   not-applied state.
3. On reopen, `TransactionStore.init` recovers it as a `STATUS_COMMITTED`
   leftover. `endLeftoverTransactions()` calls `Transaction.commit()` on it, and
   there `wasActive = isActive(previousStatus)` is **false** (previous status is
   already `STATUS_COMMITTED`), so *neither* `store.commit(this, recovery)` *nor*
   `close()` runs. The recovery-commit argument
   `store.commit(this, previousStatus == STATUS_COMMITTED)` is dead code, because
   the call is guarded by `wasActive &&`. The transaction stays in its slot
   forever with its map entries still locked.
4. Any later transaction that touches one of those keys spins: `TxDecisionMaker.decide`
   returns `ABORT` (blocking transaction non-null, committing bit clear), and
   `Transaction.waitForThisToEnd` returns **`true` immediately** because the
   blocker's status is `STATUS_COMMITTED` — so `TransactionMap.set`'s
   `do { … } while (… transaction.waitFor(…))` never terminates and `LOCK_TIMEOUT`
   can never fire.
5. CratonVM reaches the *harmful* variant of step 3 (a leftover holding a **low
   `table.0` meta key** that database reopen rewrites) far more often, because
   its wall-clock cost per SQL statement is ~8–10× HotSpot's, so the 10 ms
   background writer interleaves with commits ~3.5× more often. Measured warmup
   write-op counts for identical logical work: **146** with the background writer
   effectively off, **181–225** on HotSpot, **652–676** on CratonVM.

Two independent controls remove the hang on CratonVM without touching the VM:
`WRITE_DELAY=3000` (10/10 pass, zero leftovers) and starting the fault-injection
loop at `i=250` instead of `i=0` (12 runs, 0 spins, despite **2.5× more**
unapplied committed leftovers). Stock HotSpot with `WRITE_DELAY=1` produced 28
unapplied committed leftovers across 150 runs and never livelocked, because its
leftovers hold high `table.0`/`table.5`/`index.8` keys the reopen path does not
rewrite.

---

## Corrections to the original report

* **"A fresh CWD makes the run ~2 s."** Not the mechanism. The short form
  happens when `test(Integer.MAX_VALUE)` takes its `catch (SQLException)` path,
  which resets the counter to 0, so `Integer.MAX_VALUE - 0 + 10` overflows
  negative and the loop is skipped. It has nothing to do with the working
  directory; it is a coin flip on whether the warmup throws (5 of 12 CratonVM
  runs, on the runs measured here). When the warmup succeeds the loop really does
  run 1000 iterations.
* **"15–16 of 60 timeouts."** Two different things were being counted as one.
  Some are the plain throughput gap — the full 1000-iteration form takes 25–37 s
  on HotSpot and did not finish in 2400 s on CratonVM. The rest are the hard
  livelock above, which makes *any* timeout value expire.
* **"`SIGSEGV`, the 300 s hangs and the `ClassCastException` are CratonVM-only."**
  True of the crashes (now fixed). Not true of the livelock's underlying defect,
  which is upstream and which HotSpot demonstrably also walks into.

## Reproducer

The harness used for all of the above — the overlay `TestDiskFull` with bounded
iteration count and per-iteration timing, the two H2 instrumentation patches, and
the arm runner — is committed under
`docs/known-issues/repros/h2-testdiskfull-livelock/`.
