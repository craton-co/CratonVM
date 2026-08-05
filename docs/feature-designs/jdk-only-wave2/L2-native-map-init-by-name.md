# L2 — `native_map_init`'s raw `MAP_FIELD_*` branch → by-name

**Owns:** `native-collections/src/lib.rs` (whole file — 55k lines, one owner)
**Gated on:** nothing.
**Conflicts:** L10 (real `ThreadPoolExecutor` init) is in the same file. **Land
L2 first**; it is the smaller change.
**Effort:** M
**Evidence:** [`fabricated-object-layouts-leak-into-native-code.md`](../../known-issues/jdk-only/fabricated-object-layouts-leak-into-native-code.md)

## Goal

`native_map_init`'s legacy branch writes `MAP_FIELD_BUCKETS` / `MAP_FIELD_CAPACITY`
by raw absolute index. On a real JDK class those indices are different fields.
It is the surviving `Properties` slot-2 row (`Object` over an `int`, 3 hits) and
the whole `HashMap` family (~6,500 hits).

**Kind 2** — the real fields exist (`table`, `size`, `threshold`, `loadFactor`);
we compute their indices against the wrong layout. Fix shape already proven in
this file on 2026-08-04: `try_set_jdk_map_field` was resolving names against a
hard-coded `"java/util/HashMap"` and writing that index into any receiver;
switching it to `resolve_field_index_by_class_id(class_id_of_object(this), name)`
took three `Properties` rows to zero with every other row byte-identical.

## The part that needs care

The `HashMap` family is **documented benign** and must stay benign. Coercion of
our `Int` to `null` on a real `Map` subtype produces exactly the null-initialised
state real `Map` bytecode expects (S111r29), which is why the overlay hunter
suppresses it by default. Do not "fix" those 6,500 hits into something that
changes `Compatible` behaviour without measuring — the natives are the
authoritative implementation for `HashMap` and JDK bytecode does not run for it.

So the target is narrower than the hit count suggests: make the writes land on
the right fields *when the receiver is not one of ours*, and leave the
native-backed `HashMap` path alone.

## Steps

1. Read `native_map_init`'s two branches. `uses_native_hashtable_layout`
   (i.e. `CF_HASHTABLE_LAYOUT`) already excludes `Properties` deliberately — JDK
   25 backs it with a side `ConcurrentHashMap`. That exclusion is correct; the
   bug is in the *legacy* branch it falls into.
2. Convert `MAP_FIELD_BUCKETS` / `MAP_FIELD_CAPACITY` writes to by-name
   resolution on the receiver's class, keeping the raw write only when the
   receiver has our synthetic layout (predicate by **name**, never field count).
3. `native_props_init` writes `Value::Object(None)` to `PROPS_FIELD_DEFAULTS`,
   which is `loadFactor` on a real layout. **The hunter does not report this** —
   `overlay_write_is_destructive` ignores `Object(None)` over a primitive. Fix it
   even though it does not appear in the census, and see L4 for closing that
   blind spot.

## Verification

* A/B the census: `Properties` slot 2 goes 3 → 0. The `HashMap` rows may stay —
  if they change, prove the change is intended and that `Compatible` is
  unaffected.
* `cargo test --release -p cratonvm-native-collections --lib` (94 tests).
* Both probes vs HotSpot, both modes. Collections are exercised heavily by
  `JdkOnlyCensusLoadProbe`'s first section, so a regression here is loud.
* `Compatible` byte-for-byte: run the `test_classes` corpus under the pre-fix
  and post-fix binaries, normalise timestamps, diff.

## Done when

`Properties` slot 2 is gone, the `HashMap` family is unchanged or its change is
justified in the record, and the 94 collections tests pass.
