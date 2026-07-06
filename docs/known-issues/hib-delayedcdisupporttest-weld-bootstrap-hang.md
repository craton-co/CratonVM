# Hibernate `DelayedCdiSupportTest` — hangs during/after Weld CDI bootstrap, never reaches the test body

| | |
|---|---|
| **Status** | 🔴 OPEN — not yet root-caused. Confirmed CratonVM-specific, deterministic in isolation (no shared-box contention involved). |
| **Area** | CDI/Weld bootstrap interaction with Hibernate's `delayed` `BeanContainer` access type |
| **Symptom** | No exception, no crash — the VM process simply stops making progress after Weld SE container init and never reaches `@@RESULT`. |
| **Severity** | low-medium — single class observed hanging so far, but the immediately-preceding diagnostic (a Weld-internal collection-layout probe) suggests the stall may sit in a general CDI/collections code path Hibernate's other CDI tests don't exercise the same way. |
| **Discovered** | 2026-07-06, while re-running the Hibernate `others.txt` non-passed list (50 classes, 4 shards, 1200s per-class timeout) against a binary carrying the OSR allocation-region gate fix (`fix/hib-inpredicate-criteria-values-null-20260705`). |

## Symptom

`org.hibernate.orm.test.cdi.events.delayed.DelayedCdiSupportTest` starts
normally — Weld SE container initializes, Hibernate's connection pool comes
up — and then the process simply stops producing any further output. No
`@@RESULT`, no exception, no crash signature. Confirmed with `timeout 200`
wrapping a **single-class, no-contention** rerun (`-Dcraton.batch=1`, isolated
from the other 49 classes in the sweep), so this is not shared-host resource
contention: two other classes that also showed `HANG` in the parallel sweep
(e.g. `sorted.set.SortNaturalTest`) turned out to **pass cleanly in
isolation** (10.5s, `ok=1`) — a contention artifact, ruled out here.
`DelayedCdiSupportTest` reproducibly fails to reach `@@RESULT` within 200s
in isolation, confirming a genuine VM-side stall, not noise.

Log excerpt right before the stall (nothing further is printed for the
remaining ~190s until the wrapper kills the process):

```
INFO [org.jboss.weld.Bootstrap] WELD-000101: Transactional services not available. ...
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (caller used slot
  index past receiver's layout — class layout is correct; the bug is in the caller's slot
  computation, typically a speculative collection-layout probe dispatched on a non-matching
  receiver type) obj=0x27d9aac8 index=0 num_slots=0 class_id=ClassId(2971)
  class_name=org/jboss/weld/util/collections/ImmutableList$ImmutableListCollector real_field_count=Some(0)
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped (...) class_name=org/jboss/weld/util/collections/ImmutableSet$ImmutableSetCollector real_field_count=Some(0)
  (repeats ~10x, alternating between the two collector classes)
INFO [org.jboss.weld.Bootstrap] WELD-ENV-002003: Weld SE container {0} initialized
WARN [org.hibernate.testing.cache.CachingRegionFactory] ...
INFO [org.hibernate.orm.connections.pooling] HHH10001005: Database info:
	Database JDBC URL [jdbc:h2:mem:db1;...]
	...
	Maximum pool size: 5
<nothing further — process killed by external timeout after 200s>
```

## What's probably NOT the cause

The repeated `gen_heap::get_field: out-of-bounds field read dropped` warnings
for `ImmutableList$ImmutableListCollector`/`ImmutableSet$ImmutableSetCollector`
(both `num_slots=0`, `real_field_count=Some(0)`) are almost certainly a
**red herring, not the hang itself** — per the guard's own doc comment in
`gc/src/gen_heap.rs` (~line 1656), this is the *known-benign* "case (B)"
degrade: a caller-side speculative collection-layout probe (e.g.
`collect_collection_elements`) landing on a 0-slot object and safely
returning `Value::Object(None)` instead of dereferencing garbage. This
pattern already fires (harmlessly, per the code comment) across many other
classpaths using Weld's immutable-collection helpers. It's included here
only as a timing landmark — the stall happens shortly after these warnings,
during/after `WELD-ENV-002003` and Hibernate's connection-pool info dump,
before the test body itself ever begins printing SQL.

## Hypothesis (not yet verified)

`DelayedCdiSupportTest` specifically exercises Hibernate's **delayed**
`BeanContainer` access strategy (lazy CDI bean resolution — Hibernate holds
a proxy/delayed reference to the CDI bean instead of eagerly resolving it at
boot, unlike the `standard`/`extended` variants, which pass cleanly in this
same sweep). The likely candidates:

- A lazy/delayed CDI bean lookup that never completes — e.g. blocks
  indefinitely on a lock, a `CountDownLatch`, or a condition-wait that's
  never signaled, rather than a busy spin (CPU wasn't obviously pegged, but
  this wasn't explicitly measured this session — worth checking with
  `Get-Process` CPU-time deltas on a longer rerun).
- Something specific to the `delayed` CDI event/converter proxy creation
  path interacting with the Weld collector classes seen right before the
  stall (immutable-list/set builders are commonly used when Weld builds its
  bean-resolution/observer-method indices — if the delayed path re-triggers
  that indexing lazily at first use and something loops or blocks there,
  that would explain the timing).

## What's ruled out

- **Not shared-host contention**: verified via a clean, isolated single-class
  rerun (no other shards, no other classes running) — still hangs
  deterministically at ~200s+.
- **Not the same symptom as the last recorded run of this class**: an older
  inventory (`docs/internal/hibernate-bugs/run-20260622/INVENTORY-cvonly-real-fails.tsv`,
  2026-06-22) recorded this class failing with
  `java.lang.RuntimeException: Could not configure StandardServiceRegistryBuilder`
  — a completely different symptom (a clean exception, not a hang). Whatever
  changed between then and now on `dev` altered this class's failure mode
  from a fast exception to an indefinite stall; not yet determined whether
  that's a regression from a specific commit or a timing-dependent path that
  was always latent.
- **Likely unrelated to the OSR allocation-region-gate fix** carried by the
  binary this was discovered with (`fix/hib-inpredicate-criteria-values-null-20260705`):
  that fix only changes back-edge OSR eligibility for allocating/calling
  bytecode regions, and this class's stall happens during CDI/Weld
  bootstrap, not inside an obviously hot allocating loop. Not independently
  confirmed against unmodified `dev` this session — worth a quick check.

## Repro

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 200 <cv-binary> \
  --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 -Dcraton.trace=1 \
  CratonRunner <listfile-with-only-DelayedCdiSupportTest> 0
# -> @@BEGIN prints, then nothing further; process killed by the wrapper at 200s.
```

## Next steps (not yet done)

- Confirm whether this reproduces on unmodified `dev` (without the OSR gate
  fix) to rule out any interaction, however unlikely given the mechanism.
- Attach with a thread/stack dump at the stall point (`--stack-dump-on-timeout`
  or an external debugger) to see exactly which frame/lock the thread is
  parked in — this is the single most useful next diagnostic, since nothing
  in the log narrows it further.
- Compare against `standard`/`extended` CDI variants (which pass) to see what
  code path is unique to `delayed` bean resolution.
