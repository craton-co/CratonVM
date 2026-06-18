# Hibernate JTA cluster — Narayana XA transaction-completion gap + synthetic socket-loopback hang

> **Status (2026-06-17):** 🟡 **Partial.** The entry crash — `ServerSocket.getInetAddress()` → `null` →
> `TxControl.<clinit>` NPE — is **FIXED** (branch `suite-dev-run`, `native-builtins/src/net_phase_e.rs`).
> Underneath it are **two deeper layers** that are NOT localized native bugs and remain **OPEN (handoff)**:
> (1) Narayana **XA transaction completion** does not commit/release the enlisted H2 connection, and
> (2) the default-mode **synthetic `ServerSocket` accept/connect loopback** does not pair up.
> Found while running the full Hibernate ORM 8.0 suite under CratonVM (dev). Affects every test that boots
> the real Narayana/Arjuna JTA platform (`TestingJtaPlatformImpl` / `WildFlyStandAloneJtaPlatform`).

Bug-report companion (per-class symptom + the applied `getInetAddress` fix):
[`apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/JTA-txcontrol-clinit-gethostaddress-null.md`](../../apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/JTA-txcontrol-clinit-gethostaddress-null.md).

---

## Symptom (suite census)

JTA-platform tests crash with `process-died rc=1` (sometimes via `System.exit`); the class emits `@@BEGIN`
with no `@@RESULT`. HotSpot (JDK 25) passes all of them. Representative classes:
`actionqueue.JtaCustomAfterCompletionTest`, `connections.AggressiveReleaseTest`,
`connections.CurrentSessionConnectionTest`, `idgen.foreign.ForeignGeneratorJtaTest`,
`interceptor.InterceptorJtaTransactionTest`, plus more `*Jta*` / transaction tests.

---

## Layer 0 — entry crash (✅ FIXED)

`TransactionStatusManager.start` (TransactionStatusManager.java:112-115) does:

```java
serverSocket.getInetAddress().getHostAddress()
```

CratonVM's default-mode **synthetic** `java.net.ServerSocket` had **no `getInetAddress()` registration**, so
the call fell through to the real `ServerSocket.getInetAddress()` bytecode (`if (!isBound()) return null; …`)
— and the synthetic `isBound()` is `false` — returning `null` → `.getHostAddress()` NPE →
`TxControl.<clinit>` fails → `ExceptionInInitializerError` → `NoClassDefFoundError` on every reuse →
process dies.

**Fix applied:** register `java/net/ServerSocket.getInetAddress()` in `net_phase_e.rs` (re2 block, default
path). It resolves the bound IP from the shared listener registry — checking **both** the re2 side-table and
the phase-53 object-field listener id, because the synthetic `ServerSocket` surface is a split-brain across
~6 registration sites (see [`reference_server_socket_gap`] memory) — with a wildcard fallback so the caller
never gets `null`. Verified: `new ServerSocket(0,50,localhost).getInetAddress()` → `127.0.0.1` (was `null`);
`TxControl.<clinit>` no longer throws.

This is a correct, broadly-useful fix (any `ServerSocket.getInetAddress()` caller benefits), but it only
removes the **crash** — it exposes the layers below, turning the JTA cluster from a fast `rc=1` crash into a
hang.

---

## Layer 1 — Narayana XA transaction completion does not release the connection (🔴 OPEN)

`JtaCustomAfterCompletionTest` drives the **real Narayana transaction manager** directly:

```java
TestingJtaPlatformImpl.INSTANCE.getTransactionManager().begin();
scope.inEntityManager( session -> { … session.persist( new SimpleEntity("jack") ); } );
TestingJtaPlatformImpl.INSTANCE.getTransactionManager().commit();   // enlisted H2 conn must commit+release
```

Under `CRATONVM_REAL_NET_SOCKETS=1` (so Layer 2 is bypassed) the test body executes, but the per-method
`@AfterEach`:

```java
scope.getEntityManagerFactory().unwrap(SessionFactoryImplementor.class).getSchemaManager().truncate();
```

fails repeatedly with:

```
org.hibernate.tool.schema.spi.CommandAcceptanceException: Error executing DDL "truncate table SimpleEntity"
  Caused by: org.h2.jdbc.JdbcSQLTimeoutException: Timeout trying to lock table "SIMPLEENTITY"
```

**Interpretation:** the JTA `commit()` (and/or `rollback()`) does not fully **complete the enlisted H2
connection** — the H2 transaction stays open and holds the `SIMPLEENTITY` table lock. `@AfterEach`'s
`truncate` then blocks on the configured H2 `LOCK_TIMEOUT` (10 s) and fails; repeated across the test's
methods/setup, the 10 s timeouts accumulate to a >200 s wall — observed as a "hang" (rc=124). Secondary
evidence in the same log: interleaved `Table "SIMPLEENTITY" already exists` from schema setup racing the
un-released connection.

The gap is in CratonVM's **XA / JTA transaction-completion path**: resource enlistment, two-phase commit of
the H2 `XAResource`, synchronization (`beforeCompletion`/`afterCompletion`) ordering, and connection
release back to the pool. This is a Narayana-integration subsystem concern, not a single native.

---

## Layer 2 — synthetic `ServerSocket` accept/connect loopback hang (🔴 OPEN, default mode)

In **default** mode (no `CRATONVM_REAL_NET_SOCKETS`), with Layer 0 fixed, the main thread hangs earlier —
during `TransactionStatusManager` bring-up. The stack-dump watchdog (120 s) reports 3 threads but can only
dump the idle Cleaner daemon; the **main thread is stuck in native** (not dumpable), consistent with a
blocking synthetic `ServerSocket.accept()` / `Socket.connect()` that never pairs up. CratonVM's default
synthetic socket surface binds a real `TcpListener` but the accept↔connect loopback Narayana's status
manager / recovery connector rely on is not modelled end-to-end.

`CRATONVM_REAL_NET_SOCKETS=1` makes the registry drop **all** synthetic `java/net/Socket` /
`java/net/ServerSocket` natives so the real JDK `sun/nio/ch/Net` path runs — which is why Layer 2 does not
occur in that mode (and Layer 1 surfaces instead).

---

## Why this is deep (not a localized fix)

- Layer 1 = full Narayana **XA datasource + 2PC transaction completion** (enlist / prepare / commit /
  release / synchronization ordering). Getting `commit()` to actually finish the H2 connection touches the
  JTA transaction-coordinator integration, not a leaf native.
- Layer 2 = a working synthetic **socket loopback** (accept↔connect pairing) OR adopting the real-socket
  path by default for these tests.

Either layer alone is a multi-component effort; both must work for the JTA tests to pass. Recommended as a
**handoff** for dedicated JTA/XA + socket-subsystem work.

---

## Reproduction

- Suite: any affected class, e.g.
  `org.hibernate.orm.test.actionqueue.JtaCustomAfterCompletionTest`, via the `.cratonvm-suite` harness
  (`@common.args`, the Narayana `narayana-jta-7.3.4.Final` jar is on the classpath).
- Layer 0 (entry crash, standalone): `.cratonvm-suite/TxCtl.java` —
  `Class.forName("com.arjuna.ats.arjuna.coordinator.TxControl")` → `ExceptionInInitializerError` /
  NPE `getHostAddress` on null (pre-fix).
- Layer 0 (socket null, standalone): `.cratonvm-suite/jsonrepro/SSRepro.java` —
  `new ServerSocket(0,50,localhost).getInetAddress()` returns `null` (pre-fix) / `127.0.0.1` (post-fix).
- Layer 1: run an affected class with `CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1` → reaches the test
  body, then `@AfterEach` `truncate` → `Timeout trying to lock table "SIMPLEENTITY"` → rc=124.
- Layer 2: run an affected class in default mode (post-`getInetAddress`-fix binary) → hangs in
  `TransactionStatusManager` bring-up; watchdog dumps only the Cleaner thread.

## Environment toggles

- `CRATONVM_REAL_NET_SOCKETS=1` — real JDK socket path (drops all synthetic socket natives). Bypasses
  Layer 2; surfaces Layer 1.
- `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` — required for suite runs > 120 s; omit it to get the watchdog
  thread dump on hang.
