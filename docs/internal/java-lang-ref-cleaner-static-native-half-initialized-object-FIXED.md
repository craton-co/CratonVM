# `java.lang.ref.Cleaner.create()`/`.register()` NPE on `getCleanerImpl(...)` returning null — half-real object, same bug class as ThreadPoolExecutor

Status: FIXED — 2026-07-10, branch `fix/wildfly-residuals-20260710`
Severity: High — blocked WildFly Host Controller boot at `ServiceContainer` creation (before any of
`wildfly-domain-heap-corrupt-value-timeout.md`'s four front-line residuals could be reached)

## Symptom

Booting a WildFly domain (`bin/domain.sh`, `CRATONVM_MSC_REAL_START=1`, pristine 32.0.1.Final
distribution) crashed the Host Controller process immediately after the process-controller
handshake, with the T19.H1 watchdog eventually firing on a hang:

```
[Host Controller] java.lang.NullPointerException: Cannot read field "queue" because the return value of "jdk.internal.ref.CleanerImpl.getCleanerImpl(java.lang.ref.Cleaner)" is null
	at jdk.internal.ref.PhantomCleanable.<init>(PhantomCleanable.java:66)
	at jdk.internal.ref.CleanerImpl$PhantomCleanableRef.<init>(CleanerImpl.java:164)
	at java.lang.ref.Cleaner.register(Cleaner.java:224)
	at org.jboss.msc.service.ServiceContainer$Factory.create(ServiceContainer.java:257)
	at org.jboss.as.host.controller.HostControllerBootstrap$ShutdownHook.register(HostControllerBootstrap.java:65)
	at org.jboss.as.host.controller.HostControllerBootstrap.<init>(HostControllerBootstrap.java:36)
	at org.jboss.as.host.controller.Main.boot(Main.java:140)
[Host Controller] FATAL [org.jboss.as.server] WFLYSRV0239: Aborting with exit code %d
```

## Root cause — same bug class as `threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`

`native-builtins/src/phases_late.rs::register_p68_cleaner` registers `SyntheticStub`-category
natives for `java/lang/ref/Cleaner.create()`/`.register(Object,Runnable)` and
`Cleaner$Cleanable.clean()`, explicitly documented as a fallback "for when real class bytes are
unavailable" — with the stated intent that "real Cleaner bytecode still wins whenever the real
class is loaded."

Real `Cleaner.create()` bytecode (disassembled via `javap -c`) does:
```
new Cleaner()          // private ctor: this.impl = new CleanerImpl()
invokevirtual CleanerImpl.start(Cleaner, ThreadFactory)
areturn
```
i.e. it sets a real `impl` field and starts it. Our synthetic `create()` native instead does
`alloc_concurrent_synthetic(ctx, "java/lang/ref/Cleaner", 1)` and returns immediately — **never
setting `impl` at all**.

The dispatch-time "real bytecode wins" intent does NOT hold uniformly: `create()` is a **static**
factory method, and static-method dispatch has no per-instance real-vs-synthetic safety net the
way concrete instance methods do (this is the exact same asymmetry documented at length in the
companion `ThreadPoolExecutor` fix — static factories' natives win unconditionally once
registered, while concrete instance methods on a *loaded* real class generally do NOT let a
registered native win unless explicitly force-listed). So `create()`'s native fires and returns a
Cleaner with a null `impl`. `register()` (an instance method) then correctly prefers real
bytecode — which promptly NPEs reading the never-set `impl` field via `getCleanerImpl()`.

## Fix

Extended the exact same `drop_real_layout_synthetic` registry-level pattern already used for
`ThreadPoolExecutor`/`StringJoiner`/`EnumSet`/etc. (`native-api/src/registry.rs`,
`NativeMethodRegistry::register()`) to `java/lang/ref/Cleaner` and `java/lang/ref/Cleaner$Cleanable`
— drops all three synthetic natives in real-JDK mode so real bytecode constructs and wires up the
Cleaner end-to-end (matching the registration site's own already-stated intent, just now actually
enforced).

**Verified**: WildFly Host Controller boot no longer hits this NPE; boot progresses significantly
further (extensions parse, Elytron initializes, `host=foo:add()` op runs) before hitting a
different, unrelated blocker — see
`docs/known-issues/wildfly-domain-hostcontroller-sigsegv-inline-cache-null-receiver.md`.
