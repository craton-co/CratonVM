# TestAsyncContextImpl — `LogManager.getLogger()` returns null (NPE)

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.core.TestAsyncContextImpl` fails `testAsyncIoEnd00` (and
sibling `testAsyncIoEnd01`, etc.) with:
```
1) testAsyncIoEnd00(org.apache.catalina.core.TestAsyncContextImpl)
java.lang.NullPointerException: Cannot invoke "java.util.logging.Logger.setLevel(java.util.logging.Level)"
  because the return value of "java.util.logging.LogManager.getLogger(String)" is null
```
`java.util.logging.LogManager.getLogger(String)` returns `null` for a logger
name that Tomcat's test setup expects to already exist (likely a
`java.util.logging.Logger.setLevel` call in test setup/teardown targeting a
named logger that should have been registered by JULI's
`ClassLoaderLogManager` or by a prior `Logger.getLogger(name)` call in the
same test). On HotSpot, `getLogger` finds the already-registered logger; on
CratonVM it comes back null, meaning either the logger was never registered
in CratonVM's `LogManager` backing store, or a per-classloader/JULI
registration path CratonVM handles differently loses the entry.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName asynclog `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestAsyncContextImpl
```

## Recommendation

Find the exact `Logger.setLevel`/`LogManager.getLogger` call site in
`TestAsyncContextImpl`'s test setup (likely a `@Before`/static initializer
silencing a noisy logger for async I/O tests) and trace which logger name is
being looked up. Check whether CratonVM's JULI/`LogManager` implementation
registers loggers eagerly (on first `Logger.getLogger()` call) vs lazily, and
whether a per-webapp-classloader `LogManager` (Tomcat uses
`org.apache.juli.ClassLoaderLogManager`, one instance per webapp classloader)
is being consulted correctly — a classloader mismatch between where the
logger was registered and where `getLogger` is later called would produce
exactly this null.
