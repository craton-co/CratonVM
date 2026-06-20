# Hibernate JTA cluster — Narayana XA transaction-completion gap + synthetic socket-loopback hang

> **✅ RESOLVED (2026-06-20).** Re-investigated end-to-end against current `dev` with a freshly-built binary
> + a programmatic Hibernate 8.1 + Narayana 7.3.4 + H2 bootstrap (`_hibrepro/HibBoot`, mirrors
> `JtaCustomAfterCompletionTest.success`). **A full `TM.begin()` / `persist` / `TM.commit()` / read-back /
> `schemaManager.truncate()` cycle now passes in DEFAULT (synthetic-socket) mode**, matching
> `CRATONVM_REAL_NET_SOCKETS=1`. **Two of the three original diagnoses were wrong:**
>
> - **Layer 0 (entry crash):** ✅ on `dev` (`ada6cebf`, `net_phase_e.rs getInetAddress`). Confirmed.
> - **Layer 1 (Narayana XA-completion "gap"):** ✅ **was never broken on `dev` — REFUTED.** `XaProbe`
>   (`TM.begin → enlistResource → TM.commit`) and `HibBoot` both show `enlist=true → XA.commit(onePhase=true)
>   → commit OK`; the enlisted H2 connection commits and releases (read-back sees the row; `truncate` does
>   **not** block on `LOCK_TIMEOUT`). The "commit-walk never drives the resource / held lock" theory is wrong.
> - **Layer 2 (synthetic socket "loopback never pairs"):** ✅ **FIXED** on branch `fix/hib-jta-xa-loopback`
>   (commit `e0426050`, **pending merge to `dev`**) — and that diagnosis was wrong too: it is **not** a
>   loopback-pairing/thread problem. Root cause = a **process-wide deadlock**. `re2_accept_into`'s no-timeout
>   branch (and `p72 implAccept` / `s2_blocking_accept`) held the global `s2_registry` lock **across the
>   blocking `TcpListener::accept()`**. Narayana's `TransactionStatusManager` Listener thread (no `SO_TIMEOUT`,
>   so it takes that branch) blocks in `accept()` holding the lock, while the main thread's `SocketProcessId`
>   bind (`s2_alloc_listener → s2_registry().lock()`) waits for the same lock forever → hang at recovery
>   bring-up. Fix = clone the listener (`try_clone`) under a short lock, drop the lock, then block on
>   `accept()` on the clone. A second, independent bug was fixed in the same commit:
>   `new ServerSocket(0).getLocalPort()` returned `0` (the winning `getLocalPort` native — native-io
>   `ss_wrapper_local_port` — returned 0 for a plain socket with no channel back-ref); now it reads a shared
>   `native-api::server_socket_ports` registry that `re2_bind_listener` populates.
>
> Pinned via the native dispatch ring (`CRATONVM_ENABLE_NATIVE_RING=1`): the main thread sat in
> `SocketProcessId.createSocket → new ServerSocket(...)` blocked on `s2_registry().lock()`. The historical
> "deep handoff" write-up below is retained for the trail; the per-layer status above is authoritative.
>
> ---
>
> _Original status (2026-06-17, superseded):_ 🟡 **Partial.** The entry crash — `ServerSocket.getInetAddress()`
> → `null` → `TxControl.<clinit>` NPE — is **FIXED** (branch `suite-dev-run`). Underneath it were claimed
> **two deeper OPEN layers**: (1) Narayana XA completion doesn't commit/release the enlisted H2 connection,
> and (2) the default-mode synthetic `ServerSocket` accept/connect loopback doesn't pair. Both refuted/fixed
> above. Found running the full Hibernate ORM 8.0 suite under CratonVM (dev).

Bug-report companion (per-class symptom + the applied `getInetAddress` fix):
[`apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/JTA-txcontrol-clinit-gethostaddress-null.md`](../../apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/JTA-txcontrol-clinit-gethostaddress-null.md).

---

## Symptom (suite census)

JTA-platform tests crash with `process-died rc=1` (sometimes via `System.exit`); the class emits `@@BEGIN`
with no `@@RESULT`. HotSpot (JDK 25) passes all of them. Representative classes:
`actionqueue.JtaCustomAfterCompletionTest`, `connections.AggressiveReleaseTest`,
`connections.CurrentSessionConnectionTest`, `idgen.foreign.ForeignGeneratorJtaTest`,
`interceptor.InterceptorJtaTransactionTest`, plus more `*Jta*` / transaction tests. In the shard logs the
last line is either `linkage error: no class def found: com/arjuna/ats/arjuna/coordinator/TxControl` or
`[cratonvm] System.exit(0) called — process terminating`; both stem from the same `TxControl.<clinit>`
NPE below (once `<clinit>` fails the class is poisoned and every later use throws `NoClassDefFoundError`).

> **Consolidation note (2026-06-18):** this doc is the single source for the Hibernate JTA cluster.
> The former `hibernate-jta-txcontrol-getinetaddress-per-class-report.md` (HIB-DEV-02) described the
> *same* cluster from the per-class angle and has been folded in here (its unique
> `InetAddress.getLocalHost()` IPv4-vs-IPv6 detail is in Layer 0 below).

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

> **Red-herring ruled out (folded in from HIB-DEV-02):** the first theory blamed `InetAddress.getLocalHost()`,
> because CratonVM returns an IPv6 link-local (`VICTOR-PC/fe80::…`) where HotSpot returns IPv4
> (`Victor-PC/192.168.1.5`). That is a real difference but **not** the null source — the null is
> `ServerSocket.getInetAddress()` (above), not `InetAddress`. Standalone repro of the crash:
> `.cratonvm-suite/TxCtl.java` (`Class.forName("com.arjuna.ats.arjuna.coordinator.TxControl")` →
> `ExceptionInInitializerError`); socket-null repro: `.cratonvm-suite/jsonrepro/SSRepro.java`.

This is a correct, broadly-useful fix (any `ServerSocket.getInetAddress()` caller benefits), but it only
removes the **crash** — it exposes the layers below, turning the JTA cluster from a fast `rc=1` crash into a
hang.

---

## Layer 1 — Narayana XA transaction completion does not release the connection (🔴 OPEN)

### Refinement 2026-06-18 — pinned to the real-Narayana `commit()` → `XAResource.commit()` callback (NOT a synthetic stub)

Deeper dig narrows Layer 1 and **rules out two tempting wrong turns**:

- **The synthetic JTA/XA stub (`native-builtins/src/wildfly_datasources_tx.rs`) is INERT for this test.**
  It registers `javax/transaction/TransactionManager` + `javax/transaction/xa/XAResource`, but Hibernate 8 /
  Narayana 7.3.4 drive **`jakarta.transaction`** (classpath: `jakarta.transaction-api-2.0.1`,
  `wildfly-transaction-client-3.0.5`), so the TM natives never match — real Narayana bytecode runs. The
  `XAResource` natives also do **not** fire: native dispatch keys on the **resolved declaring class**
  (`interpreter.rs:13029` `find(&declaring_name, …)` + the below-declaring walk at `:18147`), and the enlisted
  resource is Hibernate's concrete `org.hibernate.testing.jta.JtaAwareConnectionProviderImpl$XAResourceWrapper`,
  which provides its own `commit()`. So the `wildfly_datasources_tx.rs` `jdbc_conn_id: None` no-op commit is a
  **red herring** for this test. (It would only bite a path that calls bare `XAResource`/`javax` TM.)
- **H2 `connection.commit()` is real bytecode** — `org/h2/jdbc/JdbcConnection` is not intercepted by any
  native (verified), and basic H2 connect/commit works (`.cratonvm-suite/H2ConnProbe`). So the downstream
  commit link is sound.

**What the SQL trace proves (`.cratonvm-suite/jta_real.{out,err}`):** the test-body `insert into SimpleEntity …`
**executes** (acquires the row/table lock), then EVERY following `drop`/`truncate`/`create table SimpleEntity`
on the schema-management connection fails `Timeout trying to lock table "SIMPLEENTITY"` / `already exists`.
Hibernate's `JtaAwareConnectionProviderImpl` closes an **enlisted** connection ONLY via
`XAResourceWrapper.commit()` (its `closeConnection` is a deliberate no-op for enlisted connections —
"the XAResource wrapper takes that responsibility"). Therefore the held lock proves:

> **`TM.commit()` runs but never drives the enlisted `XAResourceWrapper.commit(xid, onePhase=true)` callback**
> (which would do `connection.commit()` + `pool.delist()`). The H2 connection is left with its INSERT
> uncommitted, holding the `SIMPLEENTITY` lock; the next test's DDL blocks on `LOCK_TIMEOUT` (10 s) and fails,
> cascading to the >200 s "hang". (Rollback would also release the lock, so completion drives **neither**
> commit nor rollback on the resource — consistent with `commit()` either skipping the record list or throwing
> inside Narayana before it reaches the resources.)

**So Layer 1 is a real-Narayana transaction-completion gap on CratonVM**, NOT a synthetic-stub bug. The exact
sub-link (enlist didn't record the `XAResourceRecord` · the 1PC commit-walk over Narayana's `RecordList` is
skipped · `commit()` throws inside Arjuna before driving resources) was **not yet empirically pinned** because
live reproduction is currently blocked by heavy CPU contention from a concurrent peer suite session (~10 stray
`cratonvm_hibdev.exe`; even Narayana TM bootstrap can't make progress under it).

**Decisive confirmation tool (left in place):** `.cratonvm-suite/XaProbe.java` — minimal
`TM.begin() → getTransaction() → tx.enlistResource(MyXA) → TM.commit()` that prints which `XAResource`
callbacks fire. Run when contention clears:
`CV=…/cratonvm.exe; CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 cratonvm --java-home <jdk> @common.args XaProbe`.
Expected-good = `@@ XA.commit onePhase=true`; if that line is absent (but `@@ enlistResource returned=true`),
the gap is Narayana's commit-walk; if `enlistResource` returns false / throws, the gap is enlistment;
if `commit()` throws, capture the Arjuna stack. That result selects the next fix target inside the real
Narayana/Arjuna `BasicAction.End()` → `XAResourceRecord.topLevelOnePhaseCommit()` path.

### Static trace of Arjuna `BasicAction.End()` 1PC (prediction — `javap`-confirmed structure; pending live `XaProbe` to pick the link)

Decoded the real commit path in `narayana-jta-7.3.4.Final.jar` (Narayana 7 bundles arjuna-core):

```
TransactionImple.commit() -> commitAndDisassociate() -> AtomicAction.commit(true) -> BasicAction.End(true):
    202: doOnePhase()         -- ifeq -> 2PC
    211: onePhaseCommit(..)   -- 1PC: XAResourceRecord.topLevelOnePhaseCommit() -> XAResource.commit(xid, true)
    229: prepare()            -- 2PC: XAResourceRecord.topLevelPrepare()        -> XAResource.prepare(xid)  <-- Hibernate wrapper THROWS here
    356/318: phase2Commit/Abort()
```

`BasicAction.doOnePhase()` (decoded) returns **true (-> 1PC)** iff **all three**:
`TxControl.onePhase` (static) **&&** `pendingList.size() == 1` **&&** `pendingList.peekFront().isPermittedTopLevelOnePhaseCommit()`.
`CoordinatorEnvironmentBean.commitOnePhase` defaults to **`true`** (ctor `iconst_1`); `TxControl.<clinit>` sets
`onePhase = …isCommitOnePhase()` **early** (before the socket/`getInetAddress` work Layer 0 fixed), so post-Layer-0
`onePhase` should be `true` => a config-driven 2PC is **unlikely** (the live `XaProbe` now prints this static directly).

**Ranked prediction** (symptom = *lock held* = the enlisted resource gets **neither** `commit()` **nor** `rollback()`):
1. **Most likely — enlist gap.** `tx.enlistResource()` returns false / doesn't add the `XAResourceRecord` to
   `pendingList` (Narayana tracks resources in a `HashMap _resources` + `isSameRM` dedup — a CratonVM
   collection/identity sensitivity). Empty `pendingList` => `End()` processes nothing => the H2 connection (held in the
   JTA `synchronizationRegistry` under `CONNECTION_KEY`, and *not* closed by the provider because it's "enlisted") is
   never committed/rolled back => lock held. **Fits the symptom exactly.**
2. **Commit-walk gap.** Record IS in `pendingList` but `End()`/`onePhaseCommit` doesn't drive it (wrong action status,
   or a throw before the record loop). Same lock-held result.
3. **Less likely — 2PC mis-taken.** `doOnePhase()` false (`size!=1` or `isPermitted==false`) -> `prepare()` ->
   Hibernate `XAResourceWrapper.prepare()` throws `"this should never be called"`. *Caveat:* a clean `phase2Abort()`
   would `rollback()` -> **release** the lock (contradicting the symptom), so this fits only if Narayana leaves the
   prepare-failed resource un-rolled-back.

**`XaProbe` decision table** (prints `TxControl.onePhase`; TX1 non-throwing prepare reveals the path, TX2 throws like
Hibernate): `@@1 enlistResource returned=false` -> (1) enlist gap · `=true` with no `@@1 XA.*` -> (2) commit-walk gap ·
`@@1 XA.prepare` present -> (3) 2PC mis-taken (`@@2 commit THREW …` reproduces the real failure) ·
`@@1 XA.commit onePhase=true` only -> 1PC fine, look at connection-handling. **Still blocked on a live run** (peer-suite
CPU/RAM saturation); fire `run-xaprobe.sh` when free.

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
