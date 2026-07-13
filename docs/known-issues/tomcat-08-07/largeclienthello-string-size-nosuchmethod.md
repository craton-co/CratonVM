# TestLargeClientHello — NoSuchMethodError: java/lang/String.size()I during shutdown

**Status:** OPEN. **Severity:** medium (crash-adjacent — process exits via
`System.exit(1)`). **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.tomcat.util.net.TestLargeClientHello` fails and the process
terminates via an explicit `System.exit(1)`:
```
FAILURES!!!
Tests run: 1,  Failures: 1
```
```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String.size()I"
  caller="org/apache/juli/ClassLoaderLogManager.resetLoggers(Lorg/apache/juli/ClassLoaderLogManager$ClassLoaderLogInfo;)V @pc=38"
[cratonvm] System.exit(1) called — process terminating
```
`java.lang.String` has no `size()` method in the real JDK — this is a
**dispatch/resolution bug**: some call inside
`ClassLoaderLogManager.resetLoggers()` (almost certainly a `.size()` call
on a `Collection`/`Map`/`List` field, e.g. iterating registered loggers)
is being resolved against `java.lang.String`'s method table instead of the
real receiver's class. This is the same shape of bug as the
`NoSuchMethodError: java/lang/String.setOption(ILjava/lang/Object;)V`
signature found in the
[DoHead family's new blocker](dohead-jit-heap-corruption-register-invisibility.md)
and the
[HTTP/2 test-connection cluster](http2-testconnection-socket-closed-cluster.md)
(a `Socket.setSoTimeout` call also mis-resolving into `String`'s method
surface) — three independent call sites, all wrongly resolving to
`java.lang.String`, strongly suggesting a shared root cause in CratonVM's
method/vtable resolution rather than three unrelated bugs. Worth
investigating as one cluster.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES cleanly.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName largeclienthello `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.tomcat.util.net.TestLargeClientHello
```

## Recommendation

**High priority given the recurring pattern**: search for other
`NoSuchMethodError.*String\.` occurrences across recent full-suite runs to
scope how widespread this dispatch bug is — three confirmed sites
(`String.setOption`, `String.size`, plus whatever DoHead's exact second
occurrence was) in one day's investigation suggests this could affect many
more classes than currently visible (some may be masked by earlier
failures/hangs in the same run). Trace
`org.apache.juli.ClassLoaderLogManager.resetLoggers` to find the exact
`.size()` call site, then check whether CratonVM's invokevirtual/
invokeinterface dispatch has a shared bug where a receiver's real class
gets replaced by `java.lang.String`'s method table under some condition
(stale/corrupted class-id resolution, a vtable-slot collision, or a
receiver-type confusion bug) — this is exactly the kind of receiver-
identity bug the codebase's existing "register-invisible JIT root" and
"stale zeroed oop receiver dispatch" bug families describe, so check those
first before treating this as a new, unrelated defect.
