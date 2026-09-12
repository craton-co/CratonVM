# `TreeMap`'s declared reference fields were never written — FIXED 2026-09-12

**Status: FIXED.** The last row of the descriptor-coercion census left open by
`docs/internal/fixed-bugs/the-throwable-slot-floor-compounded-with-hierarchy-depth-FIXED-20260912.md`
§5, which closed the `java.util.Properties.defaults` one and named this as a
separate instance of the same gap.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/treemap-reference-defaults-20260912` off `dev@9e7f2ed4d`, in both
real-JDK and `--synthetic-jdk` modes.

## 1. What the census said, and what it actually meant

```text
[cratonvm] descriptor-coercion census: total=1 primitive-into-reference[read=1]
  hottest=primitive-into-reference/read class_id=132 index=0 descriptor=L hits=1
```

`CRATONVM_DBG_LAYOUT=1` resolves `class_id=124` to
`java/util/TreeMap cid=124 body=64 refs=7 fields=9`.

The obvious reading of "index 0" is `comparator`, because that is the first
field `javap -p java.util.TreeMap` prints. That reading is WRONG, and the
`fields=9` is the tell: `TreeMap` declares seven, and the other two are
INHERITED from `java.util.AbstractMap`, which declares

```java
transient Set<K> keySet;
transient Collection<V> values;
```

A superclass's fields precede the subclass's, so the layout is
`keySet`, `values`, then `comparator`, `root`, `size`, `modCount`, `entrySet`,
`navigableKeySet`, `descendingMap`. **Index 0 is `keySet`.**

`CRATONVM_DBG=coercion` names the reader exactly:

```text
 8: cratonvm_native_collections::cached_tm_view      native-collections\src\lib.rs:55728
 9: cratonvm_native_collections::native_tm_key_set   native-collections\src\lib.rs:55762
10: cratonvm_native_collections::native_tm_descending_key_set
```

`cached_tm_view` resolves the view field by NAME and expects `Object(None)` to
mean "no view cached yet".

## 2. Why nothing ever wrote it

This family keeps its state in `tm_array_table`, a Rust side-table keyed on the
object; `tm_set_slot` writes a real field only through its two explicit mirrors
(`size` and `comparator`). So no declared reference field is assigned by the
native surface — and the real constructor that would have left them null the
way HotSpot does never runs either, because `TreeMap.<init>()V` and its three
siblings are all registered natives.

`Value::Object` carries a `NonNull` niche, so the all-zero cell `alloc_zeroed`
leaves decodes as `Value::Int(0)` and NOT as `Value::Object(None)`. A read of
one of those slots hands `coerce_field_value_for_slot` a primitive in a slot
declared `L`, and it DESTROYS the value.

It coerces to null, which is the right answer for a map with no view cached —
which is exactly why it had to be written rather than tolerated. The census
exists to find the reads where the coerced answer is NOT right, and a benign
row sitting at a fixed locator is how a real one stays hidden.

There is a second-order reason too. `tm_publish_real_root` decides the
BTreeMap fast path with

```rust
.map(|i| !matches!(ctx.get_field(this, i), Value::Object(None)))
```

on `comparator` — an `Int(0)` there reads as "has a custom comparator" and
takes a natural-ordering map off the fast path. It is the coercion, not the
code, that keeps that correct today.

## 3. The fix

`init_tm_reference_defaults` writes the JVMS §2.3 default for the six declared
reference fields nothing else touches — `keySet`, `values` (AbstractMap's) and
`root`, `entrySet`, `navigableKeySet`, `descendingMap` (TreeMap's own).
`comparator` is deliberately absent: `tm_set_slot`'s mirror already writes it
on every path into the constructors, including the null one.

Resolved against the RECEIVER's own class and bounds-checked, so a fabricated
stub declaring none of those names is a no-op rather than a write to whatever
those slots mean there — the same discipline as `system_properties_object`'s
`set_field_by_name`. Not a GC point.

It is called from four places, which is every way a `TreeMap` is produced
without running a real constructor:

| site | why |
|---|---|
| `native_tm_init` | `TreeMap.<init>()V` |
| `native_tm_init_comparator` | `TreeMap.<init>(Comparator)V` |
| `tm_new_range_view` | `descendingMap()` / `subMap()` / `headMap()` / `tailMap()` |
| `ts_publish_real_backing_map` | a `TreeSet`'s mirrored backing map |

`native_tm_init_from_map` and `native_tm_init_from_sorted_map` need nothing of
their own: each delegates to one of the first two.

The constructors alone were NOT enough, and the first build proved it — the row
survived unchanged. The receiver in the backtrace is a *view*: `descendingKeySet()`
mints one in `tm_new_range_view` and hands it to `native_tm_key_set`, and a view
never runs a constructor at all.

## 4. The measurement

Real-JDK mode, `probes/CollectionSlotFloor.java`, 20 000 instances, baseline
binary built from the same commit:

```text
  before:  descriptor-coercion census: total=1 ... class_id=124 index=0 descriptor=L hits=1
  after:   (no census line at all)
```

The probe's own output — every verdict and every retained-heap row — is
byte-identical before and after (`diff` reports no differences), so the write
costs nothing observable and changes no answer.

`--synthetic-jdk` mode was measured as a true A/B: one binary, the helper
gated behind a temporary env switch removed before the commit, so both arms are
the same build.

```text
              failures   field index OOB   census total
  helper OFF         6               165             34
  helper ON          6               165             33
```

Exactly one read removed, verdicts identical, warning count identical. The
remaining 33 are led by a 28-hit row at `class_id=132 index=0 descriptor=[` —
`java/util/Properties` slot 0, which is `Hashtable.table`, an ARRAY slot and a
different class. It is present with the helper OFF, so it is pre-existing and
unrelated; it is the synthetic-mode residual of the `Hashtable.table` null that
dev landed on 2026-09-12 (see `vm/tests/properties_backing.rs`). Left open here
deliberately rather than folded into a TreeMap change.

## 5. Gates run

`cratonvm-native-collections` full suite (147 + 12 further binaries, all ok),
`cratonvm-vm` `tier1_tests` 58 (incl. T9C/T9D), `properties_backing`,
`cow_array_set_backing`, `stub_ratchet` 14, and `regression-suite` 95/95.
