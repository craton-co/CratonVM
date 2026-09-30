# A non-parallel-capable class loader gets a per-name lock instead of its own monitor

**Status: open, `--jdk-only` fixed — filed 2026-09-28 by interpreter round i1
wave 27, lane L5; fixed for `--jdk-only` by wave 28, lane L5 (below).** What
remains is `--compatible` only. Behaviour difference from HotSpot under a real
JDK image: a user class loader that never called `registerAsParallelCapable()`
is treated as parallel-capable by the JDK's own `loadClass`. Not a crash; it
changes which lock concurrent class loading takes.

## Progress (wave 28) — lane L5

Fixed under `--jdk-only`, all four places a HotSpot load of such a loader
holds the loader's monitor; `--compatible` unchanged.

* **The constructor** (`native-builtins/src/classloader_real.rs`
  `init_classloader_common_fields`, reached by the three `ClassLoader.<init>`
  `Bridge`s): under `--jdk-only` it asks the JDK's own
  `ClassLoader$ParallelLoaders.isRegistered(getClass())`
  (`loader_class_registered_parallel_capable`, one `invoke_static` per loader
  construction) and, like JDK 25's constructor (`javap -c -p
  java.lang.ClassLoader`, offsets 97-139), leaves `parallelLockMap` null and
  sets `assertionLock = this` when the class is not registered; a registered
  class gets the map and a lock object as before. A failed call keeps the map
  (the old behaviour: fail open to "parallel-capable", which only loses a
  lock HotSpot would take).
* **The JDK's `loadClass`** (the base `ClassLoader.loadClass` native,
  `cl_real_load_class_base`): the JDK method runs under
  `synchronized (getClassLoadingLock(name))`; the native took no lock at all.
  It now holds the loader's monitor for a loader whose `parallelLockMap` is
  null (`loader_locks_itself`: `--jdk-only`, real layout, user-defined
  loader), released on every path. A parallel-capable loader's per-name lock
  is still not taken by the native (see "What remains").
* **The VM-initiated load** (`vm/src/runtime/interpreter/constants.rs`
  `drive_defining_loader_load_named`, HotSpot's `ObjectLocker` in
  `SystemDictionary::resolve_instance_class_or_null`): the same predicate
  takes the loader's monitor around the `loadClass` upcall (pinned, taken
  before `catch_alloc_oom`, released after it on every path). This is the
  racing page's fix 3.
* **The built-in loaders** (`native-builtins/src/classloader.rs`
  `alloc_classloader`): the application and platform loaders are allocated
  without a constructor, so their `parallelLockMap` was null too, and
  `getClassLoadingLock` answered the loader where HotSpot answers a per-name
  lock (both classes register). They get a map under `--jdk-only`; the two
  predicates above exclude built-in loaders in any case.

**Deadlock analysis (what was checked).** The loader monitor is the lock
HotSpot takes, in the same order: VM-initiated load → loader monitor →
`loadClass` → (the JDK's `getClassLoadingLock`, now the same object,
re-entrant) → parent delegation (child before parent, as on HotSpot) →
`defineClass` → define-time supertype resolution through the same loader
(re-entrant). The inversion wave 27 feared (per-name lock inside the loader,
loader monitor outside) cannot occur: the VM lock is only taken for a loader
whose `getClassLoadingLock` IS the loader. No Rust lock is held across either
monitor: the two callers already call into Java (`loadClass`), which may
define classes and so take the class-manager write lock, so they cannot be
holding a Rust lock there; the `URLClassLoader` define locks
(`url_classloader_define_locks`) are inert under `--jdk-only`
(`ucl_try_define_local_class` refuses there). The waits use
`monitor_enter_gc_safe` (a collection can run while a thread waits for the
loader). The residual deadlock surface is HotSpot's own: a thread holding a
class's initialization while resolving through a non-parallel loader, against
a thread holding that loader and needing the class initialized.
**Run the `--jdk-only` suite and the framework boots before landing** (every
simple custom loader — test harnesses, Groovy/ByteBuddy-style loaders that do
not register — is now serialized on its own monitor, as on HotSpot).

**Verification.** `tools/probes/interp/L5/L5W27LoaderLockShape.java` (all four
rows as HotSpot under `--jdk-only`), `L5W28LoaderMonitorAtLoad.java` (VM-load
and `findClass` rows; the positive control of both new monitors) and
`L5W28BuiltinLoaderLock.java` (needs `--add-opens
java.base/java.lang=ALL-UNNAMED`).

## What remains

* `--compatible`: unchanged, deliberately. Its `registerAsParallelCapable`
  natives answer `true` without registering and its
  `SecureClassLoader.<clinit>` is a no-op (the S111r9 fat-jar workaround), so
  `ParallelLoaders` there holds no loader class but `ClassLoader`; asking it
  would serialize loaders HotSpot treats as parallel-capable. Fixing the mode
  means retiring those two shadows there too (the `--jdk-only` retirement,
  `RETIRED_SHADOW_L7_TRIPLES`), which is the owner's call (AGENTS.md).
* The base `loadClass` native takes no PER-NAME lock for a parallel-capable
  loader (HotSpot's JDK code does). Taking it would cost a
  `getClassLoadingLock` upcall (a `ConcurrentHashMap.putIfAbsent`) per
  `loadClass`; the observable difference is two threads running one
  parallel-capable loader's `findClass` for the same name at once.
* Other VM doors that call a user loader's `loadClass` without the loader
  lock (the `Class.forName` natives in `lang_class.rs`, `jboss_module_loader.rs`,
  `cglib_enhancer.rs`): see the racing page.

## Evidence

* `vm/src/vm/vm_init.rs` registers
  `native-builtins/src/classloader_real.rs` `register_classloader_real_natives`
  on both real-JDK paths ("KC26: ClassLoader constructors — the real JDK
  ClassLoader.<init> is extremely complex"). Its three `ClassLoader.<init>`
  `Bridge`s (`()V`, `(ClassLoader)V`, `(String, ClassLoader)V`) replace the
  JDK's private `ClassLoader(Void, String, ClassLoader)` constructor and call
  `init_classloader_common_fields`, which sets
  `parallelLockMap = new ConcurrentHashMap` and `assertionLock = new Object`
  unconditionally. The JDK constructor (JDK 25 bytecode, `javap -c`) sets them
  only when `ParallelLoaders.isRegistered(getClass())`, else `null` / `this`.
* `ClassLoader.getClassLoadingLock(name)` returns `this` only when
  `parallelLockMap == null`, so every loader's `loadClass(String, boolean)`
  (`synchronized (getClassLoadingLock(name))`) locks a per-name object.
* `isRegisteredAsParallelCapable()` is NOT affected under `--jdk-only`: it is
  real bytecode on a real image (its natives are synthetic-JDK only), and the
  `registerAsParallelCapable` shadow was retired there
  (`retired_shadow.rs` `RETIRED_SHADOW_L7_TRIPLES`). Under `--compatible`
  the `registerAsParallelCapable` natives answer `true` without registering,
  so the query answers `false` for every user loader.

Probe `tools/probes/interp/L5/L5W27LoaderLockShape.java` (no setup). HotSpot 25:

```
plain registered: false
plain lock is loader: true
parallel registered: true
parallel lock is loader: false
```

CratonVM (from the code, not run): `plain lock is loader: false`; under
`--compatible` also `parallel registered: false`.

## What HotSpot does

`ClassLoader`'s constructor decides both fields from `ParallelLoaders`; the
VM reads `parallelLockMap == null` (`java_lang_ClassLoader::parallelCapable`)
to decide whether `SystemDictionary` locks the loader object around a
VM-initiated load (`ObjectLocker`), so the JDK's lock and the VM's are the
same monitor for a non-parallel-capable loader.

## Recommended fix

1. `--jdk-only` only (AGENTS.md: `--compatible` unchanged): in
   `init_classloader_common_fields`, when the VM is `--jdk-only`, ask
   `ParallelLoaders.isRegistered(this.getClass())` (the real static method; the
   loader's class has finished `<clinit>`, which is where registration
   happens) and, when it answers `false`, leave `parallelLockMap` null and set
   `assertionLock = this`. Or retire the three `<init>` `Bridge`s under
   `--jdk-only` if the real constructor now runs (`new Module(this)`,
   `nameAndId`), which is the §1.4 remedy; that needs a `--jdk-only` suite run.
2. Then the VM-side lock (the racing page's fix 3): in
   `drive_defining_loader_load`, when the loader's `parallelLockMap` field is
   null, hold the loader's monitor around the `loadClass` call (pinned, taken
   before `catch_alloc_oom`, released on every path). Not before step 1: with
   per-name locks inside the loader a VM-side loader monitor inverts lock order
   against a Java thread calling `loadClass` directly (the racing page spells
   out the cycle).

Risk: step 1 serializes every simple custom loader (test harnesses, most
bytecode-generating frameworks' loaders) on its own monitor, as HotSpot does;
a CratonVM path that holds a VM lock while calling into such a loader would
now be able to deadlock where it could not. Run the `--jdk-only` suite and
the framework boots with the change before landing.

## How to verify

`L5W27LoaderLockShape`, all four rows equal to HotSpot's under `--jdk-only`.

## The `--compatible` census (wave 29, lane L5): procedure and decision rule

The `--compatible` change is to retire, in that mode too, the two shadows
that keep `ParallelLoaders` empty (the `registerAsParallelCapable` natives and
the no-op `SecureClassLoader.<clinit>`) and then take the wave-28 path. What
it changes: which user loaders serialize on their own monitor. So the census
counts the loaders that would:

1. Add a count (debug build or a `CRATONVM_DBG=access` line at exit): in
   `classloader_real.rs` `init_classloader_common_fields`, under
   `--compatible`, call the same `loader_class_registered_parallel_capable`
   query `--jdk-only` makes and count constructions of a user loader whose
   class is NOT registered, per loader class name. (It answers `false` for
   every class there today because of the shadows; so the count is only
   meaningful with the shadows retired in the census build: count the loader
   classes whose `<clinit>` CALLS `registerAsParallelCapable`, by tracing the
   native, and subtract.)
2. Run `--compatible`, JIT: the suite, Spring Boot fat jar, Tomcat, WildFly,
   Hibernate, Groovy, Mockito.
3. Decision rule: land the `--compatible` port only if every loader class the
   count names is one HotSpot also serializes (it does not register on
   HotSpot either — check with `L5W27LoaderLockShape`-style
   `isRegisteredAsParallelCapable()` on HotSpot for that class) AND the
   workloads that construct them show no wall-clock regression beyond the
   host floor on an interleaved A/B. A loader class that registers on
   HotSpot but is counted here is a bug in the census build (a shadow left
   in place), not a reason to lock it.
