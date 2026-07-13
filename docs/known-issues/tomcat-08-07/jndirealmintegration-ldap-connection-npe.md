# TestJNDIRealmIntegration — LDAP connection fails with NullPointerException

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.realm.TestJNDIRealmIntegration` fails entirely
(0 tests run) with a class-level LDAP connection error:
```
1) org.apache.catalina.realm.TestJNDIRealmIntegration
com.unboundid.ldap.sdk.LDAPException: An error occurred while attempting to connect to server 127.0.0.1:61294:
  IOException(LDAPException(resultCode=91 (connect error), errorMessage='An error occurred while attempting to
  establish a connection to server 127.0.0.1/127.0.0.1:61294:  NullPointerException(),
  ldapSDKVersion=7.0.4, revision=2b16a372bacd6513a5fc43f479dd54bdf0bdf27a'))
	at com.unboundid.util.LDAPSDKException.<init>(LDAPSDKException.java:83)
	at com.unboundid.ldap.sdk.LDAPException.<init>(LDAPException.java:179)
	at com.unboundid.ldap.sdk.LDAPConnection.connect(LDAPConnection.java:945)
```
`Tests run: 0, Failures: 1` — this fails during test-class setup (an
embedded UnboundID LDAP server the test suite spins up locally), not in an
individual `@Test` method. The UnboundID LDAP SDK's own connection
internals hit an unhandled `NullPointerException` while trying to connect
to the just-started local LDAP listener on `127.0.0.1`, meaning either the
embedded LDAP server isn't fully ready/listening when the client attempts
to connect, or something in CratonVM's socket/NIO layer that
UnboundID's client relies on internally is returning unexpected state
(null) that the SDK doesn't defensively check for (a real bug in
UnboundID's SDK would presumably also affect HotSpot, so this is more
likely CratonVM's socket layer returning something UnboundID doesn't
expect during the connection handshake).

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on). Verified via a fresh same-session HotSpot
run: PASSES on HotSpot (the whole class runs cleanly).

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jndildap `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.realm.TestJNDIRealmIntegration
```

## Recommendation

Get a full stack trace for the inner `NullPointerException()` (currently
swallowed into the LDAPException's message with no trace of its own —
rerun with `-Ddebug` or catch/print it directly if the test harness allows)
to identify which UnboundID internal call chain hits the null. Likely
candidates given "connect error" at the socket layer: a `SocketChannel`/
`Selector` state UnboundID's NIO-based connection pool reads (e.g.
`SocketChannel.getLocalAddress()`/`getRemoteAddress()` returning null
where UnboundID assumes non-null post-connect) — cross-reference against
other `SocketChannel`/`InetSocketAddress` resolution gaps already found in
this codebase's history (this session's investigations found more than one
instance of NIO socket address resolution returning null/unresolved
unexpectedly).
