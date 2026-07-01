# `TransactionTimeoutTest` — Narayana transaction-reaper never fires → `ConcurrentHashMap.entrySet()` was a dead snapshot

**Severity:** Medium (one Hibernate JTA test; but the root cause — CHM `entrySet()`
write-through — is a general native-collections correctness gap)
**Status:** FIXED (`native-collections/src/lib.rs`: `native_chm_entry_set` +
`native_hs_remove` entrySet branch)
**Binary:** `C:/craton/CratonVM/.claude/worktrees/pensive-cartwright-53b832/target/release/cvfix.exe`
**HotSpot:** not affected (JDK 25 passes)
**Surfaced by:** `org.hibernate.orm.test.jpa.transaction.TransactionTimeoutTest#testH2`
(misc16 triage §11; `docs/known-issues/HIB-misc16-correctness-sweep.md`)

## Symptom

`testH2` sets a 2 s JTA timeout, `begin()`s, then runs `select sleep(10000)` (an
H2 alias → `Thread.sleep(10000)`). Expected: a `QueryTimeout`/`LockTimeout`, or a
final `transactionManager.getStatus()` in
`{ROLLEDBACK=4, ROLLING_BACK=9, MARKED_ROLLBACK=1}`. Actual
(`TransactionTimeoutTest.java:144`):

```
AssertionError: Expecting actual: 0 to be in: [4, 9, 1]
```

`getStatus()==0` (`STATUS_ACTIVE`): the alias slept the full 10 s and the
transaction was never marked/rolled-back. The JTA platform is Arjuna/Narayana
(narayana-jta 7.3.4.Final); its `TransactionReaper` background daemon that watches
per-transaction deadlines never rolled the timed-out transaction back.

## Investigation (what it was NOT)

Isolated the reaper in a standalone probe (`apps/hib-suite-runner/TReaperProbe.java`)
— no Hibernate stack, just `com.arjuna.ats.jta.TransactionManager` +
`setTransactionTimeout(2)` + `begin()` + a status poll. It reproduced cleanly
(status stuck at 0), and reflection into the reaper ruled out the obvious guesses:

- The reaper daemon threads **do** start (`CRATONVM_DBG_THREADSTART=1` shows both
  `ReaperThread` (tid 5) and `ReaperWorkerThread` (tid 6)). Not a thread-scheduling
  gap — CratonVM spawns a real OS thread per Java thread.
- `setTransactionTimeout` **is** wired: after `begin()`,
  `reaper.numberOfTransactions()==1` and the deadline is registered
  (`checkingPeriod` counts down 1998→998→−3 across the first 2 s).
- Timed `Object.wait(ms)` wakes correctly on timeout (`monitor.rs::wait`).

The tell: on cvmisc, at ~t=3 s the reaper's `nextDynamicCheckTime` jumped to
`Long.MAX_VALUE` (i.e. `TransactionReaper.check()` ran and took the
`getFirst()==null` branch) **while `_timeouts` still held the element with
`_status=0`**. So `check()` saw an *empty* sorted queue even though the transaction
was still registered.

## Root cause

`TransactionReaper` keeps two structures: a `_timeouts`
`ConcurrentHashMap<control, ReaperElement>` (population count) and a
`ReaperElementManager _reaperElements` (the sorted deadline queue the reaper thread
actually pops). `ReaperElementManager.add()` stages the element into an internal
`pendingInsertions` **ConcurrentHashMap**, and `getFirst()`/`size()` first call
`flushPending()` to drain it into the sorted `ArrayList`:

```java
// ReaperElementManager.flushPending()
Set<Map.Entry<…>> es = pendingInsertions.entrySet();
Iterator<Map.Entry<…>> it = es.iterator();
while (it.hasNext()) {
    Map.Entry<…> e = it.next();
    ReaperElement v = e.getValue();
    if (es.remove(e))            // <-- remove THROUGH the entrySet view
        insertSorted(v);          //     only insert once the drain remove succeeds
}
```

This is the standard "iterate `entrySet()`, `remove` through the view, act on
success" idiom. It depends on `ConcurrentHashMap.entrySet()` returning a **live**
`EntrySetView` whose `remove(Map.Entry)` deletes from the backing map and reports
`true`.

On CratonVM `ConcurrentHashMap.entrySet()` returned a **detached
`java.util.HashSet` snapshot** (`entrySet class=java.util.HashSet` vs HotSpot's
`ConcurrentHashMap$EntrySetView`). Two defects fell out of that:

1. **No write-through.** `native_chm_entry_set` built its `HashSet` with a plain
   `alloc_backing_map` (no source-map/kind markers), unlike the sibling
   `native_chm_key_set`, which uses the view-backing machinery
   (`alloc_view_backing`/`make_view_set_of`). So `view_backing_source()` returned
   `None` and `native_hs_remove` skipped the source deletion — `entrySet().remove(e)`
   and `entrySet().iterator().remove()` mutated only the snapshot and left the map
   unchanged (`remove(entry) writes through` → map still size 1).

2. **Wrong boolean.** `native_hs_remove` derived its return value from the *snapshot*
   backing. JDK `EntrySetView.remove(o)` is defined as
   `map.remove(e.getKey(), e.getValue())`, so the result must reflect the **source**.
   The snapshot answer was unreliable (entries are bucketed by identity hash and the
   view resyncs), returning `false` even when the mapping was present.

Because `flushPending` gates on `if (es.remove(e))`, the element was never moved
into `_reaperElements` — the reaper's `check()` always saw an empty queue and never
scheduled the cancel. The transaction stayed `STATUS_ACTIVE`.

## Fix

`native-collections/src/lib.rs`, two parts:

1. **`native_chm_entry_set`** — back the returned `HashSet` with a
   `VIEW_KIND_ENTRYSET` view backing (`alloc_view_backing(ctx, this,
   VIEW_KIND_ENTRYSET, cap)`) instead of a plain `alloc_backing_map`, exactly like
   `native_chm_key_set` (keySet) and the generic `native_map_entry_set` (HashMap).
   This gives the set write-through + liveness; reads resync via
   `collect_entries_any` → `map_collect_entries` → `chm_collect_all_entries`
   (CHM-aware, no `entrySet().iterator()` recursion).

2. **`native_hs_remove`** (entrySet branch) — implement the JDK
   `EntrySetView.remove(o)` == `map.remove(k, v)` contract: report `true` iff the
   **source** currently maps `key` to a value equal to the entry's value, then do
   the native 1-arg `remove(Object)` write-through. The boolean is evaluated with
   `containsKey` + `get` + value-equality (mirroring `native_hs_contains`), **not**
   by dispatching the 2-arg `Map.remove(k, v)`: for a natively-backed
   LinkedHashMap/TreeMap the 2-arg form resolves to the JDK `Map.remove` DEFAULT
   method, whose real bytecode (`removeNode → afterNodeRemoval`) walks linkage the
   native backing never populated and throws.

## Verification

- `apps/hib-suite-runner/TReaperProbe.java` — reaper now fires at ~t=3 s,
  `getStatus()==4` (matches HotSpot ~t=2 s).
- `apps/hib-suite-runner/ChmEntrySetProbe.java` — 10/10 (remove/iterator.remove/
  removeIf/liveness/setValue/contains), matches HotSpot.
- `apps/hib-suite-runner/MapFamilyProbe.java` — 28/28 across
  HashMap/LinkedHashMap/TreeMap/CHM (entrySet remove + write-through + wrong-value
  reject, iterator.remove, keySet.remove, full iteration), matches HotSpot. Confirms
  no regression to the other map families that share `native_hs_remove`.
- Full test: `TransactionTimeoutTest` → `found=7 started=1 ok=1 failed=0`
  (was `failed=1`).

## Repro

```
cd C:/craton/CratonVM/apps/hib-suite-runner
# single class, full stacks:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cvfix> --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner "<listfile>" 0
# isolated probes (compile against @common.args classpath):
<cvfix> … @common.args TReaperProbe        # reaper FIRED / status=4
<cvfix> … @common.args ChmEntrySetProbe    # SUMMARY pass=10 fail=0
<cvfix> … @common.args MapFamilyProbe      # SUMMARY pass=28 fail=0
```
