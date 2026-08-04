# `TestDiskFull` hangs: an upstream H2 transaction-recovery livelock, amplified by CratonVM throughput

## Status
**CLOSED for CratonVM 2026-08-02.** Retired from `docs/known-issues/h2/`: every
CratonVM-side residual this write-up named is closed, and what remains is an
upstream H2 defect in `2.4.249-SNAPSHOT` that reproduces on stock HotSpot and
that we must not patch (`apps/h2database` runs stock code by design).

`org.h2.test.synth.TestDiskFull` therefore **still wedges** in most CratonVM
runs of the 60-iteration form, and that is expected, not a regression. The
whole point of this page is that the next person who sees it does not spend a
day looking for a GC bug. See *Closure, 2026-08-02* at the end for what was
re-verified and what was fixed.

The heap-corruption report that used to cover this class
(`class_id=ClassId(0)` guard burst → `SIGSEGV`/`ClassCastException`) is fixed
and retired separately; see the retired
`bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up.

## Severity
**MEDIUM** — `TestDiskFull` wedges (no progress, ~100 % CPU on `main`) in most
CratonVM runs of the 60-iteration form. It is a suite-time and triage cost, not a
correctness defect in the VM.

## Symptom

`main` makes no progress. A watchdog dump (`CRATONVM_DEFAULT_WATCHDOG_SEC=<n>`)
shows it cycling through, on every sample:

```
org/h2/mvstore/tx/TransactionMap.set   pc=196        <- the do/while below
org/h2/mvstore/tx/Transaction.waitForThisToEnd
org/h2/mvstore/tx/Transaction.isDeadlocked
org/h2/mvstore/tx/TxDecisionMaker.isCommitted
org/h2/mvstore/MVMap.operate
  … under org/h2/engine/Database.addMeta  <- reopening the database
```

The only other thread is `MVStore background writer`, parked. `LOCK_TIMEOUT=100`
in the test's URL never fires.

## The loop, exactly

`TransactionMap.set` (`TransactionMap.java:355`, the `do … while` closing at
`:383`) retries as long as `transaction.waitFor(blockingTransaction, …)` returns
true:

```java
do {
    result = map.operate(k, null, decisionMaker);
    …
    blockingTransaction = decisionMaker.getBlockingTransaction();
    if (decision != MVMap.Decision.ABORT || blockingTransaction == null) { return …; }
    decisionMaker.reset();
    …
} while (timeoutMillis != 0 && transaction.waitFor(blockingTransaction, mapName, key, timeoutMillis));
throw … ERROR_TRANSACTION_LOCKED …;
```

`Transaction.waitForThisToEnd` (`Transaction.java:729`) returns **`true`
immediately** when the blocker's status is `STATUS_CLOSED`, `STATUS_COMMITTED`,
`STATUS_ROLLED_BACK` or has the rollback bit — it only sleeps for a blocker that
is still open. So a blocker parked in `STATUS_COMMITTED` produces an unbounded
busy loop rather than a timeout.

`TxDecisionMaker.decide` (`TxDecisionMaker.java:73`) keeps returning `ABORT`
because `isCommitted(blockingId)` (`:208`) reads
`store.committingTransactions.get().get(id)` — **false** for this blocker — and
therefore latches `blockingTransaction = store.getTransaction(id)`, which is
still non-`null`.

Instrumented, every sample of a wedged run reads:

```
[cvm-spin] n=…  me=2 meStatus=1(OPEN)  blocking=1 blockingStatus=3(COMMITTED)
           slotIsSame=true slotNull=false committingBit=false  map=table.0 key=2
```

## Why the blocker is parked in `STATUS_COMMITTED` forever

`Transaction.commit()` (`Transaction.java:491`):

```java
long lastState  = setStatus(STATUS_COMMITTED);
hasChanges      = hasChanges(lastState);
int previousStatus = getStatus(lastState);
wasActive       = isActive(previousStatus);      // :586 — false for COMMITTED
if (wasActive && hasChanges) {
    store.commit(this, previousStatus == STATUS_COMMITTED);   // <- recovery arg
}
markTransactionEnd();
…
} finally {
    if (wasActive) { close(hasChanges, ex); }    // <- endTransaction()
}
```

`TransactionStore.endLeftoverTransactions()` (`TransactionStore.java:301`) calls
`t.commit()` on every recovered transaction whose status is `STATUS_COMMITTED`.
For exactly those transactions `previousStatus == STATUS_COMMITTED`, so
`isActive` is false, so:

* `store.commit(this, recovery)` is **never** called — the `recovery` argument is
  unreachable dead code, because its own call site is guarded by `wasActive &&`;
* `close()` → `store.endTransaction()` is **never** called, so
  `transactions.set(txId, null)` never runs and the slot stays occupied.

The recovered transaction therefore keeps its map entries locked for the entire
life of the store. Any later write to one of those keys livelocks as above. This
is an upstream H2 defect and is VM-independent — instrumenting the same H2 build
under stock HotSpot shows the identical "commit exits with `wasActive=false`"
event (28 of them across 150 runs).

## Why CratonVM hits it and HotSpot (almost) doesn't

The recovered leftover is only *harmful* if it holds a key that database reopen
rewrites — in practice a low `table.0` (meta) key such as 2 or 3, which
`Database.addMeta` writes while opening. Which key it holds is decided by where
in the logical write sequence `TestDiskFull`'s injected failure lands, and that
is decided by how many **file write operations** the VM performs per unit of
logical work.

Measured warmup write-op count (`Integer.MAX_VALUE - fs.getDiskFullCount()` after
`test(Integer.MAX_VALUE)`), identical logical workload:

| configuration | write ops |
| --- | --- |
| CratonVM, `WRITE_DELAY=3000` (background writer effectively off) | **146** (deterministic, 6/6 runs) |
| stock HotSpot JDK 25, `WRITE_DELAY=10` (the test's own setting) | **181 – 225** |
| stock HotSpot JDK 25, `WRITE_DELAY=1` | **181 – 263** |
| **CratonVM, `WRITE_DELAY=10`** | **652 – 676** |

146 is the logical floor; everything above it is the time-driven MVStore
background writer. CratonVM spends ~8–10× longer per SQL statement (60
iterations: ~1–3 s on HotSpot, 12–20 s on CratonVM), so a 10 ms writer fires
~3.5× more often per unit of work — and the injected failure at write op #`i`
lands at a correspondingly **earlier logical point**, i.e. in the first few meta
rows rather than in table/index creation.

Observed leftover contents match exactly:

* CratonVM leftovers: `mapName=table.0 key=2 / 3 / 4` — rewritten on reopen.
* HotSpot leftovers: `table.0 key=5 / 6 / 7`, `table.5`, `index.8` — not rewritten.

## Controls (all measured 2026-08-01, Azure host, JDK 25, `--Xmx 1g`)

| arm | runs | unapplied COMMITTED leftovers | runs that livelocked |
| --- | --- | --- | --- |
| CratonVM, stock URL, `i = 0…59` | 10 | 20 | **10** |
| CratonVM, `WRITE_DELAY=3000` | 10 | **0** | **0** |
| CratonVM, `i = 250…309` (later injection point) | 12 | **51** | **0** |
| HotSpot, stock URL | 60 | 4 | 0 |
| HotSpot, `WRITE_DELAY=1` | 150 | 28 | 0 |
| HotSpot, 1000-iteration form | 40 | 4 | 0 |

The third row is the decisive one: **2.5× more** of the supposedly guilty state,
and **zero** hangs. The leftover count is not what causes the hang; the key it
holds is. And the second row shows the whole chain is downstream of the
background writer interleaving with commits, which is downstream of per-statement
cost.

`P(0 livelocks | 28 events)` at CratonVM's observed per-event rate (~25 %) is
≈ 3·10⁻⁴, so HotSpot's clean record is not luck — its leftovers genuinely hold
harmless keys.

## What would actually fix it

* **Upstream H2**: `Transaction.commit()` must apply and close a recovered
  `STATUS_COMMITTED` transaction. Dropping the `wasActive &&` guard on the
  `store.commit(this, previousStatus == STATUS_COMMITTED)` call and calling
  `close()` unconditionally restores the behaviour the `recovery` parameter was
  written for. Worth reporting upstream; do **not** patch `apps/h2database`,
  the suite must run stock code.
* **CratonVM**: nothing specific. The exposure shrinks with per-statement
  throughput; it is one more consumer of the general interpreter/JIT gap, and of
  the `JIT compile bailed: code buffer estimate too small` bails this workload
  takes on hot MVStore methods (`ValueDataType.write`, `MVStore.openMap`,
  `TransactionStore$TxMapBuilder.create`).

## Reproducer

`docs/known-issues/repros/h2-testdiskfull-livelock/` — overlay `TestDiskFull`
with a bounded, env-controlled iteration count and per-iteration timing, the two
H2 instrumentation patches (`Transaction`, `TransactionStore`), and the arm
runner. Nothing in the shared `apps/h2database` checkout is modified; the
overlay is prepended to the classpath.

```bash
docs/known-issues/repros/h2-testdiskfull-livelock/build-overlay.sh
DFULL_MAX=60 docs/known-issues/repros/h2-testdiskfull-livelock/run-arm.sh \
    <cratonvm-bin> myarm 10 4 300
```

Roughly 1 run in 1–2 wedges. `CRATONVM_DEFAULT_WATCHDOG_SEC=240` turns a wedge
into a stack dump; the instrumented `Transaction` prints `[cvm-spin]` lines that
name the blocker and its status directly.

## Related

* The retired `bug-h2-testdiskfull-classid0-corruption-segv-cce` write-up — the
  crashes this class used to show, now fixed.
* `!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md` — `Chunk N not
  found` is upstream fault-injection flakiness and shows on HotSpot too (58 of
  150 runs here).

## Closure, 2026-08-02

Re-run against `origin/dev@86a01abf90` on the Azure host (JDK 25, `--Xmx 1g`,
`DFULL_MAX=60`, 10 runs, 4 at a time):

| | 2026-08-01 (as filed) | 2026-08-02 (re-verified) |
| --- | --- | --- |
| runs that livelocked | 10 / 10 | **8 / 10** |
| unapplied `COMMITTED` leftovers | 20 | 15 |
| leftover map/key | `table.0` key 2 / 3 / 4 | `table.0` key 2 / 3 / 4 |

Same defect, same leftovers, same keys. Nothing about the upstream chain moved.

### Residual 1 — the `code buffer estimate too small` bails: CLOSED

The write-up named `ValueDataType.write`, `MVStore.openMap` and
`TransactionStore$TxMapBuilder.create` as methods this workload lost to the
optimizing tier's under-sized code buffer. Across the 10 re-verification runs
there is now **one** such bail in total, on a different method
(`ValueDataType.readValue`), and none on the three named. Closed by
`ebf1cf3131` (*perf/ir-code-buffer-estimate*), which refitted the estimate to
`nodes*64 + calls*1536 + 4096`.

### Residual 2 — what that fix exposed: FIXED

Sizing the buffer correctly let the hot MVStore methods compile, and the
artifacts two of them compiled into were broken. Filed as
`docs/known-issues/jit/unresumable-unconditional-trap-mvmap-20260802.md`
(`InternalError: … refusing side-effecting replay`, which killed
`org.h2.test.db.TestMultiThread` in under 3 s), root-caused and fixed on
2026-08-02:

* `ir_lower` never stamped a method identity into the deopt frames it builds, so
  an optimizing-tier deopt could not be resumed by the call site that triggered
  it and instead surfaced as a hard `InternalError` blamed on an unrelated
  caller, at a bci that does not exist in the method it named;
* `Op::Div`/`Op::Rem` are floating nodes with no control edge, so the scheduler
  could hoist an integer division — **and its trap** — above the branch that
  guarantees a non-zero divisor. `org.h2.util.MemoryEstimator.estimateMemory`
  (H2's `MVMap` key/value size estimator) is the measured case.

`TestMultiThread` now runs 8 of 8 clean where it died 6 of 6 before. Details in
the retired `unresumable-unconditional-trap-mvmap-20260802` write-up.

Fixing it does **not** move this page's livelock: 9 of 12 runs still wedge, with
the same leftover keys. That is the prediction this write-up already makes —
the exposure is set by write-op density, and a JIT-correctness fix does not
change per-statement cost by the ~3.5× that would be needed.

### What is left, and where it lives

Nothing on the CratonVM side. The upstream fix is the one described under
*What would actually fix it*; it is worth reporting to H2, and it must not be
applied to `apps/h2database`.

The operational fact — *`TestDiskFull` wedging in the H2 suite is expected and
is not a VM defect* — is restated in
`docs/known-issues/repros/h2-testdiskfull-livelock/README.md`, which survives
this page and ships the harness that proves it.
