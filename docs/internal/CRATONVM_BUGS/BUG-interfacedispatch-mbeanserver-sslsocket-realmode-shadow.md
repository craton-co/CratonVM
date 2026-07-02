# Bug — `MBeanServer`/`SSLSocket` `invokeinterface` resolves to the abstract
# declaration (`AbstractMethodError: ... has no Code attribute`) in real-JDK mode

> **✅ RESOLUTION 2026-07-02 — FIXED.** Two independent root causes, both in the
> same failure family as TC0622's SSLSession fix (`BUG-TC0622-sslsession-buffersize-abstractmethod.md`):
> a synthetic-JDK-only native override leaking into the real-JDK registration
> path (MBeanServer), and a missing accessor registration (SSLSocket).

## Symptom

Multiple, textually unrelated interfaces threw `AbstractMethodError: method
X has no Code attribute` when real-mode Tomcat code held a reference typed
as the interface but the concrete object should have satisfied the call:

- `org.apache.catalina.manager.TestStatusTransformer` —
  `AbstractMethodError: javax/management/MBeanServer.addNotificationListener(...)V has no Code attribute`
- `org.apache.tomcat.util.net.TestSsl` —
  `AbstractMethodError: javax/net/ssl/SSLSocket.getEnabledCipherSuites()[Ljava/lang/String; has no Code attribute`

Minimal repro (no Tomcat) for the MBeanServer case:

```java
MBeanServer s = ManagementFactory.getPlatformMBeanServer();
System.out.println(s.getClass().getName()); // -> "javax.management.MBeanServer" (!)
s.addNotificationListener(new ObjectName("java.lang:type=Memory"),
    (n, h) -> {}, null, null); // -> AbstractMethodError
```

## Root cause 1 — MBeanServer (`native-builtins/src/jmx.rs`)

`register_jmx_natives` → `register_management_factory` unconditionally
registered a synthetic-object-returning override for
`MBeanServerFactory.createMBeanServer()`/`newMBeanServer()` (allocating on
the bare *interface* `javax/management/MBeanServer`, not a concrete class).
`register_jmx_natives` is called from **both** `vm_init.rs`'s synthetic-JDK
AND real-JDK registration branches (gated only by the always-on-by-default
`experimental-jmx` cargo feature, not by real-vs-synthetic mode).

The real JDK bytecode for `ManagementFactory.getPlatformMBeanServer()` calls
`MBeanServerFactory.createMBeanServer()`, whose real implementation
constructs a concrete `com.sun.jmx.mbeanserver.JmxMBeanServer` (a class that
declares `addNotificationListener` etc. with a Code attribute — interface
dispatch resolves fine on it). A prior fix (KAFKA-MBEAN) deliberately left
`getPlatformMBeanServer` itself unregistered specifically so this real
bytecode could run — but the `createMBeanServer` override one level down
was never gated the same way, so it intercepted the call *before* real
bytecode could construct `JmxMBeanServer`, silently handing back the
synthetic interface-typed object even in real-JDK mode. Every
un-overridden `MBeanServer` method (i.e. everything except the handful the
synthetic backing directly implements) then threw `AbstractMethodError`.

**Fix:** split the `MBeanServerFactory.createMBeanServer`/`newMBeanServer`
overrides out of `register_management_factory` into a new
`register_mbean_server_factory_synthetic`, called only from the
synthetic-JDK branch of `vm_init.rs`. Real-JDK mode no longer registers
anything for `MBeanServerFactory`, so the real bytecode runs end-to-end and
`getPlatformMBeanServer().getClass().getName()` now reports
`com.sun.jmx.mbeanserver.JmxMBeanServer`.

## Root cause 2 — SSLSocket (`native-builtins/src/phases_late.rs`)

`javax/net/ssl/SSLSocket` is a genuinely synthetic object in CratonVM (both
the NEW-13 client-side `SSLSocketFactory.createSocket` path and
`SSLServerSocket.accept()` allocate it directly on the interface name — no
concrete-class shadowing issue here, this receiver really is synthetic by
design). The registration block for it
(`phases_late.rs::register_p68_ssl`, real-mode reachable via
`register_essential_natives`) covered I/O and lifecycle
(`getSession`/`getInputStream`/`getOutputStream`/`close`/...) but never
registered `getSupportedCipherSuites`/`getEnabledCipherSuites`/
`getSupportedProtocols`/`getEnabledProtocols`/their setters — exactly the
same "missing accessor" shape as TC0622's `SSLSession.getApplicationBufferSize`
gap.

**Fix:** added the missing accessors, mirroring the static suite/protocol
lists already used by `SSLEngineImpl` (`t27_tls.rs`) for consistency.
`getEnabledProtocols` additionally reports the actually-negotiated protocol
read from the socket's session field.

## What did NOT reproduce — Log / DoHead

The task also reported `org.apache.juli.logging.Log.error`/`isTraceEnabled`
`AbstractMethodError`s in `TestHttpServletDoHeadInvalidWrite*` and
`TestWebSocketFrameClient`. `org.apache.juli.logging.DirectJDKLog` (the
concrete `Log` implementor, confirmed via `javap`) is real bytecode with no
synthetic involvement, unlike the two bugs above — a different shape
entirely. On this build:
- `TestWebSocketFrameClient` ran clean (`OK (4 tests)`), no Log error.
- `TestHttpServletDoHeadInvalidWrite1023ValidWrite1023` ran 280s (hit the
  already-documented, unrelated OPEN embedded-server-throughput wall) with
  no Log-related AbstractMethodError in ~3400 lines of output.

Given these DoHead tests are the subject of an extensively-documented,
still-OPEN GC root-scanning corruption bug (AQS `ConditionNode`/
`ConditionObject` register-invisibility — see
`reference_tomcat_dohead_aqs_blocked_jit_register_root` /
`reference_tomcat_dohead_reflectiondata_sidestore_gc` in project memory)
that intermittently zeroes/staleifies receiver class_ids during these exact
tests, the Log AbstractMethodError is most likely an intermittent symptom
of that already-tracked, deferred (needs precise oop maps / shadow stack)
issue rather than a new interface-dispatch defect. Did not reproduce it to
confirm the receiver's `recv_cid`/`recv_class` via `CRATONVM_DBG_NOCODE=1`
in the time available — if it recurs, triage via `CRATONVM_DBG_STALE_RECV`
alongside the existing AQS investigation rather than assuming a new itable
bug.

## task_ff0bbcca (NamingEnumeration.hasMore) — not deduped

Quick check: no `alloc_concurrent_synthetic(..., "javax/naming/NamingEnumeration", ...)`
call exists anywhere in native-builtins, unlike the MBeanServer/SSLSocket
cases. Doesn't appear to share either root cause found here; task_ff0bbcca
should proceed as its own independent investigation.

## Files changed

- `native-builtins/src/jmx.rs` — extracted `register_mbean_server_factory_synthetic`
  out of `register_management_factory`; updated doc comments and the
  `test_mbean_server_flow_methods_registered` unit test.
- `vm/src/vm/vm_init.rs` — call the new function only from the synthetic-JDK
  registration branch.
- `native-builtins/src/phases_late.rs` — added `SSLSocket` cipher-suite/
  protocol accessor natives.

## Verification

Branch `claude/compassionate-pascal-4c32ed`, worktree
`C:\craton\CratonVM\.claude\worktrees\condescending-cray-e58120`, binary
`target\release\cratonvm.exe` (built after merging dev `2ce5852f`).

- Isolated `MbsProbe.java` (`getPlatformMBeanServer().getClass().getName()`
  + `addNotificationListener`): now prints
  `com.sun.jmx.mbeanserver.JmxMBeanServer` and `addNotificationListener OK`
  (previously `javax.management.MBeanServer` +
  `AbstractMethodError`).
- `org.apache.catalina.manager.TestStatusTransformer`: `OK (3 tests)`
  (previously all 3 failed with the MBeanServer AbstractMethodError).
- `org.apache.tomcat.util.net.TestSsl`: 21 tests, 7 failures — zero
  mention of `AbstractMethodError`/`getEnabledCipherSuites` in any failure
  (previously the dominant failure mode). Remaining 7 failures are
  unrelated (e.g. `NullPointerException` in `TestSsl.testSimpleSsl` at
  line 124, a response-parsing issue) — out of scope for this fix.
- `cargo test -p cratonvm-native-builtins jmx_tests` — pending at time of
  writing, run from worktree.

Repro commands (from `apps/tomcat`, PowerShell):
```powershell
$exe = "<worktree>\target\release\cratonvm.exe"
$cp = (Get-Content ".suite\cp.txt" -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG='1'; $env:CRATONVM_DBG_NOCODE='1'
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.catalina.manager.TestStatusTransformer
& $exe -Xmx2g -cp $cp org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestSsl
```
