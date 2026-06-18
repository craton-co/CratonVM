# 13 — `Map.Entry.setValue()` did not write back to the map

**Status:** FIXED (collections) — `native-collections/src/lib.rs`.
**Affected:** StripSecretsUtilsTest (now ✓), and **any** `entrySet()` read-modify-write.
**Scope:** fundamental — not keycloak-specific.

## Symptom
`StripSecretsUtilsTest.stripRealm` / `stripComponent` failed with an empty-message
`org.junit.ComparisonFailure`: secret config values were not masked.
`StripSecretsUtils.stripComponentConfigMap` masks via
`map.entrySet().iterator()` → `entry.setValue(maskedList)`, but the masked values never
landed in the map.

## Root cause
CratonVM's native `Map.Entry` objects are **detached snapshots**. `native_entry_set_value`
only updated the entry object's own value slot and never propagated to the backing map, so
`entrySet().iterator().next().setValue(v)` was silently a no-op:

```java
HashMap<String,Integer> h = new HashMap<>(); h.put("a", 1);
h.entrySet().iterator().next().setValue(99);
h.get("a");   // CratonVM: 1 (should be 99)
```

The entries returned by the iterator are materialised in **three** places, all of which
built bare key/value entries with no link back to the source map:
1. `native_map_entry_set` (the `entrySet()` HashSet builder),
2. `resync_view_set` (live-view refresh on each read),
3. **`collect_view_snapshot_ordered`** — the one `native_hs_iterator` actually returns
   entries from (2-field `AbstractMap$SimpleEntry`).

## Fix
Give the live entries a 3rd field = the source map (`key@0, value@1, sourceMap@2`) in all
three builders, and make `native_entry_set_value` write through:

```rust
ctx.set_field(this, 1, new_val);
if ctx.object_num_fields(this) >= 3 {
    if let Value::Object(Some(src_map)) = ctx.get_field(this, 2) {
        let key = ctx.get_field(this, 0);
        native_map_put(ctx, &[Value::Object(Some(src_map)), key, new_val])?;
    }
}
```

The source-map reference is a **GC-scanned object field** (survives relocation; a Rust
side-table holding the `ObjectRef` would go stale). 2-field entries from other paths
(`Map.entry()`, TreeMap, immutable entries) have no slot 2 and keep the detached behaviour
(`object_num_fields() < 3` guard), so there is no regression.

Verified: `entrySet().iterator()...setValue()` writes through for HashMap and
MultivaluedHashMap; StripSecretsUtilsTest FAIL→PASS.
