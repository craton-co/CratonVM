# TestJNDIRealmIntegration — LDAP connection fails with NullPointerException (FIXED)

**Status:** FIXED on branch `fix/jndirealm-ldap-npe-20260713` (merged to `dev`).
**Test:** `org.apache.catalina.realm.TestJNDIRealmIntegration` — now
**76/76 PASS** (was 0 tests run / class-level setup failure). Matches HotSpot.

## Summary

`org.apache.catalina.realm.TestJNDIRealmIntegration` failed entirely
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
`Tests run: 0, Failures: 1` — this failed during test-class setup (an
embedded UnboundID LDAP server the test suite spins up locally), not in an
individual `@Test` method.

Not a JNDI/LDAP bug and not UnboundID's fault: the embedded server listens
fine; the client-side `com.unboundid.ldap.sdk.ConnectThread` (a background
thread UnboundID uses so `connect()` can be timed-out/interrupted) creates a
plain `java.net.Socket` via `javax.net.SocketFactory.createSocket()`, then
calls `socket.connect(SocketAddress, timeout)` on it. That real-bytecode
`Socket.connect()` → `getImpl()` call chain exposed two CratonVM bugs at
once.

## Root cause

`javax/net/SocketFactory.createSocket()` (native-builtins/src/phases_early.rs
`register_phase52_server_socket_factory` → `phase52_alloc_socket`) is
registered **unconditionally** (not gated behind `CRATONVM_REAL_NET_SOCKETS`,
unlike every other synthetic `java/net/Socket` native surface in this
codebase). It allocates the returned `Socket` via `alloc_concurrent_synthetic`,
which **skips real `java.net.Socket.<init>` entirely**. When the real
`java.net.Socket` class is loaded (always true under `--jdk real`),
`alloc_concurrent_synthetic` reuses the REAL class identity/layout (see its
own doc comment), so this "5-field synthetic" object is actually the real
5-instance-field `Socket` layout: `impl`(0), `state`(1), `socketLock`(2),
`in`(3), `out`(4).

Two bugs stacked on top of that skipped `<init>`:

1. **`socketLock` never initialized.** The real ctor's first act is
   `socketLock = new Object()`; skipping it leaves the `final Object
   socketLock` field null. Any later real-bytecode method that does
   `synchronized (socketLock)` — `getImpl()`, reached from `connect()`,
   `close()`, etc. — throws `NullPointerException: Cannot enter synchronized
   block because "this.socketLock" is null`. This is the NPE UnboundID's
   `ConnectThread` catches, re-wraps twice (`LDAPException` → `IOException` →
   outer `LDAPException`), and surfaces as the confusing triple-nested
   message above.

2. **`impl` field corrupted (found after fixing #1).** `phase52_alloc_socket`
   wrote a non-null empty `String` into field index 0 ("SOCK_HOST" in this
   file's own synthetic-mode index convention) to model an unconnected
   socket's host. On the real layout, index 0 is `impl` (`SocketImpl`), not
   `host`. A non-null-but-wrong-typed `impl` makes real `Socket.getImpl()`
   skip its lazy `createImpl()` call and virtual-dispatch `impl.create(true)`
   against the actual runtime class of the stored object —
   `NoSuchMethodError: java/lang/String.create(Z)V` — once bug #1 no longer
   masked it.

Both are instances of a known, already-documented class-of-bug in this exact
file (see the `W3-A2 side-tables` comment in `net_phase_e.rs`: "synthetic
field slots collide with real-JDK private fields"), and the `socketLock`
gap specifically already had one precedent fix
(`socket_channel.rs::sc_socket`, for `SocketChannel.socket()`'s bare-Socket
adapter). `phase52_alloc_socket` was simply never migrated to either
precedent.

## Fix

`native-builtins/src/net_phase_e.rs`: made the existing GC-safe
`re1_init_socket_locks` helper (already used by the synthetic
`Socket.<init>()V` and `ServerSocket.accept()` paths in that file) `pub(crate)`
so other modules can reuse it instead of re-deriving the same fix.

`native-builtins/src/phases_early.rs`:
- `phase52_alloc_socket`: write `null` (not an empty `String`) into field
  index 0, and call `net_phase_e::re1_init_socket_locks` as the last step to
  seed `socketLock`/`closeLock`.
- `register_phase53_socket_stubs`'s `ServerSocket.accept()` (a second,
  separately-gated bare-`Socket`-allocation site with the same
  skipped-`<init>` gap): seed the locks the same way.

`native-builtins/src/tls.rs`: `SSLSocketFactory.createSocket(String,int)` and
`createSocket(Socket,String,int,boolean)` had the identical bare-allocation
gap; seeded the same way.

## Verification

Fresh build at `dev` + fix, worktree `/data/wt-jndildap-npe-20260713` on the
Azure Linux host, real JDK, `CRATONVM_REAL_NET_SOCKETS=1` (the
`run-tomcat-suite.ps1` default):
- `org.apache.catalina.realm.TestJNDIRealmIntegration`: **OK (76 tests)**,
  was "0 tests run" before.
- `org.apache.catalina.realm.TestJNDIRealm` (sibling class, previously fixed
  for an unrelated `Hashtable.clone()` bug — see
  `13-hashtable-clone-cce-jndirealm-FIXED.md`): `testErrorRealm` (the one
  test that reaches real LDAP connect) still passes; unaffected by this fix.
  The other 3 tests in that class fail with an unrelated EasyMock
  `IllegalArgumentException: Not a mock` error (mock-registry/dynamic-proxy
  issue, confirmed independent of this fix — flagged separately, not
  regressed or introduced by this change).
- `org.apache.coyote.ajp.TestAbstractAjpProcessor` (a separate, still-OPEN
  known-issue — see `abstractajpprocessor-socket-not-connected.md`):
  confirmed unaffected (still 30/30 failing, same as before this fix) — it
  goes through `new Socket(host, port)` (a different constructor/code path,
  `net_phase_e.rs::register_re1_socket`'s `<init>(String,int)`), not
  `SocketFactory.createSocket()`, so it is a distinct root cause, not a
  residual of this bug.

## Reproduction (for regression testing)

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName jndildap `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.realm.TestJNDIRealmIntegration
```
