# TC0622 — `Hashtable.clone()` casts a synthetic native entry → `ClassCastException` (FIXED)

**Status:** FIXED on branch `fix/tc0622-hashtable-clone` (merged to `dev`).
**Test:** `org.apache.catalina.realm.TestJNDIRealm` — now **4/4 PASS** (was 3/4;
`testErrorRealm` failed). Matches HotSpot.

## Symptom

`org.apache.catalina.realm.JNDIRealm.start()` threw
`LifecycleException: Failed to start component [JNDIRealm[...]]`, caused by:

```
java.lang.ClassCastException: cratonvm/synthetic/AnonymousObject$4
                              cannot be cast to java/util/Hashtable$Entry
    at java.util.Hashtable.clone(Hashtable.java:565)
    at javax.naming.InitialContext.<init>(InitialContext.java:206)
    at javax.naming.directory.InitialDirContext.<init>(InitialDirContext.java:130)
    at org.apache.catalina.realm.JNDIRealm.createDirContext(JNDIRealm.java:2690)
```

Not an LDAP/JNDI gap: the clone happens in `InitialContext.<init>` (which
clones its environment Hashtable) *before* any socket connect.

## Root cause

CratonVM models `java.util.Hashtable` natively: `native_map_put` stores
synthetic bucket-node objects (`cratonvm/synthetic/AnonymousObject$N`) in the
slot-0 `table[]`, **not** genuine `java/util/Hashtable$Entry` instances. There
was no native registration for `Hashtable.clone()`, so it ran the real-JDK
bytecode:

```java
t.table[i] = (table[i] != null) ? (Entry<?,?>) table[i].clone() : null;
```

The `checkcast Hashtable$Entry` against the synthetic node threw `CCE`.

## Fix (bounded, additive)

Registered a native `java/util/Hashtable.clone()Ljava/lang/Object;`
(`native-builtins/src/deprecated_util.rs::native_hashtable_clone`) that shadows
the broken real-JDK body:

1. Resolve the receiver's concrete class (so `Properties.clone()` returns a
   `Properties`), defaulting to `java/util/Hashtable`.
2. Snapshot the `(key, value)` pairs from the slot-0 bucket store
   (`collect_hashtable_pairs`, a no-allocation field walk handling both the real
   `Hashtable$Entry` layout and the native HashMap-node layout — same
   discrimination as the existing `collect_hashtable` used by `keys()`/
   `elements()`).
3. Allocate a fresh natively-backed map (`new_object_initialized`) and re-`put`
   each pair through the native `put`. This matches `Hashtable.clone()`
   semantics — a fresh entry chain (deep) over the *same* key/value references
   (shallow) — without ever materialising a `Hashtable$Entry`.

GC-safety: every object key/value (and the in-progress clone) is pinned with
`pin_native_root` across the re-entrant allocating `new_object_initialized`/
`put` calls and read back with `read_native_pin` (the manifest-builder idiom in
`phases_late.rs`).

Paired force-native overrides so the native wins over the real-JDK bytecode
(mirrors the existing `keys()`/`elements()` entries):

* `vm/src/runtime/interpreter.rs::force_native_over_real_jdk_bytecode` —
  `("java/util/Hashtable","clone","()Ljava/lang/Object;")`.
* `vm/src/vm/vm_exec.rs` — `"clone"` added to the Hashtable-group method match
  (only fires when a native is registered for the exact triple, so non-map
  `clone()`s are unaffected).

Purely additive: the real-JDK `clone()` path was 100% broken for any natively
populated Hashtable, so there is nothing to regress.

## Validation

* `TestJNDIRealm` 4/4 PASS (`testErrorRealm` now gets the intended
  `CommunicationException: 127.0.0.1:12345`, not the CCE).
* Standalone `HtClone` probe vs HotSpot — **byte-identical** for plain
  Hashtable: size, all values, `instanceof Hashtable`, clone/original
  independence, shared (shallow) value identity, empty-table clone, and
  `keys()` enumeration over the clone.
* `native-builtins` unit tests green (incl. new
  `test_hashtable_clone_empty_returns_fresh_object`).

## Residual (out of scope — not TC0622)

`Properties.clone()` (inherited from `Hashtable`) now succeeds and copies the
entries correctly for the Hashtable surface (`get`/`containsKey`/`size`/`keys`
all match HotSpot), but `Properties.getProperty(key)` on the *clone* can return
`null` for copied keys. This is a **pre-existing Properties dual-model quirk**,
not a clone bug: `new_object_initialized("java/util/Properties","()V")` runs the
real-JDK `<init>`, which creates the separate JDK-25 internal
`ConcurrentHashMap map` field. Native ops read the slot-0 buckets (populated by
the re-put) and work; the real `getProperty` bytecode reads the empty internal
`map`. Plain `Hashtable` has no such separate field, which is why it is
byte-perfect. TC0622's JNDI environment is a plain `Hashtable`, so this residual
does not affect the fix. Properties.clone() previously threw CCE outright, so
this is a net improvement, not a regression.
