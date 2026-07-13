# TestDefaultInstanceManager — class-unloading count off by one

**Status:** OPEN. **Severity:** low-medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.core.TestDefaultInstanceManager.testClassUnloading`
fails:
```
1) testClassUnloading(org.apache.catalina.core.TestDefaultInstanceManager)
java.lang.AssertionError: expected:<8> but was:<9>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.junit.Assert.failNotEquals(Assert.java:835)
	at org.junit.Assert.assertEquals(Assert.java:647)
```
This test starts/stops a webapp classloader repeatedly and expects some
countable quantity (likely the number of GC'd/unloaded classes, or the
number of `WeakReference`s cleared, tracked via a phantom/weak-reference
poll loop typical of Tomcat's class-unloading tests) to reach exactly 8;
CratonVM produces 9 — one extra. This is a quantitative off-by-one, not a
crash or hang, and could stem from either a genuine double-count in
CratonVM's classloader/GC bookkeeping, or a class that HotSpot's GC
collects differently (later/earlier) than CratonVM's generational
collector under this specific load/unload sequence.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName defaultinstmgr `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.core.TestDefaultInstanceManager
```

## Recommendation

Read `TestDefaultInstanceManager.testClassUnloading`'s source to identify
exactly what's counted to 8 vs 9 (likely a loop over
`WeakReference.get() == null` checks after forcing GC N times) — trace
whether CratonVM's `System.gc()`/full-GC semantics under this test's
specific trigger pattern collect one extra class instance CratonVM
considers unreachable but HotSpot doesn't (or vice versa — an instance
HotSpot keeps alive one cycle longer). This smells like a GC-timing/
generation-boundary difference rather than a functional classloading bug.
