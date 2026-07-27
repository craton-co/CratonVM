# `spring-boot-jdbc` Oracle UCP pool-init hang — FIXED

**Fixed: 2026-07-18**

## Confirmed cause

This was not a database-network wait, UCP maintenance-thread problem, or
dynamic-proxy retry. In the default real-AQS configuration, CratonVM still
registered the legacy synthetic natives for `java.util.concurrent.Semaphore`.
Those constructor natives stored their private permit/fair-state `int[]` in
the real JDK `Semaphore.sync` reference field.

That representation is only valid when every `Semaphore` operation is
intercepted by the synthetic surface. Oracle UCP's
`CriStats$BorrowSemaphore` instead invokes the inherited protected AQS path:
`Semaphore$Sync.reducePermits()` reads the `state` field from what should be a
`Semaphore$Sync`. It received the synthetic `int[]`, leading to an
out-of-layout volatile field read and an endless compare-and-set retry loop.

The same defect is reproducible without Spring or UCP:

1. Construct `new Semaphore(1)`.
2. Inspect private `sync`: before the fix it is `[I`; after the fix it is
   `Semaphore$NonfairSync`.
3. Call `reducePermits(1)` from a subclass: before the fix it spins; after the
   fix it completes.

## Fix

Synthetic Semaphore registrations, including the early bootstrap
registrations, are now enabled only when `CRATONVM_SYNTHETIC_AQS=1` is
explicitly selected. The default real-AQS mode therefore executes the real
JDK constructor and preserves `sync: Semaphore$Sync`, while synthetic-AQS
mode retains its existing compatible native implementation.

The accompanying interpreter guards stop two independent, non-semantic
out-of-layout probes encountered during the original reproduction:

- real-super `$ProxyN` classes use their declared interfaces rather than a
  nonexistent synthetic interface slot;
- the MethodHandle downcall adapter accepts only an actual
  `java/lang/invoke/MethodHandle` receiver, not arbitrary methods named
  `invoke*` in JUnit's interception chain.

## Validation

Dedicated release binary:
`cratonvm-oracle-ucp-pool-hang-20260718-019f742b-semaphore-real.exe`

Minimal semaphore regression probe:

| Mode | Result |
|---|---|
| JIT | `Semaphore.sync == Semaphore$NonfairSync`; 100 `reducePermits` rounds pass |
| `--nojit` | 100 `reducePermits` rounds pass |

Spring Boot fixture: `apps/spring-boot`, JDK 25.0.3, runner timeout 180 s,
one class per process.

| Mode | Class | Result |
|---|---|---|
| JIT | `OracleUcpDataSourceConfigurationTests` | PASS, 7 tests, 45.577 s |
| JIT | `OracleUcpDataSourcePoolMetadataTests` | PASS, 6 tests, 9.131 s |
| `--nojit` | `OracleUcpDataSourceConfigurationTests` | PASS, 7 tests, 29.231 s |
| `--nojit` | `OracleUcpDataSourcePoolMetadataTests` | PASS, 6 tests, 6.142 s |

All four final stderr logs are free of the previous out-of-bounds field-read
warnings and `Semaphore$Sync.getState` spin signature.

## Note (2026-07-23): a different, unrelated `OracleUcpDataSourcePoolMetadataTests` failure found

`OracleUcpDataSourcePoolMetadataTests` fails again in the
`RunName=craton-rerun-20260723` rerun (5/6 pass), but **not** a recurrence of
this hang — no spin, no `Semaphore$Sync` involvement, finishes in ~10s. New,
distinct symptom (`getPoolSizeOneConnection`, `UCP-45069: Universal
Connection Pool is empty` on the first on-demand connection borrow from a
fresh empty pool) — see
[`../../../known-issues/springboot/oracleucp-poolsizeoneconnection-connection-pool-empty-20260723.md`](../../../known-issues/springboot/oracleucp-poolsizeoneconnection-connection-pool-empty-20260723.md).
Filed separately rather than reopening this doc since the mechanism this doc
fixed (synthetic-`Semaphore`/real-AQS layout) is confirmed unrelated to the
new failure.
