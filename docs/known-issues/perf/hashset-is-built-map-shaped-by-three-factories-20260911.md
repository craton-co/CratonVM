# `HashSet` is built MAP-shaped by three factories, which pins its slot floor

**Status: open.** Split out of
`docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md`
§6, which closed every other row of that page and could not close this one.

**Cost, measured** (`probes/CollectionShapeCause.java`, retained heap per empty
instance, Temurin 25.0.3+9, Windows, Generational):

```text
                    HotSpot   CratonVM
  java.util.HashSet    64.0      128.0      = 64 (the set) + 64 (its backing map)
```

The backing map is already right (64, and `HashMap` is not padded). The set
object is 64 bytes for **one reference field**, which needs 24.

## Why it is 64 and not 24

`java.util.HashSet` declares exactly one instance field, `map`. Its entry in
`class_manager.rs::synthetic_stub_fields` declares THREE, so the class is padded
by two — and `ClassStore::build_compact_layout` refuses a compact layout to any
padded class, because a padded slot carries no descriptor and its oop-map entry
would be a guess. The whole object falls back to the legacy uniform 16-byte
tagged cell: 3 slots x 16 + a 16-byte header.

```text
[layout] java/util/HashSet cid=65 LEGACY, no compact layout
         (num_total_fields=3 declared_instance_fields=1 PADDED by 2)
[layout] java/util/LinkedHashSet cid=129 LEGACY, no compact layout
         (num_total_fields=3 declared_instance_fields=1 PADDED by 2)
```

`LinkedHashSet` declares no instance fields of its own and inherits this floor,
so it is padded for the same reason and would move with it.

## Why the floor cannot simply be lowered

It was lowered to 1 during the parent work, on the strength of `HS_FIELD_MAP =
0` being the only index `native-collections`' `native_hs_*` surface uses — and
that surface resolves by NAME first (`hs_map_slot`) whenever the real class is
present. Both true, and both beside the point.

`t9c_synthetic_field_tables_cover_their_factories` is what caught it. Three
factories build a HashSet by writing absolute slots 0/1/2 directly:

| site | what it writes |
|---|---|
| `native-builtins/src/phases_late/collections.rs` — `Collections.singleton` | slot 0 = `Object[16]` bucket array, 1 = `Int(size)`, 2 = `Int(capacity)` |
| `native-builtins/src/phases_late/collections.rs` — `WeakHashMap.keySet` | the same three, empty |
| `native-builtins/src/phases_late/charset_buffers.rs` | the same three |

That is the MAP layout (`MAP_FIELD_BUCKETS/SIZE/CAPACITY`) on a class whose one
real field is `map`. So the floor is load-bearing — for a shape that is itself
wrong.

## What it is wrong about, and why that matters beyond the bytes

On a REAL `java.util.HashSet` slot 0 is `HashSet.map`, declared
`Ljava/util/HashMap;`. These three factories put an `Object[]` there. Every
reader that resolves `map` by name — `hs_map_slot`, and any real JDK bytecode
that survives to run — gets a bucket array where a `HashMap` belongs.

It is the same shape as the defect `publish_map_table` already fixed on the map
side, recorded in `native-collections/src/lib.rs`: a VM-built view left
`keySet = ARRAY[Object]` where HotSpot has `null`, and it "does not fault today
only because the natives shadow every reader". The same sentence applies here,
and the same thing retires it: make the object real.

## The fix, in the order it has to happen

1. **Retarget the three factories.** Build the set through the real
   `HashSet.<init>` (as `spring_startup_bootstrap`'s fallbacks already do), or
   set `map` by name to a real backing `HashMap`. Either makes the object's one
   slot mean what it is declared to mean.
2. **Then lower the floor to 1**, and re-run BOTH arms — the real-JDK
   `CollectionSlotFloor` and the `--features synthetic-jdk` binary against an
   unchanged-tree build of the same commit. A floor lowered too far fails
   silently: an out-of-range `set_field` is dropped, not raised.

Expected after: the set object 64 -> 24, so `HashSet` 128 -> 88 and
`LinkedHashSet` likewise, which is the ratio every other collection on that page
now sits at (reference width, and its own subject).

## Repro

```bash
javac -d out probes/CollectionShapeCause.java
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause
CRATONVM_DBG_LAYOUT=1 cratonvm --Xmx 2g -c out CollectionShapeCause 100 \
  | grep 'java/util/HashSet'
cargo test --release -p cratonvm-vm --test tier1_tests \
  t9c_synthetic_field_tables_cover_their_factories
```
