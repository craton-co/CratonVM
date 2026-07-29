# OPEN — Keycloak 26.6.1 boot: the blocker chain after RUNTIME_INIT

**Status: OPEN.** Filed 2026-07-28 as the follow-on to
`docs/internal/keycloak/keycloak-no-vertx-http-runtime-init-20260728.md`, which
fixed `Application.start()` skipping RUNTIME_INIT. These are surfaces that fix
made reachable for the first time, not residuals of it.

## Where the boot stands

`bash bin/kc.sh start-dev` with default CratonVM flags, `-Xmx4g`, `--nojit`
now runs the whole RUNTIME_INIT phase as real bytecode: the Quarkus logging
setup, Netty event loops, Vert.x, Infinispan, the Narayana recovery manager and
the Agroal/H2 datasource. The Agroal / Vert.x / net-sockets opt-in gates are
NOT needed to get there.

The blockers below are listed in the order the boot meets them. Blocker 0 is
fixed; Blocker 1 is the current wall, and it currently hides Blockers 2 and 3
(both of which were observed and characterised before Blocker 0 appeared, when
the boot ran all the way through the Liquibase migration into Keycloak's
master-realm bootstrap).

## Blocker 0 — `StackOverflowError` from `ExtHandler.setHandlers` — FIXED

`origin/dev` at `1af2f0674` regressed this boot: it died right after
`ISPN000974: Virtual threads support: enabled` with a `StackOverflowError`
whose Java stack was only 22 frames deep, and **no cause printed at all**, even
with `--verbose`. Confirmed a `dev` regression, not a RUNTIME_INIT residual, by
reverting the RUNTIME_INIT commit in the merged tree and reproducing it on pure
`dev`.

Root cause (`CRATONVM_DBG_SOE=1` names it in one line -- 8192 identical frames):
the `org/jboss/logmanager/ExtHandler.setHandlers` native delegated back to real
bytecode with `invoke_virtual_bytecode_only`, which re-dispatches on the
RECEIVER. The receiver is a `io.quarkus.bootstrap.logging.QuarkusDelayedHandler`,
whose `setHandlers` is `super.setHandlers(...); activate();` -- so the
`invokespecial ExtHandler.setHandlers` super-call landed straight back on the
native. Forever. Exactly the hazard
`NativeContext::invoke_special_bytecode_only` was added for
(`ThreadPoolExecutor.shutdown()` via `ScheduledThreadPoolExecutor`).

Fixed by `delegate_to_real_bytecode` (`native-builtins/src/lib.rs`), which tells
the two entry shapes apart by the CALLER: only a subclass of the owner can issue
`invokespecial owner.m()`, so a call from one runs the owner's own body with
invokespecial semantics, and everything else keeps re-dispatching on the
receiver so a subclass override still wins. Applied to `ExtHandler.setHandlers`
and `ExtHandler.addHandler`.

**The same hazard is latent on ~10 sibling registrations** in that same block
(`close`/`flush`/`publish`/`setLevel`/`setFormatter`/`getFormatter`/`isLoggable`
registered in the `for handler_class in [ExtHandler, QuarkusDelayedHandler]`
loop). They were left alone because the loop variable cannot be captured by the
`fn`-pointer closures `registry.register` takes; converting them needs the loop
unrolled or the owner threaded through differently. Any subclass that overrides
one of those AND super-calls it will hang the same way.

Note the throw site for the 8192-frame ceiling does not print anything without
`CRATONVM_DBG_SOE`, which is why this arrived as a silent failure. Making that
dump unconditional (or at least logging the repeated frame) would have turned a
multi-hour investigation into a one-line read.

## Blocker 1 — the boot now HANGS in the JPA/Hibernate phase

With Blocker 0 fixed, `--nojit` gets through the Quarkus logging setup (the
`quarkus.log.console.*` deprecation warnings are new), Netty's event loop, the
Narayana recovery manager and the Agroal datasource init:

```
INFO [com.arjuna.ats.jbossatx] ARJUNA032013: Starting transaction recovery manager
INFO [io.agroal.pool] Datasource '<default>': Initial size smaller than min. Connections will be created when necessary
```

...and then produces no further output for 35+ minutes (killed by `timeout`,
exit 124). Before the Blocker-0 regression the same phase completed the whole
Liquibase migration in ~10 minutes, so this is a genuine hang, not slowness --
though note the two runs are not otherwise identical (the logging setup now
actually runs, which changes what handlers exist).

Not yet root-caused. Get a thread dump / `CRATONVM_DBG_HANG_SAMPLE` on it first;
the historical suspect for a hang at exactly this point is the
`JPAConfig.startAll` -> `CompletableFuture.get` family.

## Blocker 2 — `NullPointerException: Cannot invoke "java.lang.Long.longValue()"`

Reached before the Blocker-0 regression, so it is behind Blocker 1 now. A
null-`Long` unboxing during `ApplianceBootstrap.createMasterRealm` ->
`DeclarativeUserProfileProvider.setConfiguration` ->
`RealmAdapter.updateComponent` ->
`DeclarativeUserProfileProviderFactory.validateConfiguration` ->
`UPConfigUtils.validate`.

Note the frame list our stack renderer prints for this failure interleaves and
repeats frames (`initializeProviders:165` appears five times); trust the method
names, not the exact ordering, and prefer a fresh `CRATONVM_DBG_ATHROW` capture
for the throw site.

Two benign-looking companions appear just before it and may or may not be
related:
* `NullPointerException: Cannot invoke
  "org.hibernate.resource.transaction.spi.TransactionCoordinator.isTransactionActive()"
  because "this.transactionCoordinator" is null`
* `IllegalStateException: EntityManagerFactory is closed` on
  `CertificateReloadManager.bootReload` (Keycloak logs and ignores it)

## Blocker 3 — heap exhaustion CORRUPTS the heap instead of throwing OOM

`kc.sh start-dev` defaults to `-Xms64m -Xmx512m`. Under that ceiling the boot
does not report an `OutOfMemoryError`: it corrupts the heap and dies with
`SIGILL` / `SIGSEGV` during the Liquibase migration. The GC says so first:

```
WARN cratonvm_gc::g1: g1 concurrent mark: skipping gray entry 0x... (region 67)
     with implausible header — cleanup will retain all regions this cycle
WARN cratonvm_gc::g1: g1 cleanup: implausible gray entry seen during marking
```

* NOT G1-specific: with `-XX:+UseG1GC` removed the same run panics with
  `capacity overflow` in `raw_vec` and then SIGSEGVs.
* NOT JIT-specific: `CRATONVM_DISABLE_JIT=1` reproduces it.
* Raising to `-Xmx4g` makes it *mostly* go away, but not reliably -- one JIT
  run at 4g still died with SIGILL mid-Liquibase, so there is a genuine
  nondeterministic corruption underneath the pressure, not only a
  too-small-heap effect.

This is the highest-value of the four: it is a general GC-under-pressure
defect, it is not Keycloak-specific, and it makes every measurement on this
boot flaky.

Also note the crash handler truncates: it prints the `SIGILL/SIGSEGV` banner and
`slot[r10]:` and then stops, and the `hs_err_pid*.log` it claims to have written
contains only the banner ("truncated: full report requires allocator, unsafe in
signal handler"). Whatever named the failing JIT method on other crashes did not
fire here.

## Smaller residuals seen on the way

* `NoSuchMethodError: java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite.<init>`
  from `org.jboss.threads.JDKSpecific$ThreadAccess.clearThreadLocals()` on every
  worker-thread teardown. Harmless (teardown only) but noisy, and it makes each
  dying pool thread report `terminated with error`.
* Three classes still fall back to fabricated synthetic stubs:
  `io/quarkus/arc/impl/package-info`, `io/agroal/narayana/package-info`,
  `io/quarkus/runtime/PreventFurtherStepsException`. The first two are benign
  `package-info`; the third is a real Quarkus class and worth checking.
* `Logger.getLogger("com.example.Foo").getParent()` returns an intermediate
  `com.example` logger where HotSpot returns the root logger. JUL only
  materialises ancestors that actually exist; we fabricate them. It did not
  affect level inheritance in the probe, but it is a visible divergence.
