# TestLargeClientHello — new bug: out-of-bounds field read on PrivateKey during TLS connector init

**Status:** OPEN. **Severity:** high (blocks TLS connector startup).
**HotSpot:** PASS. **Related:** supersedes
[largeclienthello-string-size-nosuchmethod.md](../../internal/tomcat-08-07/largeclienthello-string-size-nosuchmethod-FIXED.md)
(`docs/internal/tomcat-08-07/`) — that doc's `NoSuchMethodError:
java/lang/String.size()I` symptom is confirmed genuinely fixed (does not
reproduce in this run), but the class now fails with a completely
different, unrelated bug.

## Summary

`org.apache.tomcat.util.net.TestLargeClientHello.testLargeClientHelloWithSessionResumption`
fails during TLS connector initialization, before the test's own logic
runs:
```
1) testLargeClientHelloWithSessionResumption(org.apache.tomcat.util.net.TestLargeClientHello)
org.apache.catalina.LifecycleException: Protocol handler initialization failed
	at org.apache.catalina.LifecycleException.<init>(LifecycleException.java:69)
	at org.apache.catalina.connector.Connector.initInternal(Connector.java:1279)
	at org.apache.catalina.util.LifecycleBase.init(LifecycleBase.java:128)
	at org.apache.catalina.core.StandardService.initInternal(StandardService.java:543)
```
The underlying cause, from the guard-rail warning logged just before the
failure:
```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read
  dropped (caller used slot index past receiver's layout — class layout is
  correct; the bug is in the caller's slot computation, typically a
  speculative collection-layout probe dispatched on a non-matching receiver
  type) obj=0x2a117fe8 index=4 num_slots=4 class_id=ClassId(879)
  class_name=java/security/PrivateKey real_field_count=Some(0)
```
Something in the HTTPS/JSSE connector init path (this test configures a
`https-jsse-nio` connector, matching the class name's focus on large
`ClientHello` / session-resumption TLS handshake behavior) calls
`get_field` on a `java.security.PrivateKey` object at slot index 4, but
`PrivateKey` is an interface with **zero** real declared fields
(`real_field_count=Some(0)`) — the guard correctly drops the read rather
than corrupting memory, but the connector then fails to initialize because
whatever value it needed from that (nonexistent) slot never arrives. The
guard's own diagnosis is that this is a caller-side bug: something
speculatively treats a `PrivateKey`-typed reference as if it had the field
layout of some other, unrelated class (likely a concrete key
implementation or a wrapper/holder type), and computes a slot index valid
for that other layout but not for the real interface type actually
received.

Found via a fresh Windows full-suite rerun (dev commit `f23a3f42a`,
2026-07-14, real JDK, JIT on, `CRATONVM_REAL_NET_SOCKETS=1` etc. set).
Verified via a fresh HotSpot run on the same host: PASSES.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName largeclienthello2 `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.tomcat.util.net.TestLargeClientHello
```

## Recommendation

Trace the HTTPS/JSSE connector init path this test exercises (large
`ClientHello`/session-resumption configuration — likely touches
`SSLHostConfig`/`SSLUtil`/keystore-loading code that extracts a private
key from a loaded certificate/keystore entry) to find the call site doing
a `get_field` on a `PrivateKey`-typed value at index 4. Given the guard's
own hypothesis ("speculative collection-layout probe dispatched on a
non-matching receiver type"), check whether this is a JIT-speculated
field-access site that assumed a specific concrete `PrivateKey`
implementation class (e.g. `RSAPrivateKey`/`sun.security.rsa.*Impl`) based
on prior profiling, then got a differently-shaped object (a different key
algorithm, or a proxy/wrapper) at this call — a receiver-type-confusion
bug in the same general family as other JIT speculative-dispatch findings
in this codebase, though the specific mechanism (interface-typed
`PrivateKey` with a real, non-speculative concrete implementation
expected) is distinct enough to verify independently rather than assume
it's identical to a known pattern.
