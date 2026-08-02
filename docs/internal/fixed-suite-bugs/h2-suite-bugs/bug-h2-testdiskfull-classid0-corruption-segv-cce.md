# `TestDiskFull` — `class_id=ClassId(0)`/`num_slots=0` guard burst, `SIGSEGV`, `ClassCastException`, and the "hang"

## Status
**✅ RESOLVED / RETIRED (2026-08-01).** The three symptoms this report filed are
closed:

| symptom | disposition |
| --- | --- |
| 1 — `gen_heap::set_field`/`get_field` guard burst, `class_id=ClassId(0)` | **FIXED.** Reproduces 2 runs in 42 on a `dev` build from immediately *before* the post-GC reference-processing fix; **0 runs in 330** on current `dev`. |
| 2 — `cratonvm.synthetic.AnonymousObject$3` cast to `[Ljava/lang/String;` | **Not reproducible** anywhere in the same census (0 `ClassCastException`, 0 `AnonymousObject`, 0 `corrupt Value cell`, 0 `AbstractMethodError` — on either build). |
| 3 — >300 s "hang" | **Root-caused, and it is not a CratonVM defect.** An upstream H2 transaction-recovery livelock, reached far more often on CratonVM because CratonVM issues ~3.5× more file write operations for the same logical work. See the retired `h2-testdiskfull-upstream-transaction-recovery-livelock` write-up. |

One residual is **handed over, not closed**: a single `SIGSEGV` in those 330
runs, with a different signature from this report's (see below). It is handed to
`docs/known-issues/h2/bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`.

The `Chunk N not found` failures this report already attributed to upstream H2
fault-injection flakiness are confirmed again: stock HotSpot JDK 25 hit them in
58 of 150 runs under the same settings.

---

## Symptoms 1 and 2 — the A/B

The measurements in the original report were taken on 2026-07-31 with a binary
built from `dev` plus the `TestGetGeneratedKeys` wrapper-`equals` fix. The two
commits that fix this guard signature — `c0d09e2451` ("post-GC reference
processing wrote through stale old-gen addresses") and `b86945eafe` ("the refproc
staleness guard had the same old-gen blind spot") — reached `dev` **after** that,
so the report's arms never contained them.

Both builds run the **stock, unmodified** `org.h2.test.synth.TestDiskFull` (no
overlay), `--Xmx 1g`, JDK 25, fresh scratch CWD per run, Azure host,
2026-08-01:

| build | runs | runs with `gen_heap` OOB guard hits | guard hits | runs reporting an all-zero-header stale pointer | `SIGSEGV` |
| --- | --- | --- | --- | --- | --- |
| `dev@22107d5122` — the last `dev` state **before** the reference-processing fix | 42 | **2** | 14 | **3** | 0 |
| `dev@c8a3ba181d` — current | **330** | **0** | **0** | **0** | 1 (different signature, below) |

The pre-fix build reproduces the reported shape verbatim, including the
consequence:

```
gen_heap::get_field: out-of-bounds field read dropped … num_slots=0 class_id=ClassId(0)
Stale pointer detected in invokevirtual receiver (ptr=0x200102130e0, all-zero header)
Thread Thread-610 terminated with error: ExceptionThrown(…)
```

Strictly, other commits also landed in the window between those two builds; what
the A/B establishes is that the signature was fixed *in that window*, and the
reference-processing fix is the change in it whose documented cause is exactly
this signature (`Reference` and referent allocated back to back is also where the
report's constant `0xe0` receiver-to-value delta comes from — it is the pair
spacing, not a miscomputed field offset, which disposes of the report's third
"suggested next step").

A further **265 runs** of a bounded-iteration overlay form of the same class on
current `dev` (JIT and `--nojit`) produced 0 guard hits, 0 `SIGSEGV`, 0
`ClassCastException`, 0 `corrupt Value cell`. In that census the 24 `SIGABRT`s
are all our own `CRATONVM_DEFAULT_WATCHDOG_SEC` aborts, deliberately armed in
three arms to dump stacks on the livelock; the fatal-error banner appears in
exactly those 24 logs and nowhere else.

The `AbstractMethodError` this report explicitly declined to carry forward
(`org/h2/value/Value.getValueType()I has no Code attribute`) did not appear on
either build.

## The one residual crash — handed over

One run in 330 on current `dev` died with:

```
#  SIGSEGV at pc=0x7e65e95ad765, addr=0x0, pid=930353
#  fault pc is inside a RECENTLY FREED code buffer: … active_jit_executions_at_free=0x0
#  fault pc is inside a LIVE registered code buffer: base=0x7e65e95ad000 cap=0x3240
#  maps: fault pc IS MAPPED - perms are on the `here` line   (r-xp)
#  slot[r10]: 0x0 0x0 0x0 0x0 0x0 0x0 0x0 0x0        r10=0x2003a0dd800
```

This is **not** this report's symptom 1: `oob=0` for that run, no guard burst, no
`corrupt Value cell`. `addr=0x0` with a mapped executable `pc` is a *data* fault
inside compiled code, not an instruction fetch off unmapped code, so the
freed/live code-buffer lines are the recycled-address artefact the crash handler
warns about, not a use-after-free. What is suspicious is `r10` pointing at eight
zero words — an **all-zero object header**, the premature-reclamation shape.

Not root-caused: it is one unreproduced sample, and 203 further runs armed with
`CRATONVM_DBG_SWEEP_ZERO=1` (the reclaimed-live-object ring, which self-diagnoses
on a hit) produced no second occurrence and no ring hit. Recorded in
`bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep.md`, which owns that
family; `TestDiskFull` at ~73 s per run may be a cheaper handle on it than
`TestMVStoreCachePerformance` at 12–40 min, if the rate holds up.

---

## Symptom 3 — the "hang" is a real livelock, in upstream H2

Root-caused in full on 2026-08-01. Full write-up, evidence and controls:
the retired `h2-testdiskfull-upstream-transaction-recovery-livelock` write-up.
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
  True of the crashes. Not true of the livelock's underlying defect, which is
  upstream and which HotSpot demonstrably also walks into.

## Reproducer

The harness used for all of the above — the overlay `TestDiskFull` with bounded
iteration count and per-iteration timing, the two H2 instrumentation patches, and
the arm runner — is committed under
`docs/known-issues/repros/h2-testdiskfull-livelock/`. The A/B in this document
used the **stock** class, not the overlay.
