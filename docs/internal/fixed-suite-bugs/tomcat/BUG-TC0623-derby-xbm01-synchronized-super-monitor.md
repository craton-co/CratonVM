# BUG-TC0623 — Apache Derby embedded boot fails (XBM01.D): synchronized super-call skips method monitor on the cached dispatch path

**Suite:** Apache Tomcat (rerun on `dev`, 2026-06-23)
**Tests:** `org.apache.catalina.realm.TestDataSourceRealm`,
`org.apache.catalina.servlets.TestWebdavPropertyStore`
**Status:** FIXED — both tests `OK` on the fixed VM (see "Validation").

---

## Symptom

Both tests use an in-process **embedded Apache Derby** database. Connecting
(`DriverManager.getConnection("jdbc:derby:...;create=true")`) failed:

```
java.sql.SQLException: Startup failed due to an exception.
Caused by: org.apache.derby.shared.common.error.StandardException: XBM01.D
  at ...BaseMonitor.bootService(BaseMonitor.java:1845)
  ...
Caused by: java.lang.IllegalMonitorStateException:
        thread Thread-0 called notifyAll() without owning the monitor
  at org.apache.derby.impl.store.raw.data.BasePage.releaseExclusive(BasePage.java:1844)
  at org.apache.derby.impl.store.raw.data.CachedPage.releaseExclusive(CachedPage.java:531)
  at org.apache.derby.impl.store.raw.data.StoredPage.releaseExclusive(StoredPage.java:1073)
  at org.apache.derby.impl.store.raw.data.BasePage.update(BasePage.java:1648)
  ...
```

Derby SQLState `XBM01` ("Startup failed due to an exception") is only a wrapper.
The real cause is the nested `IllegalMonitorStateException` thrown when Derby
releases a page latch.

---

## Root cause (primary) — `synchronized` super-call ran without its monitor

`org.apache.derby.impl.store.raw.data.BasePage.releaseExclusive` is declared
`protected synchronized void` and ends with `aload_0; invokevirtual notifyAll`,
i.e. it calls `notifyAll()` **on `this`**, relying on the implicit
method-level monitor (`this`) acquired on entry.

Crucially, only the base method is `synchronized`; the overrides are not:

| class       | `releaseExclusive`             |
|-------------|--------------------------------|
| `StoredPage`| `protected void` (not sync)    |
| `CachedPage`| `protected void` (not sync)    |
| `BasePage`  | `protected **synchronized** void` |

So the chain is `StoredPage.releaseExclusive → super → CachedPage.releaseExclusive
→ super → BasePage.releaseExclusive`, where each `super` is an **`invokespecial`**.
The only monitor acquisition for the whole chain is `BasePage.releaseExclusive`'s
implicit method monitor.

CratonVM's interpreter dispatches `invokevirtual`/`invokespecial` through a
monomorphic inline cache (`execute_invokevirtual_cached`). It has five
frame-pushing arms; four acquire the synchronized-method monitor, **one did
not**:

| arm (`CachedInvokeTarget`)          | path                              | acquires monitor? |
|-------------------------------------|-----------------------------------|-------------------|
| `VirtualBytecode`                   | `invokevirtual` (interpreter)     | ✅ yes |
| `Bytecode` (in `execute_invokestatic_cached`) | `invokestatic`          | ✅ yes |
| vtable-fast                         | `invokevirtual` fast             | ✅ yes |
| slow path `try_stackless_invoke`    | first call (cache miss)          | ✅ yes |
| **`Bytecode` (in `execute_invokevirtual_cached`)** | **`invokespecial` super-calls** | ❌ **NO (bug)** |

The buggy arm built the callee frame and pushed it **without** calling
`shared.monitors.enter(...)` and **without** setting `frame.monitor_on_exit`,
ignoring `cached.is_synchronized` entirely.

### Why it was latent and why it was deterministic here

- The **first** call to `BasePage.releaseExclusive` misses the inline cache and
  runs the slow path (`try_stackless_invoke`), which *does* acquire the monitor —
  so it works once.
- The slow path then populates the call site's cache with a
  `CachedInvokeTarget::Bytecode`. **Every subsequent** call hits the buggy arm
  and runs the `synchronized` body **without holding `this`**.
- For most `synchronized` methods this is silently wrong (lost mutual
  exclusion) but raises no error — the missing implicit `monitorexit` is also
  skipped (`monitor_on_exit` is `None`), so the frame pops cleanly.
- The moment the body calls `wait`/`notify`/`notifyAll` on `this` (Derby's page
  latch does exactly this), the VM correctly observes "no ownership" and throws
  `IllegalMonitorStateException`. Derby creates many pages during boot, so the
  2nd+ `releaseExclusive` reliably trips it.

### Fix

In `execute_invokevirtual_cached`'s `CachedInvokeTarget::Bytecode` arm, acquire
the method monitor for `synchronized` methods and record `monitor_on_exit`,
mirroring the `VirtualBytecode` / `invokestatic` arms exactly
(`vm/src/runtime/interpreter.rs`).

---

## Root cause (secondary) — masked missing native `OperatingSystemImpl.initialize0`

With the monitor bug fixed, Derby boots further and then fails differently:

```
java.lang.UnsatisfiedLinkError: com/sun/management/internal/OperatingSystemImpl.initialize0()V
  at com.sun.management.internal.OperatingSystemImpl.<clinit>
  at ...PlatformMBeanProviderImpl.getOperatingSystemMXBean
  at java.lang.management.ManagementFactory.getPlatformMBeanServer
  at org.apache.derby.impl.services.jmx.JMXManagementService.boot(JMXManagementService.java:114)
```

This is a distinct, previously-masked CratonVM gap. CratonVM registered the OS
MXBean natives (`initialize0`, `get*0`) on the pre-JPMS class name
`sun/management/OperatingSystemImpl`. JDK 9+ moved the platform implementation
to `com.sun.management.internal.OperatingSystemImpl`, and JDK 25 renamed
several natives (`getFreePhysicalMemorySize0 → getFreeMemorySize0`,
`getTotalPhysicalMemorySize0 → getTotalMemorySize0`,
`getSystemCpuLoad0 → getCpuLoad0`). The class's `<clinit>` calls the static
`initialize0()`; with no native registered, class init fails and aborts every
`ManagementFactory.getPlatformMBeanServer()` caller — including Derby's JMX
service boot — leaving the embedded driver unregistered
(`AutoloadedDriver.getDriverModule()` → null → NPE).

### Fix

`register_operating_system_impl` now also registers the modern
`com/sun/management/internal/OperatingSystemImpl` natives (JDK 25 names),
returning the same spec-defined "unavailable" sentinels (`-1` / `-1.0`) as the
legacy class (`native-builtins/src/jmx.rs`). The legacy registration is kept.

---

## Validation

Isolated reproducer of the primary bug (`SyncSuperNotify.java`): a
`synchronized` base method called via two `super`/`invokespecial` hops that
calls `notifyAll()` on `this`, looped 50× to populate + hit the cached arm.

- Baseline `dev`: `IllegalMonitorStateException: thread Thread-0 called
  notifyAll() without owning the monitor` — identical to the Derby cause.
- Fixed: `OK no IMSE after 50 synchronized super-call notifyAll`.

Tomcat suite (`apps/tomcat/.tooling/run-suite.ps1 -Vm craton`, real-net env,
`-Xmx2g`, server `--add-opens`):

| test | before | after |
|------|--------|-------|
| `TestDataSourceRealm`     | NOSUMMARY (XBM01) | **OK (1 test)** |
| `TestWebdavPropertyStore` | NOSUMMARY (XBM01) | **OK (2 tests)** |

---

## Files

- `vm/src/runtime/interpreter.rs` — monitor acquisition in the cached
  `Bytecode` (invokespecial super-call) arm.
- `native-builtins/src/jmx.rs` — `com.sun.management.internal.OperatingSystemImpl`
  natives for JDK 25.
