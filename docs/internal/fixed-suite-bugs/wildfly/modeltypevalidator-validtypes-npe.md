# WildFly boot: `ModelTypeValidator.validTypes` NPE — CLOSED, and the `org/jboss/as/` (SPB.8b) JIT ban is lifted

**Status:** FIXED 2026-07-27 (branch `fix/wildfly-jbossas-jitban-20260727`).
Two VM bugs, **neither of them a JIT miscompile**. The `org/jboss/as/`
blanket ban is removed from `vm/src/jit/skip_list.rs`.

## Verdict: the "JIT-only" characterisation was wrong

The original write-up recorded a clean three-row differential — ban in place →
boots; ban lifted + JIT → `NullPointerException: ... "this.validTypes" is null`;
ban lifted + `--nojit` → boots — and concluded "confirmed JIT-specific,
allocate-then-putfield miscompile archetype".

The NPE was not produced by compiled code at all. It was a *consequence* of the
whole boot being torn down while the boot thread was still running: something
called `AbstractControllerService.stop`, which sets `controller = null`, and
every subsequent field read in the boot thread (`this.controller`,
`this.validTypes` on freshly-parsed validator objects, …) then saw a
half-dismantled server. **Whether the teardown happened depended on GC
timing**, and the JIT changes allocation/GC timing — which is why lifting the
ban correlated perfectly with the failure without causing it.

A `--nojit` differential only ever proves *JIT-timing-sensitive*. Every
GC-reachability, `Cleaner`/`ReferenceQueue` and thread-interleaving bug is
JIT-timing-sensitive.

## Root cause 1 — `Runtime.addShutdownHook` was a no-op stub

`native-builtins/src/lang_system.rs` registered

```rust
registry.register("java/lang/Runtime", "addShutdownHook",
                  "(Ljava/lang/Thread;)V", |_ctx, _args| Ok(None));
```

The hook `Thread` was dropped on the floor: not rooted, never run.
`removeShutdownHook` unconditionally returned `true`.

A shutdown hook is normally never *started*, so on HotSpot the only thing
keeping it — and everything it transitively references — alive is the
`ApplicationShutdownHooks.hooks` static map. WildFly depends on exactly that:

```java
// org.jboss.as.server.BootstrapImpl$ShutdownHook.register()
Runtime.getRuntime().addShutdownHook(this);          // ← the only strong root
synchronized (this) {
    this.container = ServiceContainer.Factory.create("jboss-as", MAX_THREADS, 30, SECONDS, false);
    return this.container;
}
```

and MSC 1.5.4 ships a *leak detector*:

```java
// org.jboss.msc.service.ServiceContainer$Factory.create(...)
LeakDetectorServiceContainer wrapper = new LeakDetectorServiceContainer(container);
cleaner.register(wrapper, container::shutdown);   // container dies when the
return wrapper;                                  // wrapper becomes unreachable
```

So the chain was: hook dropped → `BootstrapImpl$ShutdownHook` unreachable →
its `container` field (the `LeakDetectorServiceContainer` wrapper) unreachable
the moment `Main.main` returned → `Cleaner` fired → `container.shutdown()` →
`ServiceControllerImpl$StopTask` → `ServerService.stop` →
`AbstractControllerService.stop` nulls `controller` → boot thread dies with
`WFLYCTL0085` / `WFLYSRV0056`.

`Main.main` returning early is *correct*, by the way: WildFly's
`bootstrap(...).get()` completes as soon as the `JBOSS_SERVER_CONTROLLER`
service reaches UP (`BootstrapImpl$1$1.handleEvent` → `future.done(...)`, and it
`removeListener`s itself), which is long before the boot finishes. HotSpot
survives that only because the shutdown-hook map holds the chain.

**Fix.** Hooks are registered in a process-lifetime table as
`NativeContext::add_global_root` handles (a persistent, GC-remapped root), and
`removeShutdownHook` resolves each handle and compares it with its argument, so
identity matching survives object motion. Registration is idempotent per
object.

**Known gap left open:** cratonvm still does not *run* registered hooks at VM
shutdown (`System.exit`, last non-daemon thread, SIGTERM). Retaining them is
what the correctness of the *running* program depends on, which is what this
change delivers; executing them on exit is a separate behaviour change with a
much wider blast radius (every Spring Boot / Netty / logging hook would start
firing) and is deliberately not attempted here.

## Root cause 2 — `HashSet.addAll(<foreign open-addressed Set>)` returned null holes

With root cause 1 fixed the boot got much further and then died on

```
java.lang.NullPointerException: Cannot invoke
  "org.jboss.msc.service.ServiceController.getState()" because "controller" is null
    at org.jboss.as.controller.ContainerStateMonitor.createContainerStateChangeReport(ContainerStateMonitor.java:144)
```

`createContainerStateChangeReport` iterates its `problems` `HashSet`, which is
filled by `ServiceContainerImpl.awaitStability(…, failed, problems)` doing
`problems.addAll(this.problems)` where `this.problems` is an
`org.jboss.msc.service.IdentityHashSet`.

`collect_collection_elements` in `native-collections/src/lib.rs` guesses
"field 0 = `Object[]`, field 1 = `int size`" and returns `arr[0..size)`. That is
a **list** invariant. `IdentityHashSet` is open-addressed: `table` holds its
elements at hash positions with NULL holes in between, and its field layout is
exactly `(Object[] table, int size)`. The probe therefore returned the right
element *count* made of mostly nulls — plausible-looking, and only fatal when a
caller dereferences one.

Reduced to a 20-line probe (reflective, because `IdentityHashSet` is
package-private):

```
ihs.size=3
iter count=3 nulls=0
hs.size=1 containsNull=true     ← HotSpot: hs.size=3 containsNull=false
```

**Fix.** `heuristic_snapshot_is_suspect()`: if a heuristic snapshot contains
nulls **and** the receiver is not a `java/util/List`, re-derive the elements
through the collection's real `iterator()` (`collect_via_real_iterator_once`,
with a thread-local re-entrancy guard). Applied in
`collect_collection_elements_or_real` (addAll / removeAll / retainAll /
containsAll / hashCode / copy ctors) and in `native_al_to_array` (which is
registered on `AbstractCollection.toArray()` and so was returning the same
null-holed snapshot for `IdentityHashSet.toArray()`).

A `List` may legitimately hold nulls at any index, so the `List` test is what
keeps the guard honest. Note the guard must **not** consult
`is_synthetic_backed_collection`: its `al_is_list_layout` arm matches the very
`(Object[], int)` shape a foreign open-addressed set has, so it classified
`IdentityHashSet` as synthetic-backed and suppressed the guard.

The file already carried one-off special cases for Kafka's
`ImplicitLinkedHashCollection` and Jetty's `BlockingArrayQueue` for exactly this
shape; the general guard removes the need for more of them.

Regression witness: `regression-suite/src/RCollections.java` gains a `HoleySet`
(open-addressed, `(Object[] table, int size)`, null holes) and checks `addAll`,
the copy ctor, `toArray`, `containsAll`, `retainAll`, `removeAll` — plus a
negative control that a `List` containing nulls is left alone. The suite
diff-compares against HotSpot.

## How it was found — tooling that landed with the fix

`CRATONVM_DBG_FIELD_WATCH` was a bare on/off switch whose ledger was hard-wired
to the H2 `Page`/`RootReference` investigation, and whose registration hook
(`field_watch::watch(args[0])`, four sites in `interpreter.rs`) registered
**every constructed object** when set — a mutex + registry scan on every heap
field write, millions of log lines, unusable on anything bigger than the H2
repro.

It now takes a filter matched against `<class>.<field>`:

```bash
CRATONVM_DBG_FIELD_WATCH=AbstractControllerService.controller
```

and emits `[PUTFIELD-WATCH]` / `[GETFIELD-WATCH]` with the object address,
declaring class, field name/index, old+new value, the frame, and up to 24 Java
frames of the call chain. An on/off-shaped value (`1`, `true`, `on`, empty)
keeps the original H2 filter, so existing invocations are unchanged.

That produced the answer in one run — three lines total:

```
[PUTFIELD-WATCH] … field=Some("controller") … in AbstractControllerService.start pc=504
[PUTFIELD-WATCH] … field=Some("controller") … new=Object(None) in AbstractControllerService.stop pc=22
    at org/jboss/msc/service/ServiceContainerImpl.shutdown pc=19
    at jdk/internal/ref/CleanerImpl$PhantomCleanableRef.performCleanup pc=9
    at jdk/internal/ref/CleanerImpl.run pc=62
[GETFIELD-WATCH] … field=Some("controller") value=Object(None) in
    AbstractControllerService.registerModelControllerServiceInitializationBootStep
```

`performCleanup` in the stack means "the referent was collected", so the
question was never which GC phase misbehaved — it was which root cratonvm
failed to hold.

## Evidence for lifting SPB.8b

Two independent A/B campaigns on the Azure host, WildFly 32.0.1.Final, real
`standalone.sh` boot, JIT on, identical harness, runs strictly **interleaved**
so host load (this box runs many concurrent sessions; load average swung between
17 and 35 during the campaign) hits both arms equally.

**Campaign 1** — one binary, arms separated by `CRATONVM_JIT_ALLOW_PACKAGES`,
6 pairs:

| Arm | Reached `WFLYSRV0026` |
|---|---|
| ban in place | 5 / 6 |
| ban lifted | 4 / 6 |

**Campaign 2** — two binaries built from the identical tree, differing **only**
in whether the SPB.8b block is present in `skip_list.rs`, 28 pairs:

| Arm | `WFLYSRV0026` | `WFLYSRV0056` | no terminal line in 900 s |
|---|---|---|---|
| ban in place | 20 | 6 | 2 |
| ban lifted | 16 | 10 | 2 |

Every successful boot in both arms reports the identical
`Started 273 of 522 services (5 services failed …)`.

**Read this honestly.** 20/26 vs 16/26 of the runs that reached a terminal line
is 77 % vs 62 %; Fisher's exact gives p ≈ 0.36, so at this sample size the
difference is not distinguishable from noise — but the point estimate came out
in the same direction in both campaigns. The plausible mechanism is not a
miscompile: more compiled code means different allocation/GC timing, which
widens the exposure window of the flaky family below. That is the *same*
mechanism that produced this doc's original bogus "JIT-only" conclusion, so it
deserves to be re-measured rather than assumed away.

**Every failure in both arms is the same pre-existing signature**, and no
signature is unique to the lifted arm:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Object.hasNext()Z"   (also "java/lang/Object.next()Ljava/lang/Object;")
  caller="org/jboss/as/controller/registry/BasicResource.writeModel(Lorg/jboss/dmr/ModelNode;)V @pc=8"
     or  "org/jboss/as/controller/xml/VersionedNamespace.createURN(...) @pc=78"
-> WFLYCTL0083: Failed to load module org.wildfly.extension.<x>  /  WFLYCTL0193 (infinispan)
-> WFLYSRV0056
```

That is a receiver whose header reads all-zero (`ClassId(0)`) or is misread as
an array, so virtual dispatch resolves against `java/lang/Object` — a *fatal*
member of the separately-tracked open family in
`docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`,
where it is now written up with both call sites. It reproduces with the ban in
place and under `--nojit`, so it is not evidence about this ban. Each arm also
produced exactly one SIGSEGV and one-to-two runs that had not finished in 900 s
at load ~30.

**Recommendation attached to the lift:** re-run this A/B once the stale-receiver
family is closed. If the ~15-point gap survives with that noise removed, put
SPB.8b back — the lift is not worth a real boot-reliability cost.

`WFLYSRV0026` ("started (with errors)") rather than `WFLYSRV0025` in **both**
arms: the 5 failed services are the `org.wildfly.security.key-store.applicationKS`
chain, blocked by a separate provider gap —
`java.security.NoSuchAlgorithmException: no KeyStore JKS implementation for
provider ` (note the empty provider name). See
`docs/known-issues/wildfly/elytron-keystore-jks-provider-gap-20260727.md`.
Identical in both arms, so also not evidence about this ban.

## The doc's residual: the four sibling bans, re-tested individually

Each lifted alone via `CRATONVM_JIT_ALLOW_PACKAGES`, JIT on, same harness:

| Ban | Runs | Result |
|---|---|---|
| `org/jboss/modules/` (SPB.8) | 2 | 1 × `WFLYSRV0026`, 1 × the `Object.next()` stale-receiver signature |
| `org/wildfly/` (SPB.8c) | 1 | `WFLYSRV0026` |
| `org/jboss/msc/` | 2 | 1 × `WFLYSRV0026`, 1 × `CloneNotSupportedException` at `IdentityHashSet.<init>` (array `clone()` on a bad receiver) |
| `org/jboss/logging/` | 1 | `WFLYSRV0026` |
| all five together | 2 | 1 × `WFLYSRV0026`, 1 × SIGSEGV |

Every failure above is a *single* occurrence that did **not** reproduce on
repeat with the identical config — the same flakiness rate and (where
identifiable) the same stale-receiver family as the baseline. Two conclusions:

1. None of the four is now backed by a reproducible failure of its own. They are
   **left in place** anyway: with a ~1-in-4 flaky boot failure in the baseline,
   a 1–2 run sample cannot distinguish "ban unnecessary" from "ban needed", and
   the honest next step is a proper batch after the stale-receiver family is
   closed. `org/jboss/as/` is lifted because it is the only one with an
   interleaved A/B against baseline.
2. An earlier reading of this session — "the `CloneNotSupportedException` and
   the SIGSEGV are caused by `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY`
   flipping to default-ON in `c8b71911a`" — was **wrong**, and is recorded here
   so nobody re-derives it. Turning that flag off did make both configs boot,
   but so did simply re-running with the flag left ON. Single-boot conclusions
   about this app are worthless.

## Also lifted implicitly by the A/B, deliberately not removed

`CRATONVM_JIT_ALLOW_PACKAGES=org/jboss/as/` also satisfies
`package_allowed("org/jboss/as/controller/", …)` (the check is
`prefix.starts_with(entry)`), so the A/B's "ban lifted" arm was in fact running
with **WILDFLY-CONTROLLER-JIT.1** lifted too, and never reproduced that entry's
symptom (a null `AbstractOperationContext.controllerOperations` during parallel
EJB boot). The shipped state keeps that narrower ban: it is a distinct entry
with its own root-cause note ("keep controller bytecode interpreted until the
special-call backend is corrected"), so lifting it belongs to its own change.
The shipped configuration is therefore strictly more conservative than what the
A/B measured.

## Reproduction

```bash
mkdir -p /data/tmp/wf-javahome/bin
cp <cratonvm-binary> /data/tmp/wf-javahome/bin/java
chmod +x /data/tmp/wf-javahome/bin/java
mkdir -p /data/tmp/wf-run/deployments
cp -r /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/standalone/configuration /data/tmp/wf-run/
JAVA_HOME=/data/tmp/wf-javahome CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  timeout 900 bash /data/data/wildfly-dist-keep/wildfly-32.0.1.Final/bin/standalone.sh \
  -Djboss.server.base.dir=/data/tmp/wf-run
```

`standalone.sh` is not `chmod +x` in the dist — always invoke via
`bash standalone.sh`. Create `deployments/` or the scanner logs a spurious
`WFLYDS0042`. The dist's `logging.properties` on this host is set to TRACE,
which is why boots take ~100 s and logs run to ~10k lines. See
`docs/internal/fixed-suite-bugs/wildfly/wildfly-gc-barrier-boot-hang-and-harness-fixes.md`
for why the fake-JDK-home + `CRATONVM_JAVA_HOME` shape is required.

The MSC null-hole bug reproduces in a second without WildFly:

```bash
MSC=$(find /data/data/wildfly-dist-keep -name 'jboss-msc*.jar' | head -1)
javac -cp $MSC MscSetProbe.java     # Class.forName + setAccessible on IdentityHashSet
<cratonvm> --nojit -cp .:$MSC MscSetProbe
```

## Files changed

- `native-builtins/src/lang_system.rs` — shutdown-hook registry
  (`shutdown_hook_add` / `shutdown_hook_remove`), real
  `addShutdownHook` / `removeShutdownHook`.
- `native-collections/src/lib.rs` — `heuristic_snapshot_is_suspect`,
  `collect_via_real_iterator_once`, applied in
  `collect_collection_elements_or_real` and `native_al_to_array`.
- `vm/src/runtime/env_cache.rs` — `field_watch_class_matches`
  (`CRATONVM_DBG_FIELD_WATCH` value as a `Class.field` filter).
- `vm/src/runtime/interpreter.rs` — `[GETFIELD-WATCH]` ledger, Java-frame dump
  on both ledgers, class-filter gate on the four `field_watch::watch` sites.
- `vm/src/jit/skip_list.rs` — SPB.8b (`org/jboss/as/`) removed.
- `regression-suite/src/RCollections.java` — `HoleySet` coverage.

## Related

- `docs/internal/fixed-suite-bugs/wildfly/wildfly-interpreter-operand-stack-slot-stale-after-nested-alloc-FIXED.md`
  — the open stale-receiver family that owns every residual failure above.
- `docs/known-issues/wildfly/elytron-keystore-jks-provider-gap-20260727.md`
  — the 5 failed services in every boot, both arms.
- `docs/internal/jit-ban-sweep-20260725.md` — the sweep this was found under.
- `docs/internal/fixed-suite-bugs/jit-regalloc-callee-saved-clobber-family.md`
  — the family this symptom was mis-attributed to.
