# BUG-J — `ResourceBundle` getObject/handleGetObject native shadows real `ListResourceBundle` subclasses (DOCUMENTED)

**Test:** `jakarta.el.TestResourceBundleELResolver` (`testGetValue03`).
**Symptom:** `ResourceBundleELResolver.getValue(ctx, new TesterResourceBundle(),
"key1")` returns `null` instead of `"value1"`. HotSpot: PASS. **Status: FIXED.**

> **FIX (`locale_resources.rs` `rb_get_object`):** Detect a non-synthetic
> receiver (`class_id_of(this)` ≠ `java/util/ResourceBundle`) and resolve the
> key from the subclass's overridden `getContents()` (`Object[][]`), throwing
> `MissingResourceException` when absent (which the EL resolver catches to
> produce `"???key???"`). `getContents` has no native, so this can't recurse
> back into the override (avoiding the loop the `invoke_virtual(handleGetObject)`
> approach risked). Synthetic `getBundle` bundles keep the field-0 map path.
> Verified: `TestResourceBundleELResolver` 13/13.

## Root cause

`native-builtins/src/locale_resources.rs` registers natives on
`java/util/ResourceBundle` for `getObject`, `getString`, `handleGetObject`,
`containsKey`, `getKeys` (and `getBundle`). These read a synthetic backing
`HashMap` from field 0 — correct for the synthetic bundle objects that
CratonVM's `getBundle` native fabricates (allocated as a raw
`java/util/ResourceBundle` with field 0 = map), but **wrong for a real
`ListResourceBundle` subclass** instantiated from bytecode (`new
TesterResourceBundle()`), whose field 0 is the real `ResourceBundle` layout, not
a map. `getObject` (final on `ResourceBundle`) therefore reads the wrong slot
and returns `null` instead of running the subclass's real
`getContents()`→`handleGetObject` lookup.

## Suggested fix (deferred — risk to the synthetic locale-data path)

Gate the `getObject`/`getString`/`handleGetObject`/`containsKey` natives on
`class_id_of(this) == java/util/ResourceBundle` (the synthetic marker). For a
real subclass receiver, run the subclass's real `handleGetObject` (reachable via
`invoke_virtual`, since the native is registered on `ResourceBundle`, not on the
overriding subclass) and reproduce `ResourceBundle.getObject` semantics
(parent-chain walk + `MissingResourceException` on a missing key — the resolver
relies on the MRE to produce `"???key???"`). Deferred because the synthetic
`getBundle` locale-data path (FormatData/CurrencyNames/… used by e.g. XML
serialization, Hazelcast) depends on the current native behavior and must be
verified not to regress.
