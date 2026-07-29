# OPEN — Keycloak 26.6.1 boot: the blocker chain after RUNTIME_INIT

**Status: OPEN.** Filed 2026-07-28 as the follow-on to
`docs/internal/keycloak/keycloak-no-vertx-http-runtime-init-20260728.md`, which
fixed `Application.start()` skipping RUNTIME_INIT. These are surfaces that fix
made reachable for the first time, not residuals of it.

## Where the boot stands

With default flags (`bash bin/kc.sh start-dev`, `-Xmx4g`, `--nojit`) the boot
now runs the whole RUNTIME_INIT phase and stops in Keycloak's **master-realm
bootstrap**:

```
INFO  [org.infinispan.CONTAINER] ISPN000974: Virtual threads support: enabled
INFO  [org.infinispan.CONTAINER] ISPN000556: Starting user marshaller '...ImmutableProtoStreamMarshaller'
INFO  [org.keycloak...QuarkusJpaUpdaterProvider] Initializing database schema. Using changelog META-INF/jpa-changelog-master.xml
...   (Liquibase migration completes)
ERROR [org.keycloak...ExecutionExceptionHandler] ERROR: Failed to start server in (development) mode
ERROR [org.keycloak...ExecutionExceptionHandler] ERROR: Cannot invoke "java.lang.Long.longValue()"
```

Everything before that is real bytecode: Netty event loops, Vert.x, Infinispan,
Narayana JTA recovery, the Agroal/H2 datasource, the Hibernate SessionFactory,
and the full Liquibase schema migration. The Agroal / Vert.x / net-sockets
opt-in gates are NOT needed to reach this point.

## Blocker 1 — `NullPointerException: Cannot invoke "java.lang.Long.longValue()"`

A null-`Long` unboxing during `ApplianceBootstrap.createMasterRealm` ->
`DeclarativeUserProfileProvider.setConfiguration` ->
`RealmAdapter.updateComponent` ->
`DeclarativeUserProfileProviderFactory.validateConfiguration` ->
`UPConfigUtils.validate`. Not yet root-caused. Reproduce with:

```bash
JAVA_OPTS_KC_HEAP='-Xms512m -Xmx4g' CRATONVM_DISABLE_JIT=1 \
  bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false --verbose
```

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

## Blocker 2 — heap exhaustion CORRUPTS the heap instead of throwing OOM

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

This is the higher-value of the two blockers: it is a general
GC-under-pressure defect, it is not Keycloak-specific, and it makes every
measurement on this boot flaky.

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
