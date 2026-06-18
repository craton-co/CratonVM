# HIB-DEV-02 — Narayana `TxControl.<clinit>` NPE (`getHostAddress` on null) kills every JTA-platform test (`rc=1`)

**Severity:** High — every test that boots the Narayana/Arjuna JTA platform dies; the process exits (`rc=1`, sometimes via `System.exit`), so the test never reports a result and (in-suite) takes the VM down mid-class.
**Status:** 🟡 **PARTIAL — crash (NPE) FIXED; deeper hang REMAINS (handoff).** The `getInetAddress`-null root cause is fixed (`net_phase_e.rs`, branch `suite-dev-run`). With the fix the clinit no longer NPEs, but the now-active Narayana `TransactionStatusManager`/recovery machinery **hangs** — and the hang persists **even with `CRATONVM_REAL_NET_SOCKETS=1`**, so the residual is a deeper JTA/recovery-lifecycle issue, not just the socket address.
**Mode:** Interpreter (JIT-off census). Reproduces JIT-on too.
**HotSpot (JDK 25):** all affected classes **PASS**.

## Symptom

Classes that use the Narayana JTA platform (`WildFlyStandAloneJtaPlatform` / `TestingJtaPlatformImpl`) crash with `process-died rc=1`. The class emits `@@BEGIN` but never `@@RESULT`. In the shard logs the last line is either:

```
Error in thread "main" linkage error: no class def found: com/arjuna/ats/arjuna/coordinator/TxControl
```

or

```
[cratonvm] System.exit(0) called — process terminating
```

Both stem from the same root failure earlier in the run:

```
<clinit> failed — wrapping in ExceptionInInitializerError
   class=com/arjuna/ats/arjuna/coordinator/TxControl
   cause=java/lang/NullPointerException  Cannot invoke getHostAddress on null
```

Once `TxControl.<clinit>` fails, the class is marked erroneous and **every** later use throws `NoClassDefFoundError` → the JTA platform can't initialize → the test (or Narayana's recovery/shutdown machinery) tears the process down.

## Root cause

`com.arjuna.ats.arjuna.coordinator.TxControl.<clinit>` calls `.getHostAddress()` on a `null` `InetAddress`:

```
java.lang.NullPointerException: Cannot invoke getHostAddress on null
   at com.arjuna.ats.arjuna.coordinator.TxControl.<clinit>(TxControl.java:248)
   at com.arjuna.ats.arjuna.coordinator.TxControl.createTransactionStatusManager(TxControl.java:171)
   at com.arjuna.ats.arjuna.recovery.TransactionStatusManager.start(TransactionStatusManager.java:109)
```

Narayana 7.3.4's `TxControl` static initializer derives the transaction-manager process id from a host address. On HotSpot the lookup yields a non-null `InetAddress`; on CratonVM the same code path produces `null`, so `address.getHostAddress()` NPEs.

`InetAddress.getLocalHost()` itself is **not** null on CratonVM — but it returns a different *kind* of address than HotSpot, which is the likely trigger:

| | `InetAddress.getLocalHost()` |
|---|---|
| HotSpot | `Victor-PC/192.168.1.5` (IPv4) |
| CratonVM | `VICTOR-PC/fe80::1aac:9eaa:ed9b:e3c9` (IPv6 link-local) |

Narayana's process-id code (`com.arjuna.ats.internal.arjuna.utils.*ProcessId` / `Utility`) very likely enumerates `NetworkInterface`s or filters for an IPv4/non-link-local address and gets `null` when CratonVM only surfaces the IPv6 link-local one (or when a `NetworkInterface`/`getByName` call returns null where HotSpot returns an address). Exact null source is **TxControl.java:248** — needs the Narayana source + tracing which network call returns null on CratonVM.

## Repro (standalone, no Hibernate)

`.cratonvm-suite/TxCtl.java` (run with `@common.args` for the Narayana classpath):

```java
public class TxCtl {
  public static void main(String[] a) {
    try { Class.forName("com.arjuna.ats.arjuna.coordinator.TxControl"); }
    catch (Throwable t) { /* prints ExceptionInInitializerError -> NPE getHostAddress(null) */ }
  }
}
```

CratonVM → `ExceptionInInitializerError` / NPE; HotSpot → loads fine.

## Affected classes (CratonVM-only crashes, this dev run)

`actionqueue.JtaCustomAfterCompletionTest`, `connections.AggressiveReleaseTest`, `connections.CurrentSessionConnectionTest`, `idgen.foreign.ForeignGeneratorJtaTest`, `interceptor.InterceptorJtaTransactionTest`, and additional `*Jta*` / transaction tests that boot the Narayana platform. (Several more JTA tests appear as `FAIL`/`HANG` downstream of the same init failure.)

## Precise root cause (corrected)

The null is **not** `InetAddress` — it's `java.net.ServerSocket.getInetAddress()` returning null. `TransactionStatusManager.start` (TransactionStatusManager.java:112-115) does:

```java
serverSocket.getInetAddress().getHostAddress()   // serverSocket.getInetAddress() == null on CratonVM
```

CratonVM's default-mode **synthetic** `ServerSocket` had **no `getInetAddress()` registration at all**, so the call fell through to the real `ServerSocket.getInetAddress()` bytecode — `if (!isBound()) return null; …` — and the synthetic `isBound()` is false, so it returned null → `.getHostAddress()` NPEs. (The synthetic `ServerSocket` is a split-brain across ~6 registration sites — phases_early uses object fields, net_phase_e uses a side-table — see `reference_server_socket_gap`.)

## Fix applied (crash → no crash)

Registered `java/net/ServerSocket.getInetAddress()` in `net_phase_e.rs` (re2 block, default path): resolve the bound IP from the shared listener registry (checking both the side-table and the phase-53 object-field listener id) with a wildcard fallback so the caller never gets null.

Verified: `new ServerSocket(0,50,localhost).getInetAddress()` → `127.0.0.1` (was `null`); `TxControl.<clinit>` no longer throws.

## Residual (handoff) — Narayana XA transaction-completion gap (dug in; deep)

With the crash gone, the test runs the body and reaches `@AfterEach`, then **hangs** (rc=124). Investigated to the bottom; there are two layers, both rooted in CratonVM's incomplete Narayana JTA support:

1. **`CRATONVM_REAL_NET_SOCKETS=1` mode** — the test actually executes, then `@AfterEach`'s `getSchemaManager().truncate()` fails repeatedly with:
   ```
   org.h2.jdbc.JdbcSQLTimeoutException: Timeout trying to lock table "SIMPLEENTITY"
   ```
   The test drives the **real Narayana transaction manager** directly —
   `TestingJtaPlatformImpl.INSTANCE.getTransactionManager().begin()/commit()/rollback()` —
   enlisting the H2 connection as an XA resource. On CratonVM the JTA `commit()`
   does **not** complete the enlisted H2 connection (commit + release), so its
   table lock is held; the per-method `@AfterEach truncate` then blocks on the
   10s H2 `LOCK_TIMEOUT`, repeated across methods → cumulative >200s hang.
   This is an **XA transaction-completion** gap (enlistment / 2PC commit /
   connection release / synchronization ordering).

2. **Default (synthetic-socket) mode** — hangs earlier, in `TransactionStatusManager`
   bring-up: the main thread blocks in the synthetic `ServerSocket` accept/connect
   loopback (the watchdog could not dump the main thread — stuck in native).

Both are substantial: layer 1 is the Narayana XA datasource/transaction-completion
path; layer 2 is full synthetic-socket loopback (accept↔connect pairing). Neither
is a localized native fix. **Handoff: deep JTA/XA + socket-loopback work.**

Net effect on the JTA cluster: CratonVM goes from a fast `rc=1` crash to a hang. The `getInetAddress` fix is a correct, broadly-useful bug fix (any `ServerSocket.getInetAddress()` caller benefits), but it alone does not make the JTA tests pass — the XA transaction-completion gap does.
