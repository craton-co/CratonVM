# `hql.ASTParserLoadingTest` JAXB reflection throughput wall — FIXED

## Resolution

`safe_native_call` used to rebuild and publish the complete interpreter-root
snapshot at every ordinary object-returning native boundary.  JAXB model
construction reaches those boundaries at deep, rapidly changing reflection
stacks; the old publication therefore repeatedly scanned the whole stack even
though a running mutator cannot be collected there.

Object returns and native-thrown exceptions now remain in
`native_pending_return` until the next collector-visible boundary.  A
safepoint refreshes the snapshot before a stop-the-world collector reads it,
and blocking natives continue to publish through `deposit_root_snapshot`
before they park.  The redundant post-return refresh is removed as well.

The Hibernate suite runners now default to `-Xmx2g`.  The prior `1500m`
default added sufficient young-generation pressure to push this otherwise
finite reflection-heavy class past the harness's 300-second limit.

## Regression coverage

`vm/src/vm/vm_exec.rs` verifies both handoff shapes:

- a native-thrown Java exception remains rooted and is published at the next
  safepoint;
- an object returned from a native remains rooted and is likewise published at
  the next safepoint.

## Validation (2026-07-21/22)

All runs used the real Hibernate fixture, Eclipse Adoptium JDK 25, the exact
`CratonRunner` class-list entry, and the task-specific release binary:

| Runtime | Heap | Result |
|---|---:|---|
| CratonVM JIT | 2 GiB | 106/106 passed in 276606 ms |
| CratonVM `--nojit` | 2 GiB | 106/106 passed in 242315 ms |
| HotSpot | 1500 MiB | 106/106 passed in 16377 ms |

Both CratonVM modes complete within the suite runner's 300-second per-class
cap.  This closes the reported timeout without conflating it with unrelated
fixture failures or treating active computation as a deadlock.
