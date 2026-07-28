# `action.queue` GRAPH-default proof tests — expected fallout of the LEGACY flush-queue compatibility default (not a bug)

**Status: WON'T-FIX / expected.** Root-caused and confirmed by direct A/B repro.
Both failures are a **fully understood, intentional consequence** of the
real-JDK `hibernate.flush.queue.type` default CratonVM already ships (commit
`0e87935f2`, "fix(hibernate): default real-jdk flush queue to legacy" — see
[`joinedsubclassbatch-cyclebreaker-flush-hang-20260721-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/joinedsubclassbatch-cyclebreaker-flush-hang-20260721-FIXED.md)).
No config-detection bug, no ServiceLoader bug, no new fix needed here — this
doc exists only to close the loop so the two classes aren't re-investigated
as a fresh regression.

## Source run

`apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(shard-2 idx 0, shard-3 idx 4), binary from worktree `CratonVM-hib-local-0712`
merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on.

| Class | Failure |
|---|---|
| `org.hibernate.orm.test.action.queue.ActionQueueDefaultTest` | `AssertionFailedError: expected: <GRAPH> but was: <LEGACY>` |
| `org.hibernate.orm.test.action.queue.proof.InsertOrderingReferenceSeveralDifferentSubclassTest` | `AssertionFailedError:` (blank message — an AssertJ `assertThat(...).isEqualTo(...)` on SQL batch shape/order) |

## Root cause

`ActionQueueDefaultTest.graphQueueIsTheDefault` (`apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/action/queue/ActionQueueDefaultTest.java:27`)
directly asserts:

```java
assertEquals( QueueType.GRAPH, sessionFactory.getActionQueueFactory().getConfiguredQueueType() );
```

This is upstream Hibernate ORM 8.0.0-SNAPSHOT's own contract test for its new
GRAPH-based `ActionQueue`/`CycleBreaker` flush planner being the **default**
when no `hibernate.flush.queue.type` property is set.

CratonVM, however, intentionally overrides that default for real-JDK mode.
`vm/src/vm/vm_init.rs` (~line 2556):

```rust
// Hibernate ORM 8 defaults to its graph-based ActionQueue. Its
// cycle-breaking planner can spend several minutes in dispatch-heavy
// DFS work on CratonVM's real-JDK runtime. The mature legacy queue
// completes the same workloads predictably. An explicit user property
// is applied immediately below and therefore still selects `graph`.
if !config.use_synthetic_jdk {
    sys_props
        .entry("hibernate.flush.queue.type".to_string())
        .or_insert_with(|| "legacy".to_string());
}
```

This was added by commit `0e87935f2` specifically to fix the
`joinedsubclassbatch` `CycleBreaker` DFS hang (>1000x slowdown, documented in
the FIXED doc linked above). It is a deliberate real-JDK-only compatibility
default, applied via `sys_props.entry(...).or_insert_with(...)` — i.e. it
only fires when the test/app doesn't already set the property, and any
explicit `-Dhibernate.flush.queue.type=...` (including `graph`) still wins.

Both failing tests here rely on **no explicit property being set**, so they
observe CratonVM's compatibility default (`legacy`) instead of upstream's
shipped default (`graph`):

- `ActionQueueDefaultTest` fails for the obvious reason — it's checking the
  literal default value.
- `InsertOrderingReferenceSeveralDifferentSubclassTest` (`@JiraKey("HHH-14344")`,
  package `action.queue.proof`) asserts an exact SQL statement/batch-position
  sequence that only the GRAPH planner produces (it groups/orders inserts by
  entity type across interleaved `persist()` calls for optimal JDBC batching).
  Under LEGACY, inserts execute in raw `persist()` call order with no
  cross-type regrouping — confirmed in the raw log: statements alternate
  `UnrelatedEntity` → `SubclassZero` → `SubclassTwo` → `SubclassOne` →
  `UnrelatedEntity` → ... (batch of 1 recreated per statement) instead of the
  test's expected grouped-batch shape, and a separate `UPDATE ... parent_fk`
  is emitted afterward to patch up the FK LEGACY couldn't resolve up front.
  This is exactly what "legacy" ordering is expected to look like; it is not
  a new bug.

## Confirmation (A/B repro, both directions)

```bash
cd apps/hib-suite-runner
printf "org.hibernate.orm.test.action.queue.ActionQueueDefaultTest\n" > /tmp/aq1.txt
printf "org.hibernate.orm.test.action.queue.proof.InsertOrderingReferenceSeveralDifferentSubclassTest\n" > /tmp/aq2.txt

# Default (no override) — reproduces both failures:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner /tmp/aq1.txt 0
# -> HHH90032023: Using LEGACY ActionQueue implementation
# -> @@FAIL ... expected: <GRAPH> but was: <LEGACY>

# Explicit graph override — both classes PASS clean:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dhibernate.flush.queue.type=graph -Dcraton.batch=1 \
  CratonRunner /tmp/aq1.txt 0
# -> HHH90032023: Using GRAPH ActionQueue implementation
# -> @@RESULT ... ok=1 failed=0

"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --Xmx 1500m @common.args -Dhibernate.flush.queue.type=graph -Dcraton.batch=1 \
  CratonRunner /tmp/aq2.txt 0
# -> @@RESULT ... ok=1 failed=0
```

Result: with the property forced back to `graph`, both classes pass cleanly
(`ok=1 failed=0` each). This proves the failures are 100% explained by
CratonVM's intentional LEGACY default and rules out any config-detection,
ServiceLoader, or classpath-scanning bug — the selection mechanism itself
works correctly and honors explicit overrides exactly as designed.

## Second cost of the LEGACY default, measured 2026-07-28: −28 % throughput on an insert-heavy fixture

The trade-off is not only "two contract tests fail". On a fixture that
`persist()`s many rows in one transaction, the LEGACY queue is also
substantially *slower* than the GRAPH default it replaces — the opposite of the
direction one would assume from the `CycleBreaker`-hang rationale above.

Measured on dev `d0a6c7987`, quiet host, one fresh process per sample,
`OracleInlineMutationStrategyIdTest#testDeleteFromPerson` (its `@BeforeEach`
persists 2200 JOINED-inheritance entities = 4400 INSERTs). Three samples per
configuration, **interleaved** round-robin rather than in blocks, so host drift
cannot favour one side:

| configuration | samples (ms) | mean |
|---|---|---:|
| CratonVM default (LEGACY) | 255 999 / 241 995 / 216 814 | 238 269 |
| `-Dhibernate.flush.queue.type=graph` | 168 208 / 185 606 / 159 862 | **171 225 (−28 %)** |

The two sample sets do not overlap, so this is well clear of the ±8 % run-to-run
spread the default alone shows.

So the LEGACY default is currently paying twice: two expected-fail classes *and*
better than a quarter of the wall clock on insert-heavy Hibernate fixtures. That
does not change the disposition below — the `CycleBreaker` DFS hang it avoids is
a >1000x pathology and this is 1.4x — but it does raise the value of revisiting
the default once that hang is addressed, and it should be quoted alongside the
hang when that trade is re-litigated. Full measurement context:
[`h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md),
residual section.

## Disposition

**Leave as-is.** These two classes are, by construction, tests of upstream's
GRAPH-is-the-default contract, which CratonVM deliberately does not honor by
default on real-JDK (to avoid the far more impactful `CycleBreaker` DFS hang
documented separately). This is the same kind of accepted trade-off as other
permanent divergences in this repo (e.g. `rustls` no-DHE support). No action
needed; not counted as a genuine CratonVM correctness bug. If upstream's
GRAPH planner's `CycleBreaker` performance is ever fixed for CratonVM's
real-JDK dispatch overhead, the default could be revisited — until then these
two classes are expected-fail collateral of that fix.
