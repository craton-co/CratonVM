# HIB-CV-06 — EMF/SessionFactory bootstrap DB connect fails: "Wrong user name or password"

**Severity:** High — blocks every `@Jpa`/`@SessionFactory` test that actually opens a connection;
the bootstrap failure is also the exception behind HIB-CV-04's "multiple times" cascade.
**Status:** OPEN
**HotSpot:** not affected (same class + same config connects and runs).

## Symptom

During Hibernate bootstrap (e.g. `DialectFilterExtension` resolving the dialect, or
`JdbcEnvironmentInitiator` reading metadata):

```
org.junit.jupiter.engine.execution.ConditionEvaluationException:
  Failed to evaluate condition [org.hibernate.testing.orm.junit.DialectFilterExtension]:
  Could not connect to database with JDBC URL
  'jdbc:h2:mem:db1;DB_CLOSE_DELAY=-1;LOCK_TIMEOUT=10000;DB_CLOSE_ON_EXIT=FALSE'
  [Wrong user name or password [28000-240]]
```

Config (from `hibernate.properties`): `url=jdbc:h2:mem:db1;…`, `username=sa`, **`password=` (empty)**.

Confirmed CratonVM-specific: `LocalTemporaryTableMutationStrategyNoDropTest` passes on HotSpot
(`ok=1`) but fails this way on CratonVM. With `CRATONVM_DISABLE_JIT=1` the surface error is this
clean DB failure; with JIT on it often manifests as a silent `rc=1` crash (the JIT miscompiles the
same throwing bootstrap path).

## What's ruled out

Raw H2 connections are **fine** on CratonVM (`H2ConnProbe`): `DriverManager.getConnection(url,"sa","")`,
the `Properties` form (`user=sa, password=""`), reconnects, and `Driver.connect` all succeed and
match HotSpot. So the bare JDBC/H2 path is not the bug.

The `GradleParallelTestingConnectionCreatorFactoryImpl`/`...Resolver` only rewrites `$worker`
patterns (absent in this config), so it passes `sa`/empty through unchanged.

## Likely cause

The failure is in **Hibernate's bootstrap connection path** specifically (ServiceRegistry →
`GradleParallelTestingConnectionCreatorFactoryImpl` → `DriverManagerConnectionCreator`), where the
`Properties` are assembled from Hibernate settings. "Wrong user name or password" against a fresh
in-mem DB means the DB was first created (by an earlier bootstrap connection — the `H2DatabaseCleaner`
runs first) with **different effective credentials** than the failing connection uses. I.e. CratonVM
produces **inconsistent empty-password handling** across the two bootstrap connections (one creates
`db1` with creds A, the next presents creds B). Candidate: the empty `hibernate.connection.password`
value being read as `null` vs `""` inconsistently through Hibernate's settings → `Properties` →
H2 (cf. the prior native-`Properties` bug that dropped persistence.xml values).

## ✅ RESOLVED — `Properties.setProperty` polluted the global system-property store

**Two stacked native `Properties` bugs; the second was the dominant one.**

**Bug B (dominant):** `native_properties_set_property` / `native_properties_put` mirrored **every**
`Properties.setProperty`/`put` to the **global** system-property store (`ctx.set_system_property`),
and `getProperty` falls back to that store on a side-table miss. So distinct `Properties` objects
cross-contaminated: `new Properties().setProperty("k","v")` made `System.getProperty("k") == "v"`.
`ConfigurationHelper.maskOut` clones the global props and sets the **clone's** password to `"****"`
for logging — the `"****"` leaked into system properties and back into the real password, so every
EMF/SessionFactory bootstrap connected with password `"****"` → H2 "Wrong user name or password",
which `DialectFilterExtension` turned into a **skip** and which (on other paths) cascaded into the
JUnit `ValidatingInvocation` "Chain of InvocationInterceptors called invocation multiple times"
abort (HIB-CV-04). **Fix:** mark only the synthetic `Properties` returned by
`System.getProperties()`; regular `Properties` no longer write to the system store.

**Bug A (latent):** the side-table was keyed by `obj.as_ptr()` (not GC-stable). Fixed to key by
`ctx.identity_hash_code` + a generation registry.

**Verified:** `IdProbe` distinct Properties independent; `EnvProbe` `password=[]` (was `****`);
**`DeleteDecomposerTest` 12/12 PASS** (was 12 FAIL "multiple times"); `MiniJpaT` ok=1. This single
fix unblocks the dominant CratonVM-only failure buckets (the ~503 SKIPPED + ~75 "multiple times").
HIB-CV-04 is resolved by the same change.

---

### (original investigation notes below)

## ROOT CAUSE FOUND — native `Properties` side-table keyed by raw pointer (superseded — this was Bug A)

`Environment.getProperties().getProperty("hibernate.connection.password")` returns **`****`** on
CratonVM but **`""`** on HotSpot. `Environment.<clinit>` does
`GLOBAL_PROPERTIES.load(stream)` then `CORE_LOGGER.propertiesLoaded(maskOut(GLOBAL_PROPERTIES, PASS…))`.
`ConfigurationHelper.maskOut` **clones** the props and masks the **clone**'s password to `"****"` for
logging — the original must stay `""`. On CratonVM it doesn't: the masked `****` leaks back into the
real `GLOBAL_PROPERTIES`, so every bootstrap connects with password `****` → H2 `Wrong user name or
password`.

Bisected to the native `Properties` side-table (`native-builtins/src/properties_sidetable.rs`):

```java
Properties p1 = new Properties(), p2 = new Properties();
p1.setProperty("AAA","fromP1");
System.getProperty("AAA");   // CratonVM: "fromP1"   HotSpot: null
p2.getProperty("AAA");       // CratonVM: "fromP1"   HotSpot: null
```

Three **distinct** `Properties` objects (distinct identity hashes) **share one side-table entry** —
`new Properties()` even aliases `System.getProperties()`. Cause: the side-table was keyed by
`key_for(obj) = obj.as_ptr()` — the **raw heap address**, which is **not a stable object identity
under the moving GC**. After a relocation a fresh `Properties` reuses an address a different
`Properties` held, so their side-table entries collide. `maskOut`'s clone therefore writes `****`
into the address now shared with `GLOBAL_PROPERTIES` (and `System` props).

### Fix

Key the side-table by a **GC-stable identity** — `ctx.identity_hash_code(obj)` plus a per-hash
generation registry to disambiguate genuine 32-bit hash collisions (mirroring `native-collections`'
`widened_obj_key`, which already solved the identical bug for the `LinkedHashMap` overlay). Threaded
`ctx` through `key_for` and the `put_kv`/`get_kv`/`remove_kv`/`snapshot_kv`/`count_kv` helpers and
their callers. (worktree `fix/hibernate-suite-loop`, `properties_sidetable.rs`.)

This is the highest-impact fix in the suite: it should convert the ~503 SKIPPED ORM classes (and a
large share of the HIB-CV-04 "multiple times" cascades, which were the bootstrap throwing on the
corrupted password) from skipped/failed to runnable. **Also a correctness fix for ALL `Properties`
usage** — any two `Properties` mutated near a GC could previously cross-contaminate / pollute
`System.getProperties()`.

## Narrowed (earlier, superseded by root cause above)

`H2Conn2` probe (both VMs identical): creating `db1` with `sa`/empty then reconnecting **with no
`user`/`password` properties at all** → `Wrong user name or password [28000-240]` on **both** VMs.
So the H2 error itself is normal — it means the failing CratonVM bootstrap connection reaches
`driver.connect(url, props)` with the **`user`/`password` properties missing or wrong**, where
HotSpot passes `sa`/empty. CratonVM is **dropping the connection credentials** somewhere on
Hibernate's connection-provider settings path (settings `Map` → `ConnectionProvider` →
`connectionProps`). `Properties.load` of the empty-password line is fine (`PropsProbe`), and
`DialectContext.init`'s explicit `props.setProperty("user"/"password", …)` form works in isolation —
so the drop is on the EMF/SessionFactory `ConnectionProvider` build, not the raw read. This is the
same family as the already-fixed **HIB-1** bug (native `Properties` mirrored writes only to a
side-table, so `configValues.putAll(puProperties)` copied 0 entries) — a residual on the
connection-credential propagation path.

**Reclassified suite impact:** this defect makes `DialectFilterExtension` (which uses
`DialectContext`) unable to connect, so it **disables** the test — counted as *skipped*, not failed.
Correct reclassification of the partial run shows CratonVM **real PASS=22** vs HotSpot **858**, with
**~503 classes SKIPPED on CratonVM that PASS on HotSpot** — almost all from this single defect. It is
the #1 highest-leverage fix for the suite by a wide margin.

## Next step

Instrument `DriverManagerConnectionCreator.makeConnection` (or wrap H2's `Driver.connect`) to log the
exact `user`/`password` of **both** the first (creating) and the failing connection in one JVM. The
diff between them is the bug. Then trace the empty-password value back through
`ConnectionProviderInitiator` → settings map → native `Properties`.

## Impact

Combined with HIB-CV-04, the EMF bootstrap path (this connect + the `ServiceLoader$Itr` failure
HIB-CV-05) is the common denominator behind the large majority of Hibernate ORM test failures on
CratonVM. These three bootstrap defects are the highest-leverage fixes for this suite.
