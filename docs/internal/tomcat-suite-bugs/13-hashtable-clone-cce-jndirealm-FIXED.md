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

1. Resolve the receiver's concrete class (a user `Hashtable` subclass clones as
   itself), defaulting to `java/util/Hashtable`.
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

### `Properties.clone()` — the side-table half

`Properties` is **not** routed through `native_hashtable_clone`: it stores its
entries in an identity-keyed **side-table** (`properties_sidetable`), not the
slot-0 buckets, so a slot-0 re-put would clone *zero* entries. Instead it is
handled by the generic shallow `Object.clone` native
(`native-builtins/src/lib.rs::native_object_clone`), to which a `Properties`
arm was added — exactly mirroring the existing `LinkedHashMap`-overlay arm:

* The shallow field copy duplicates all heap fields (including the inherited
  Hashtable `defaults` field).
* Then `properties_sidetable::snapshot_sidetable(this)` →
  `replace_sidetable(clone, …)` gives the clone its **own independent**
  side-table of the receiver's entries (a fresh identity ⇒ empty side-table
  otherwise).

Without this, the cloned `Properties` had a new identity and therefore an empty
side-table, so `getProperty` / `stringPropertyNames` saw nothing.

## Validation

* `TestJNDIRealm` 4/4 PASS (`testErrorRealm` now gets the intended
  `CommunicationException: 127.0.0.1:12345`, not the CCE).
* Standalone `HtClone` probe vs HotSpot — **byte-identical** for plain
  Hashtable: size, all values, `instanceof Hashtable`, clone/original
  independence, shared (shallow) value identity, empty-table clone, and
  `keys()` enumeration over the clone.
* `Properties.clone()` vs HotSpot (`CloneFaith` probe) — **byte-identical**:
  entries copied, `size`, clone↔original mutation **independence** both ways,
  `instanceof Properties` / `getClass()`, and `stringPropertyNames()`.
* `Properties` `defaults` chain vs HotSpot (`DefChk` probe) — **byte-identical**:
  `getProperty` falls back through `defaults`, the 2-arg `getProperty(k, def)`
  honours the chain, multi-level `new Properties(parent)` chains resolve, and a
  clone preserves its source's defaults.
* System properties / `Properties.load` round-trip unchanged (no regression on
  the touched `getProperty` path).
* `native-builtins` (2690) and `native-collections` (69) lib tests green (incl.
  new `test_hashtable_clone_empty_returns_fresh_object`).

## `Properties.getProperty` defaults chain — also fixed

CratonVM models `Properties` entries in an identity-keyed side-table, and the
side-table `getProperty` (`native_properties_get_property_1`) previously checked
the side-table → system properties → `null` with **no `defaults` fallback**, so
`new Properties(def).getProperty(keyOnlyInDef)` returned `null` (a pre-existing
gap affecting the original, not just clones). Two coordinated fixes:

1. **Storage** (`native-collections::native_props_init_defaults`): the
   `Properties(Properties)` constructor stored the defaults ref at the native
   model's `PROPS_FIELD_DEFAULTS` (slot 3), which on the real 12-field
   `Properties` layout is `loadFactor` (a float) — so the reference was
   misplaced and unreadable. It now *also* writes the defaults into the real
   `defaults` field, resolved **by name** (`resolve_field_index`), so the
   reference lands where readers expect it regardless of layout.
2. **Lookup** (`native_properties_get_property_1`): after a side-table miss it
   reads the `defaults` field (by name) and, if present, recurses through the
   defaults Properties' own `getProperty` — naturally walking multi-level
   chains — *before* the system-property fallback (so a Properties' own defaults
   win over a same-named system property). The 2-arg `getProperty(k, def)` now
   delegates to the 1-arg path and substitutes the caller's default only on a
   true null, matching the JDK.
