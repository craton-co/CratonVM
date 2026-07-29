# RETIRED — Keycloak 26.6.1 boot: the blocker chain after RUNTIME_INIT

**Status: RESOLVED 2026-07-29** (branch `fix/kc-boot-chain-20260728`). Filed
2026-07-28 as the follow-on to the RUNTIME_INIT fix; every blocker and residual
listed here is closed. Keycloak 26.6.1 now boots to a listening HTTP server
under CratonVM:

```
INFO [io.quarkus] Keycloak 26.6.1 on JVM (powered by Quarkus 3.33.1)
     started in 163.798s. Listening on: http://localhost:8080
INFO [io.quarkus] Profile dev activated.
INFO [io.quarkus] Installed features: [agroal, cdi, hibernate-orm, jdbc-h2,
     keycloak, narayana-jta, opentelemetry, reactive-routes, rest,
     rest-jackson, smallrye-context-propagation, vertx]
```

Reproduce with the fixture scripts on the Linux build host:

```bash
CRATONVM_DISABLE_JIT=1 KCTIMEOUT=1500 \
  JAVA_OPTS_APPEND='-Dcom.arjuna.ats.arjuna.coordinator.defaultTimeout=1200' \
  bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

The `defaultTimeout` bump is the one thing still needed at the stock
`-Xms64m -Xmx512m` heap — see "Remaining, and why it is not a defect" below.
With `-Xmx4g` the stock timeout is enough (boot completes in ~218s).

## Blocker 0 — `StackOverflowError` from `ExtHandler.setHandlers` — FIXED

`origin/dev` at `1af2f0674` regressed this boot: it died right after
`ISPN000974: Virtual threads support: enabled` with a `StackOverflowError`
whose Java stack was only 22 frames deep, and **no cause printed at all**, even
with `--verbose`.

Root cause (`CRATONVM_DBG_SOE=1` names it in one line — 8192 identical frames):
the `org/jboss/logmanager/ExtHandler.setHandlers` native delegated back to real
bytecode with `invoke_virtual_bytecode_only`, which re-dispatches on the
RECEIVER. The receiver is a `io.quarkus.bootstrap.logging.QuarkusDelayedHandler`,
whose `setHandlers` is `super.setHandlers(...); activate();` — so the
`invokespecial ExtHandler.setHandlers` super-call landed straight back on the
native. Forever.

Fixed by `delegate_to_real_bytecode` (`native-builtins/src/lib.rs`), which tells
the two entry shapes apart by the CALLER: only a subclass of the owner can issue
`invokespecial owner.m()`.

**Follow-up applied 2026-07-29:** the ~10 sibling registrations in the same
block (`close`/`flush`/`publish`/`setLevel`/`getLevel`/`setFormatter`/
`getFormatter`/`isLoggable`, plus `QuarkusDelayedHandler.addHandler`/
`setHandlers`) carried the identical latent hazard and now all route through
`delegate_to_real_bytecode`. The `for handler_class in [...]` loop could not
express that (`registry.register` takes an `fn` pointer, so the closures cannot
capture the loop variable and each body needs its owner as a literal), so it is
now a local `macro_rules! register_ext_handler_shims!` invoked once per class.

## Blocker 1 — boot hangs in the JPA/Hibernate phase — FIXED

The boot stopped dead after

```
INFO [com.arjuna.ats.jbossatx] ARJUNA032013: Starting transaction recovery manager
INFO [io.agroal.pool] Datasource '<default>': Initial size smaller than min. …
```

and produced no further output for 35+ minutes, with **every** thread at 0% CPU.

`CRATONVM_DEFAULT_WATCHDOG_SEC=600` (the `--stack-dump-on-timeout` watchdog)
named it immediately:

```
tid=0 "main"               … JPAConfig.startAll → CompletableFuture.get → Signaller.block
tid=7 "JPA Startup Thread" … ConnectionPool.returnConnectionHandler
                             → StampedCopyOnWriteArrayList.size
                             → StampedCopyOnWriteArrayList.getUnderlyingArray@25
tid=8 "agroal-11"          … ConnectionPool$ValidationTask.run
                             → StampedCopyOnWriteArrayList.iterator
                             → StampedCopyOnWriteArrayList.getUnderlyingArray@25
```

`getUnderlyingArray` pc=25 is `StampedLock.readLock()`. Two threads blocked
there with no writer in sight means the write bit was stuck on.

**Root cause:** Agroal's `io.agroal.pool.util.StampedCopyOnWriteArrayList`
never calls `unlockWrite` at all. Every mutator is

```java
long stamp = lock.writeLock();
try { … } finally { optimisticStamp = lock.tryConvertToOptimisticRead(stamp); }
```

and `tryConvertToOptimisticRead(J)J` was **the one method of the StampedLock
surface CratonVM did not implement natively**. The call therefore fell through
to real JDK bytecode, which decides everything from the real `state` field —
a field the `parking_lot`-backed side-table implementation in
`native-builtins/src/stamped_lock.rs` does not drive. It returned 0, released
nothing, and the leaked write hold blocked every later `readLock()` forever.

**Fix** (`native-builtins/src/{stamped_lock.rs,util_concurrent_ext.rs}`,
`vm/src/runtime/interpreter.rs`):

* `StampedState` is now `{ version, write_held, readers }`. The version
  advances by 256 per write release, leaving the low 8 bits for mode flags —
  exactly the way the JDK reserves its low `ABITS`. A write stamp carries bit 0
  (keeping the pre-existing "odd stamp = write stamp" convention that
  `unlock(J)V` relies on), a read stamp carries bit 1, and an optimistic stamp
  carries neither. **Read and optimistic stamps had been indistinguishable**,
  which is why `tryConvertToOptimisticRead` could not have been written
  correctly against the old encoding.
* Added `tryConvertToOptimisticRead(J)J`, and completed the rest of the
  blocking surface so no part of it can fall through to bytecode again:
  `readLockInterruptibly()J`, `writeLockInterruptibly()J`,
  `tryReadLock(JLjava/util/concurrent/TimeUnit;)J`,
  `tryWriteLock(JLjava/util/concurrent/TimeUnit;)J`, `isLocked()Z`.
* `tryConvertToReadLock(J)J` is now stamp-checked (it previously ignored its
  argument), and `validate` follows the JDK in accepting a held write stamp.
* `is_stamped_lock_native_override` (`vm/src/runtime/interpreter.rs`) gained all
  of the above **plus `unlock(J)V`**, which was registered in the registry but
  missing from the force-native gate, so real bytecode could still win for it.

Seven new unit tests in `stamped_lock.rs` cover the Agroal shape directly
(`stamped_convert_to_optimistic_releases_the_write_lock` and friends).

## Blocker 2 — `NullPointerException: Cannot invoke "java.lang.Long.longValue()"` — FIXED

Thrown from Keycloak's master-realm bootstrap
(`ApplianceBootstrap.createMasterRealm` → `DeclarativeUserProfileProvider.setConfiguration`
→ `RealmAdapter.updateComponent` → `DeclarativeUserProfileProviderFactory.validateConfiguration`
→ `UPConfigUtils.validate` → `validateAttributeGroups`).

`javap` of the last frame gives the whole answer:

```
19: invokestatic  Collectors.counting:()Ljava/util/stream/Collector;
22: invokeinterface Stream.collect:(Collector;)Ljava/lang/Object;
27: checkcast     class java/lang/Long
30: invokevirtual java/lang/Long.longValue:()J
```

**Root cause:** the `COLLECTOR_TAG_COUNTING` arm of `native_stream_collect`
(`native-collections/src/lib.rs`) returned a bare `Value::Long`. `collect` is
declared `(Ljava/util/stream/Collector;)Ljava/lang/Object;`, so a primitive
there is coerced away and the caller reads back **null** — `checkcast` passes
(null casts to anything) and `longValue()` NPEs. The finisher arm and all three
`groupingBy`-downstream COUNTING arms already boxed, with a comment warning
about exactly this; the top-level arm was the one that did not.

Isolated repro (2 seconds, no Keycloak):
`list.stream().filter(…).collect(Collectors.counting())` returned `null` for
every stream shape — plain, `filter`, `map` — while `toList`/`toSet`/`joining`/
`count()` were all correct. Now matches HotSpot exactly.

## Blocker 3 — heap exhaustion aborted the VM instead of throwing OOM — FIXED

The original report was "under `-Xms64m -Xmx512m` the boot corrupts the heap and
dies with SIGILL/SIGSEGV, with G1 reporting `implausible header`".

Reduced to a 25-line probe (`OomProbe.java`: grow a `List<byte[]>` until the
heap is gone), which reproduces in seconds and shows the actual mechanism —
**not corruption, an un-catchable abort**:

```
$ cratonvm -Xmx64m -XX:+UseG1GC OomProbe 0
FATAL: heap exhausted allocating java/lang/String (46 units)
#  SIGABRT …
```

The `String` created by the loop's own `"iter=" + i` was the trigger.
`vm_object::create_java_string_uninterned` takes only a `&SharedVm`, and every
GC entry point needs the calling thread (to retire its TLAB and contribute its
roots) — so its exhaustion path was `eprintln!` + `std::process::abort()`. It
was the one allocation path in the VM that neither collected nor threw.

**Fix:** `vm_object::try_create_java_string_uninterned` (fallible) plus
`interpreter::create_string_or_oom(shared, thread, text)`, which runs the same
escalation ladder as `alloc_object_shared` — forced GC, retry, G1 last-ditch
full cycle, retry, then a catchable `OutOfMemoryError` — and
`execute_string_concat` (`vm/src/runtime/invokedynamic.rs`) now goes through it.
The probe now prints `OOM thrown correctly after allocMB=44` and the VM exits
normally, matching HotSpot.

**The corruption symptom is gone.** Five separate boots at the stock
`-Xms64m -Xmx512m` (with `-XX:+UseG1GC`, as `kc.sh` passes) produced **zero**
`implausible`, zero `capacity overflow`, zero `SIGILL`/`SIGSEGV` and zero
`FATAL` lines. The pre-fix runs that produced them were also running with the
Blocker-1 write-lock leak in place, which parked the pool threads while the rest
of the boot kept allocating.

## Smaller residuals — all FIXED

* **`NoSuchMethodError: java/lang/invoke/VarHandleReferences$FieldInstanceReadWrite.`**
  (note the *empty* method name) from
  `org.jboss.threads.JDKSpecific$ThreadAccess.clearThreadLocals()` on every
  worker-thread teardown, so every dying pool thread reported
  `terminated with error`.
  The JDK-24+ multi-release copy of that class does
  `privateLookupIn(Thread.class,…).unreflectVarHandle(threadLocals)
  .toMethodHandle(AccessMode.SET)`. Neither `Lookup.unreflectVarHandle` nor
  `VarHandle.toMethodHandle` was implemented, so real JDK bytecode ran and
  resolved through `VarForm.memberName_table` — which CratonVM's post-clinit
  fixup fills with a stub whose four tables are all null
  (`vm/src/vm/vm_util.rs`). Hence a `MemberName` with no name. It also produced
  the `g1::get_field: out-of-bounds field read … index=16..20 num_slots=8`
  warnings: a real 8-slot JDK `MethodHandle` being read through the synthetic
  MethodHandle layout, whose fields live at slots 16-20.
  Both methods are now native (`native-builtins/src/lang_invoke.rs`), building
  the same synthetic getter/setter `MethodHandle` that `Lookup.findGetter` /
  `findSetter` hand out. Access modes with no field-accessor form (the CAS and
  `GET_AND_*` families) raise `UnsupportedOperationException` rather than
  returning a handle that would silently do the wrong thing.

* **`io/quarkus/runtime/PreventFurtherStepsException` fabricated as a synthetic
  stub** even though it ships in `io.quarkus.quarkus-core-3.33.1.jar`.
  `CRATONVM_DBG_STUB_BT=PreventFurtherSteps` named the site in one backtrace:
  `find_exception_handler_impl` (`vm/src/runtime/interpreter.rs`) resolved catch
  types with `SharedVm::load_class_concurrent`, which searches only the
  bootstrap/application classpath. Every `lib/main/*.jar` of a Quarkus fast-jar
  distribution is reachable only through `RunnerClassLoader`, so the "load"
  succeeded by minting a code-less stub and registering it **globally** under
  that name — poisoning it for the loader that owns the real class.
  Fixed by refusing to lazily load a catch type when the load would fabricate a
  stub (`would_fabricate_synthetic_stub`). Nothing is lost: the by-name subclass
  test right below walks the *thrown* object's own superclass chain and needs no
  `ClassId` for the catch type at all.

* **`package-info` fabricated as a synthetic stub** — six of them on this boot
  (`io/quarkus/arc/impl`, `io/agroal/narayana`, four `org/infinispan/**`).
  A `package-info` class exists only when the package actually declares
  annotations; `java.lang.Package.getPackageInfo()` probes for it inside a
  `catch (ClassNotFoundException)`. Fabricating one answers that probe with a
  bogus non-null `Class`. `ClassManager::load_class` now raises
  `ClassNotFound` for any `*/package-info`, alongside the existing `$$` gate,
  and `would_fabricate_synthetic_stub` mirrors the rule. The boot is now free of
  stub fallbacks entirely.

* **`Logger.getLogger("com.example.Foo").getParent()` returned an intermediate
  `com.example` logger** where HotSpot returns the root.
  `allocate_logger` (`native-builtins/src/logmanager.rs`) demand-created the
  whole dotted prefix chain. Real JUL records names in a `LogNode` tree but only
  ever links a new `Logger` to the nearest ancestor that already **has** a
  Logger object, falling back to the root — and re-parents existing descendants
  when an intermediate node appears later (`LogNode.walkAndSetParent`). Both
  halves are implemented now, and a probe matches HotSpot on all four
  observations (root parent, re-parenting after the intermediate is created, the
  intermediate's own parent, and level inheritance through the chain).

## Remaining, and why it is not a defect of this chain

At the stock `-Xms64m -Xmx512m` the boot takes ~164s of wall clock, which
exceeds Narayana's **default 120s transaction timeout**: the reaper aborts the
bootstrap transaction mid-flight, Quarkus starts shutting down, and the HTTP
server start then races the teardown and surfaces as a
`NoClassDefFoundError` on whichever Netty class the shutdown reached first
(`io/netty/util/NetUtil` in two runs of three). Raising
`-Dcom.arjuna.ats.arjuna.coordinator.defaultTimeout` — or giving the VM
`-Xmx4g`, where the boot finishes in ~218s but under far less GC pressure —
boots cleanly. That is interpreter throughput and per-object footprint, both
tracked elsewhere; it is not memory unsafety and it produces no corruption.

One genuinely separate gap was found while reducing Blocker 3 and is **not**
part of this chain: the *generational* (default, non-G1) young collector
aborted the process with
`FATAL: OutOfMemoryError: GC could not relocate a live object` when to-space
filled mid-copy. `kc.sh` runs with `-XX:+UseG1GC`, so it never affected the
Keycloak boot. **FIXED 2026-07-29** in a follow-up on the same day: the
end-of-cycle expansion grew only `young_to` (the arena that had just been
reset), so after the next swap the semispaces were a full doubling apart and
Cheney's "to-space must hold all of from-space" invariant no longer held. The
semispaces are now equalised at the top of each cycle, while to-space is still
empty and growing it is legal. Heap exhaustion under the generational collector
now raises a catchable `OutOfMemoryError: Java heap space` and the VM stays
usable, matching G1 and HotSpot.

## Fixture notes

The dist lives at `/data/tmp/kc-dist/keycloak-26.6.1` on the Linux build host
and is booted through a fake `JAVA_HOME` whose `bin/java` is a copy of the
CratonVM binary, plus `CRATONVM_JAVA_HOME=/home/victor/jdk25` (`/tmp/kcboot.sh`,
`/tmp/kctime.sh`). Only **one** boot may run at a time — they share
`data/h2/keycloakdb.mv.db` and a second one fails with
`The file is locked`. A full boot is 3-5 minutes at `-Xmx4g`.
