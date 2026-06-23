# HIB-CV-29 — `StreamCorruptedException: List implementation not in base module.` — root cause: non-canonical `Class.getModule()`

**Status:** ✅ FIXED (working tree; verified `--nojit` == HotSpot). Not yet merged.
**Severity:** High — breaks `ObjectInputStream` round-trip of *any* object whose graph
contains a `java.util` `List` (every serialized `Throwable` qualifies), so it hits
caching, clustering, RMI, session replication, and JPA `testSerializableException`.
**Deterministic, `--nojit`.** HotSpot PASS.

---

## TL;DR

The original report assumed `"List implementation not in base module."` was a
**CratonVM-internal** message from CratonVM's serialization code. It is **not** —
it is a **real JDK guard**. The string lives in `java.lang.Throwable` (verified:
`grep -a` of `$JDK/lib/modules` → `java/lang/Throwable.class`).

`Throwable.readObject` validates the deserialized `suppressedExceptions` list via:

```java
// java.lang.Throwable (JDK 25)
private int validateSuppressedExceptionsList(List<Throwable> list) throws IOException {
    if (!Object.class.getModule().equals(list.getClass().getModule()))   // identity!
        throw new StreamCorruptedException("List implementation not in base module.");
    ...
}
```

`Module` does **not** override `equals`, so this is an **identity** comparison.
The real bug is in **`Class.getModule()` / module identity**, not in serialization.
There were **two** defects, both fixed:

1. **Wrong class resolved (primary).** The native took the receiver mirror and
   called `class_id_of_object(mirror)` — the mirror's *own* object class, which is
   always `java/lang/Class` (in `java.base`). So **every** `getModule()` call
   reported `java.base` regardless of which class the mirror reflected. It must
   instead resolve the **reflected** class via `class_id_from_mirror(mirror)` (the
   mirror→ClassId reverse map). Confirmed by instrumenting `module_name_of_class`:
   it was invoked with `class=java/lang/Class` on every call.

2. **Fresh `Module` per call (identity).** Even with the right module name, the
   native **allocated a brand-new `java.lang.Module` on every call**, so two
   `java.base` classes were never the same instance. Fix = a **canonical `Module`
   per module name** (real-JVM semantics: one `Module` instance per module).

Together: `java.base` classes now share one `Module` (guard passes), and
classpath/app classes correctly resolve to the **unnamed** module (`getName()` ==
`null`), so `Object.class.getModule() != MyApp.class.getModule()` — matching
HotSpot for both the `==` and `!=` cases.

## Evidence

Identity probe (`ModProbe.java`: `Object.class.getModule()`,
`ArrayList.class.getModule()`, `Collections.emptyList().getClass().getModule()`):

```
# BEFORE (dev, fresh Module per call)
Object module:    unnamed module @5f  name=java.base
ArrayList module: unnamed module @65  name=java.base
EmptyList module: unnamed module @6c  name=java.base
Object==ArrayList module: false        <-- guard throws
Object==EmptyList  module: false

# AFTER (this fix)
Object module:    unnamed module @5f  name=java.base
ArrayList module: unnamed module @5f  name=java.base
EmptyList module: unnamed module @5f  name=java.base
Object==ArrayList module: true         <-- guard passes
Object==EmptyList  module: true
```

End-to-end repro (`SerRepro.java`: serialize+deserialize a `RuntimeException` and an
`ArrayList`):

```
# BEFORE: java.io.StreamCorruptedException: List implementation not in base module.
#         at java/lang/Throwable.validateSuppressedExceptionsList(Throwable.java:1010)
# AFTER (cratonvm-h29, --nojit):  HotSpot:
#   throwable OK: boom                 throwable OK: boom
#   list OK: [x, y, z]                 list OK: [x, y, z]
#   ALL OK                             ALL OK
```

Cross-module probe (`ModProbe2.java`: a `java.base` class vs a classpath/app class)
— **matches HotSpot** after the fix:

```
                              CratonVM (--nojit)        HotSpot
user (app class) getName()    null                      null      (was "java.base")
base.isNamed()                 true                      true      (was false)
user.isNamed()                 false                     false
base == ArrayList  (java.base) true                      true
base == userAppClass           false                     false     (was true)
```

Every line now matches HotSpot.

Dispatch note: `Class.getModule()` has real JDK bytecode but is **not**
force-listed; the registered native fires anyway because `java.lang.Class` mirror
methods resolve to the registry (confirmed with a temporary firing marker).

## Fix

Two coordinated changes: resolve the **reflected** class, and a canonical
`Module`-per-module-name cache on the VM.

1. **`native-builtins/src/lib.rs`** (`register_essential_natives`, real-mode) and
   **`native-builtins/src/phases_late.rs`** (`register_p59_module`, synthetic-mode)
   — both `Class.getModule()` natives now resolve the module via
   `ctx.class_id_from_mirror(mirror)` (the mirror→ClassId reverse map), falling
   back to `class_id_of_object(mirror)` only if the reverse lookup misses. This is
   defect #1: previously they read the mirror's own class (`java/lang/Class`).
2. **`vm/src/vm/vm_init.rs`** — new field
   `SharedVm::module_mirrors: RwLock<FxHashMap<String, ObjectRef>>`
   (key = module name; `""` = the unnamed module), initialised in `new()`.
3. **`vm/src/memory/roots.rs` §8a** + **`vm/src/memory/gc.rs` §8a** — scan the
   cached mirrors as permanent GC roots and remap them after a moving collection
   (same shape as `primitive_mirrors`). Without this a moving GC would hand back a
   stale canonical ref.
4. **`native-api/src/registry.rs`** — two `NativeContext` methods:
   `get_cached_module_mirror(Option<&str>) -> Option<ObjectRef>` and
   `cache_module_mirror(Option<&str>, ObjectRef)` (defaults: `None` / no-op for
   mock contexts).
5. **`vm/src/vm/vm_exec.rs`** — VM impl backing the two methods with
   `shared.module_mirrors`.
6. The `getModule` natives consult the cache (hit → return shared instance) and on
   a miss allocate (unchanged GC-safe pin/`create_string` path) and publish via
   `cache_module_mirror`.
7. **Name dual-write (defect #3 — `isNamed()`).** The natives write the module name
   to **both** slot 0 (the synthetic 2-field Module contract the
   `register_p59_module` getName/isNamed/toString natives read) **and** the real
   `name` field by name (`set_field_by_name(m, "name", …)`). Real
   `java.lang.Module.isNamed()`/`getName()` bytecode reads the `name` field (slot 1
   in the real layout — slot 0 is `layer`); writing only slot 0 left `name` unset,
   so real `isNamed()` reported a named platform module (e.g. `java.base`) as
   UNNAMED. `set_field_by_name` no-ops when the field is absent, so it is safe in
   both the real-layout and synthetic-shape cases. In real mode
   `alloc_concurrent_synthetic("java/lang/Module", 2)` allocates the full real
   `Module` layout, so the `name` field resolves.

Net effect: every class in a module observes the **same** `Module` instance, each
class resolves to its **own** module, and named/unnamed status is correct — so JDK
identity *and* `isNamed()`/`getName()` behave like a real JVM.

## Verification

- `ModProbe2` (`--nojit`) is byte-for-byte identical to HotSpot on **every** line:
  `base.getName()=java.base`, `base.isNamed()=true`, `user.getName()=null`,
  `user.isNamed()=false`, `base==ArrayList`=`true`, `base==userClass`=`false`.
  `SerRepro` (`--nojit`) also matches HotSpot.
- Full `cargo build --release` succeeds (compilation proven); release binary built
  with sandbox disabled to allow the C-dep crates / final link.

## Follow-ups / residual

- Real-suite re-run of `org.hibernate.orm.test.jpa.EntityManagerTest`
  (`testSerializableException`) in the H2 suite — the minimal repro already
  exercises the identical JDK guard, but a full-suite confirmation is worth doing.
- JEP-403 reflection enforcement is unaffected: that path reads `Class.module_name`
  directly via `ModuleRegistry::check_deep_reflection_access` (see
  `vm/tests/new19_module_access.rs`), not `getModule()`/this cache.
