# The synthetic slot floor is ONE number serving TWO layouts, and six classes pay for it

**Status: open.** Supersedes
`docs/known-issues/perf/hashset-is-built-map-shaped-by-three-factories-20260911.md`,
which named one row of this and proposed a fix that the measurement below shows
cannot work.

**Measured on:** Windows 11, JDK 25 Temurin `25.0.3+9`, `dev@f2a60701b`,
`probes/CollectionShapeCause.java` (retained heap per empty instance, 20 000
instances, Generational) and `CRATONVM_DBG_LAYOUT=1`.

## 1. The cost, and the one mechanism behind all of it

`ClassStore::build_compact_layout` refuses a compact layout to any class with a
padded slot — correctly, since a padded slot has no descriptor and its oop-map
entry would be a guess. So the class falls back to the legacy uniform 16-byte
tagged cell for EVERY slot. Padding by one costs the whole object.

`CRATONVM_DBG_LAYOUT=1` names every class it happens to:

```text
[layout] java/util/Properties       LEGACY (num_total_fields=24 declared=10 PADDED by 14)
[layout] java/util/HashSet          LEGACY (num_total_fields=3  declared=1  PADDED by 2)
[layout] java/util/LinkedHashSet    LEGACY (num_total_fields=3  declared=1  PADDED by 2)
[layout] java/util/ArrayDeque       LEGACY (num_total_fields=4  declared=3  PADDED by 1)
[layout] java/util/concurrent/CopyOnWriteArraySet   LEGACY (2 vs 1, PADDED by 1)
[layout] java/util/concurrent/ConcurrentLinkedQueue LEGACY (4 vs 2, PADDED by 2)
[layout] java/util/concurrent/ConcurrentLinkedDeque LEGACY (4 vs 2, PADDED by 2)
```

and what it costs, against HotSpot:

| class | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `Properties` | 120.3 | 544.0 | 4.5x |
| `ConcurrentLinkedDeque` | 48.0 | 120.0 | 2.5x |
| `CopyOnWriteArraySet` | 56.1 | 136.0 | 2.4x |
| `ConcurrentLinkedQueue` | 48.0 | 112.0 | 2.3x |
| `ArrayDeque` | 112.1 | 232.0 | 2.1x |
| `HashSet` | 64.0 | 128.0 | 2.0x |
| `LinkedHashSet` | 80.1 | 152.0 | 1.9x |

Every collection NOT on that list sits at 1.2x-1.7x, which is reference width
and a separate subject. These seven are the outliers, and they are outliers for
one reason.

## 2. Why none of them can simply be narrowed

The obvious fix — lower the arm in `synthetic_stub_fields` — was tried for
`HashSet` and reverted, and the reason generalises to all seven. **The floor is
a single number consulted in both modes, and the two modes want different
numbers.**

* In **synthetic-JDK** mode the stub IS the layout. `HashSet` really is
  map-shaped there (bucket array, size, capacity at slots 0/1/2), `ArrayDeque`
  really does need a fourth slot for `size` that the real class computes instead
  of storing, and `ConcurrentLinkedQueue`/`Deque` really are served by
  `native_lbq_*` over a four-slot array layout.
* In **real-JDK** mode none of that is reachable. The natives resolve by NAME
  (`hs_map_slot`, `al_slots_resolved`, `receiver_table_slot`), or they are
  shadowed out entirely by the receiver-has-its-own-bytecode rule —
  `ConcurrentLinkedQueue` is the clean demonstration: its registrations exist,
  and a fresh one nonetheless has `head == tail == new Node<>()` built by real
  JDK bytecode.

`wildfly_security.rs::count_carrying_hash_set` already writes the mode-aware
shape by hand:

```rust
if ctx.is_class_synthetic_stub("java/util/HashSet") {
    // the 3-slot shape IS the layout here
} else {
    crate::build_real_layout_string_hashset(ctx, &[])
}
```

The floor has no equivalent. That is the gap.

## 3. The fix, and the one thing that makes it non-trivial

**Apply the floor only where the layout it describes is the layout in use** —
i.e. to fabricated stubs, not to classes defined from real bytes.

The counter-example the original record gives is the reason this is not a
one-line change: `java.net.InetSocketAddress` declares one instance field
(`holder`) and its native `<init>` writes raw synthetic indices on the REAL
class. Classes in that position need the floor in real-JDK mode and would break
silently without it — an out-of-range `set_field` is dropped, not raised.

So the change is per-class, and the screen is mechanical: a class needs the
real-mode floor if and only if some native writes a raw absolute slot on a
receiver of that class in real-JDK mode. `t9c_synthetic_field_tables_cover_their
_factories` already scans for the literal-factory half of that population; the
other half is raw `ctx.set_field(obj, N, ..)` on a class the caller did not
allocate, which is the same grep.

Suggested order:
1. Add the screen as a gate (or extend T9C), so the population is a list rather
   than an argument.
2. Make the floor mode-aware for the classes the screen clears, starting with
   the seven above.
3. Re-run BOTH arms per class — real-JDK `CollectionSlotFloor`, and a
   `--features synthetic-jdk` binary against an unchanged-tree build of the same
   commit. Verdict-neutral in synthetic mode is the criterion.

Expected: all seven drop to the 1.2x-1.7x band the rest of the collections are
already in — `Properties` 544 -> ~190, `HashSet` 128 -> 88, `ArrayDeque` 232 ->
~90, the three concurrent ones to ~50-60.

## 4. The `HashSet` factories, which are a real defect either way

Three factories built a HashSet by writing absolute slots 0/1/2 with a bucket
array, a size and a capacity — the MAP layout, on a class whose one real field
is `map` (`Ljava/util/HashMap;`). `Collections.singleton` additionally wrote its
element straight into bucket 0 with no hashing, so the set was findable only by
an implementation that agreed to look there.

All three now build through the real `HashSet.<init>` (and `add`), which works
in both modes — `native_hs_init` allocates a proper backing map and
`hs_set_backing_map` resolves `map` by name with the synthetic slot 0 as its
fallback. Fixed in the same change as the `ConcurrentHashMap` work; it does NOT
by itself let the floor move, for the §2 reason.

`phases_late/text_intl.rs` (`ResourceBundle.keySet`) and
`phases_late/reflect_invoke.rs::build_string_set` went the same way in that
change -- five factories in total.

**Six remain, and they are what still pins `HashSet`'s floor.** Each builds an
array-backed set by writing raw absolute slots on a real receiver, so the screen
in §3 does not clear the class until they are converted:

```text
  native-builtins/src/jmx.rs:7042                        slots 0,1
  native-builtins/src/phases_late/net_channels.rs:623    slots 0,1  (Selector.selectedKeys)
  native-builtins/src/phases_late/net_channels.rs:642    slots 0,1  (Selector.keys)
  native-builtins/src/phases_late/reflect_invoke.rs:3342 slots 0,1,2
  native-builtins/src/util_time.rs:4922                  slots 0,1  (available zone ids)
  native-builtins/src/servlet.rs:8929                    slots 0,1
```

The tool for all six already exists and does not need writing:
`native-builtins/src/lib.rs::build_real_layout_string_hashset` builds the real
single-`map` shape by NAME, sizing the object with
`(s_map + 1).max(class_num_total_fields)`, and falls back to the legacy
two-field synthetic layout only when the real one cannot be resolved -- i.e. it
is already mode-aware. `wildfly_security.rs::count_carrying_hash_set` is the
model call site.

**Two floor sites, and both are real-mode-only.** That is what makes the §3 fix
surgical rather than dangerous: `class_manager.rs` applies the floor at ~6296
(`define_class_with_options`) and ~10589 (the stub-upgrade path), and BOTH run
only when a class is defined from real class-file bytes. A fabricated stub gets
its fields straight from `synthetic_stub_fields` at ~9776 and never consults
either. So an allowlist that skips the floor for a screened class changes
synthetic-JDK mode by nothing at all, and the arm that has to be re-run is the
real-JDK one plus a verdict-neutrality check.

## 5. Repro

```bash
javac -d out probes/CollectionShapeCause.java
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause
CRATONVM_DBG_LAYOUT=1 cratonvm --Xmx 2g -c out CollectionShapeCause 100 | grep PADDED
```
