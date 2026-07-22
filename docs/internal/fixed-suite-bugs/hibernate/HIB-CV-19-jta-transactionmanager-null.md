# HIB-CV-19 — JTA `TransactionManager` is null under `TestingJtaPlatformImpl` (`Cannot invoke suspend on null`)

**Severity:** Medium — fails the entire JTA action-queue / coordinator cluster
(20+ classes: `Cannot invoke begin/getStatus/suspend on null`, "could not
locate TransactionManager to suspend", SessionFactory-build failures).
Named class: `org.hibernate.orm.test.actionqueue.JtaCustomAfterCompletionTest`.

**Status:** ✅ FIXED (VM bug) — branch `fix/hib-cv-19-jta-env-bean-stub`,
`native-builtins/src/wildfly_datasources_tx.rs`.
**Mode:** Interpreter (JIT-off census).
**HotSpot:** not affected.

## Symptom

`TestingJtaPlatformImpl.INSTANCE.getTransactionManager()` returns **null**, so
Hibernate's JTA coordinator NPEs on the first `.begin()` / `.getStatus()` /
`.suspend()` call. The named test wraps it as `AssertionError: Should not have
thrown an exception` with cause `NullPointerException: Cannot invoke begin on
null`.

## Root cause (NOT "deep JTA support missing" — a synthetic stub)

The original "deep — stand up the whole Arjuna stack" diagnosis was wrong. The
real Narayana 7.3.4 (`narayana-jta`) runs correctly under CratonVM. The TM was
null because of a **synthetic native stub that shadowed real Narayana
bytecode**:

`native-builtins/src/wildfly_datasources_tx.rs` registered a native override
of `com/arjuna/ats/jta/common/jtaPropertyManager.getJTAEnvironmentBean()`
(`native_narayana_pm_get_env_bean`) that returned a freshly-allocated **2-slot**
`JTAEnvironmentBean` with only the object-store directory set and every other
field at its zero/null default — instead of running the real method
(`BeanPopulator.getDefaultInstance(JTAEnvironmentBean.class)`, which returns a
fully-populated, cached bean with ~40 defaults).

That stub was registered as `NativeKind::Bridge` (so `CRATONVM_NO_STUBS` did not
drop it) and, by native-override priority (WP0.1), shadowed the real bytecode at
every call site — including Narayana's own internals:

```
BaseTransaction.<clinit>:
    tpe = new ThreadPoolExecutor(1, getJTAEnvironmentBean().getAsyncCommitPoolSize(), ...)
```

With the stub, `getAsyncCommitPoolSize()` read back **0** (undersized object →
out-of-bounds field read), so `ThreadPoolExecutor(1, 0, …)` threw
`IllegalArgumentException: maximumPoolSize must be positive` →
`ExceptionInInitializerError` for `BaseTransaction` →
`TransactionManager.transactionManager()` yielded null → the NPE above.

Diagnosis was nailed by a sequence of probes (`.cratonvm-suite/JtaProbe*.java`):
calling `BeanPopulator.getDefaultInstance(JTAEnvironmentBean.class)` directly
gave the correct bean (async=10), while the *identical* call routed through
`jtaPropertyManager.getJTAEnvironmentBean()` returned a zeroed, uncached bean —
isolating the native override as the culprit (not classloaders, not reflection,
not field-init).

## Fix

Remove the `getJTAEnvironmentBean` native registration (and the dead
`native_narayana_pm_get_env_bean` + its two class-name consts) so the real
Narayana bytecode runs. The WildFly synthetic-TM glue is untouched: it uses the
old `javax.transaction.*` namespace and `com/arjuna/ats/jta/TransactionManagerImple`,
both still registered; only the env-bean shadow is dropped. Nothing Rust-side
reads the synthetic env-bean.

## Second layer: recovery `TransactionStatusManager` needs real sockets

With the stub removed, real Narayana init proceeds and the recovery
`TransactionStatusManager.start()` binds a `ServerSocket` and calls
`serverSocket.getInetAddress().getHostAddress()`. CratonVM's **synthetic**
`java.net.ServerSocket` returns `null` for `getInetAddress()` → NPE in
`TxControl.<clinit>`. This is the pre-existing server-socket gap
(`reference_server_socket_gap`), resolved by the existing
`CRATONVM_REAL_NET_SOCKETS=1` route-1 gate (which also needs
`CRATONVM_REAL_AQS=1`). This layer is **not** a new VM fix — it is the existing
real-sockets gate.

## Verification (JIT-off interpreter, vs HotSpot)

| class | HotSpot | CratonVM (fix + `REAL_NET_SOCKETS=1 REAL_AQS=1`) |
|---|---|---|
| `JtaCustomAfterCompletionTest` | found=2 ok=2 | **found=2 ok=2** ✅ |
| `JpaComplianceAlreadyStartedTransactionTest` | found=1 ok=1 | **found=1 ok=1** ✅ |
| `CMTTest` | found=6 ok=5 skip=1 | reaches @@BEGIN, exceeds 600s wall ⏱ |

- **Necessity:** without the env-bean fix the test fails even *with* the socket
  gates (`ok=0 failed=2`, TM null at `BaseTransaction.<clinit>`) — the fix is
  required, not just the gate.
- **Perf:** the real recovery `TransactionStatusManager` + real sockets are very
  slow under the interpreter (a 1-test JTA class takes ~440s vs HotSpot's ~64s);
  multi-test classes (`CMTTest`) can exceed the per-class wall cap. This is a
  throughput artifact of the recovery socket path, not a correctness failure.

To green the whole JTA cluster in a suite run, set
`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1` for the recovery
`TransactionStatusManager`'s ServerSocket; the env-bean VM fix is the
prerequisite for any of them.
