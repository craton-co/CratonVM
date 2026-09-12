# The throwable slot floor COMPOUNDED with hierarchy depth — FIXED 2026-09-12

**Status: FIXED.** The successor to
`docs/internal/fixed-bugs/the-synthetic-slot-floor-is-one-number-for-two-layouts-FIXED-20260912.md`,
which closed the same cliff for seven COLLECTION classes. This one is the other
92 padded classes that document's §6 left open, and the reason they were padded
turns out to be different in kind: not one floor that was too high, but one
floor RE-DECLARED at every level of a hierarchy.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/exception-floor-coercion-20260912` off `dev@46d7b7211`, against a
baseline worktree built from that SAME commit, in both real-JDK and
`--synthetic-jdk` modes.

## 1. The mechanism

`synthetic_stub_fields(name)` declares a class's OWN fabricated instance
fields, appended after its parent's. The throwable arm named
`java/lang/Throwable` and twenty-two of its subclasses together and gave each
of them two. Because the count is per-level and OWN, naming a subclass does not
restate the parent's model — it ADDS to it, and the total compounds with depth:

```text
                                before   after   model
  java/lang/Throwable                2       3       3
  java/lang/Exception                4       3       3
  java/io/IOException                6       3       3
  java/io/FileNotFoundException      8       3       3
```

In real-JDK mode that sum is applied as a FLOOR over the real class, whose
`Throwable` already declares six. So `IOException` was floored to ten against a
real six and `FileNotFoundException` to twelve, and
`ClassStore::build_compact_layout` ends `if padded { return None }` — a padded
slot has no descriptor, so its oop-map entry would be a guess. One pad costs
the WHOLE class its compact layout, so every exception object this VM allocated
sat on the legacy uniform 16-byte tagged `Value` cell.

Three is also the right number, not two: `synthetic_throwable_slot` maps
`detailMessage` = 0, `cause` = 1, `suppressedExceptions` = 2, so slot 2 fell
outside a bare synthetic `Throwable` and `write_throwable_field`'s
`slot < object_num_fields(this)` guard dropped every `addSuppressed` on one.

## 2. The fix

`java/lang/Throwable` declares the three. Every subclass INHERITS them: the
catch-all arm asks the new `jdk_chain_reaches_throwable(name)` and hands out
ZERO own fields when the fabricated parent chain already carries the model.

A name `jdk_superclass` does NOT know still gets the three, because it gets a
blanket `java/lang/Object` parent — an application `…/DbException` is that case,
and zero own fields there would mean a stub with no slots at all, whose every
throwable write is silently dropped.

Which makes a MISSING `jdk_superclass` arm newly expensive, and four were
missing. They were invisible while every level re-declared the model:

| class | real parent | was |
|---|---|---|
| `java/lang/InterruptedException` | `java/lang/Exception` | no arm |
| `java/lang/CloneNotSupportedException` | `java/lang/Exception` | no arm |
| `java/lang/InstantiationError` | `java/lang/IncompatibleClassChangeError` | no arm |
| `java/lang/InstantiationException` | `java/lang/ReflectiveOperationException` | no arm |

and one was misspelled: `ReflectiveOperationException` lives in `java.lang`,
not `java.lang.reflect`, so the only arm for it matched nothing and
`InvocationTargetException`'s chain — which points at the misspelled name —
stopped at `java/lang/Object`.

## 3. The measurement

`probes/ThrowableSlotFloor.java`, retained heap per instance, 20 000 instances.
HotSpot is the same probe under `java -Xmx3g`.

| class | HotSpot | before | after |
|---|---|---|---|
| `java.lang.Throwable` | 743.8 | 112.0 | **112.0** |
| `java.lang.Exception` | 741.7 | 144.0 | **112.0** |
| `java.lang.RuntimeException` | 750.2 | 176.0 | **112.0** |
| `java.lang.IllegalStateException` | 745.9 | 208.0 | **112.0** |
| `java.io.IOException` | 747.4 | 176.0 | **112.0** |
| `java.io.FileNotFoundException` | 743.2 | 208.0 | **112.0** |
| `java.io.EOFException` | 743.5 | 224.0 | **112.0** |

The absolute HotSpot number is not the target and not comparable — HotSpot
retains a real `backtrace` per throwable and this VM does not, which is a
different subject. The SHAPE is the whole point: HotSpot is FLAT across depth
(743.8 at the root, 743.5 six levels down), and before this change CratonVM
climbed 112 -> 224 over those same six levels. It is flat now, at exactly what
a bare `Throwable` costs, because every subclass has stopped paying for a
second and third copy of its parent's model.

Padded classes over that probe's whole run:

```text
                        before   after
  padded, all classes       91      49
  padded, throwable-like    43       1
```

The one that remains is `java/lang/reflect/InvocationTargetException`
(`num_total_fields=13 declared_instance_fields=7 PADDED by 6`), left
deliberately: it carries `target` on top of the model and has natives that
index it by absolute slot, so it is a single special case with its own reasons
rather than an instance of this bug. It is the only exception to
`throwable_like_inherits_its_chain`'s zero-own-fields rule, and that test says
so where it exempts it.

## 4. The gate

`class_manager::tests::throwable_like_inherits_its_chain` asserts, in two
layers:

* a hand-written list of ~35 names — the regression cases, each of which says
  which arm was added for which measurement, so DELETING an arm names the class
  it broke;
* and EXHAUSTIVELY over `BOOTSTRAP_EXCEPTION_CLASSES`, hoisted out of
  `bootstrap_core_classes` for this purpose: every throwable the VM pre-loads
  must have a `jdk_superclass` chain that reaches `java/lang/Throwable`.

The exhaustive half is the one that matters, and it is what the hand list could
not do: it catches a name ADDED to the bootstrap list with no arm at all, which
is exactly how the four above sat at `PADDED by 3` with nothing to report them.
It found `InterruptedException` on its first run.

Lowering a floor fails SILENTLY — an out-of-range `set_field` is dropped, not
raised — so `probes/ThrowableSlotFloor.java` reads back, through the public
surface, every slot the model names at seven depths: messages, causes
(constructor, `initCause`, two-deep chains, the double-`initCause` refusal),
suppressed exceptions (including try-with-resources), `catch` matching, and
`toString`. A lost write shows up there as a null message or a missing
suppressed exception. It PASSES in both modes.

## 5. Two other things this touched

**`Properties.defaults` was never written.** `system_properties_object` is the
one `Properties` in the image that never runs a constructor, and neither the
factory nor `alloc_object` writes the JVMS §2.3 reference default.
`Value::Object` carries a `NonNull` niche, so the all-zero cell `alloc_zeroed`
leaves decodes as `Value::Int(0)` and NOT as `Value::Object(None)`. Measured on
`probes/CollectionSlotFloor`:

```text
  before:  descriptor-coercion census: total=31
             class_id=132 index=8 descriptor=L hits=30
  after:   descriptor-coercion census: total=1
```

Thirty reads of a reference slot holding a primitive, each DESTROYED by
`coerce_field_value_for_slot`, on a probe that does nothing but read
properties. The coerced answer was `null`, which is the RIGHT answer, so
nothing observable was ever wrong — and that is precisely why it had to be
written rather than tolerated. The census exists to find the reads where the
coerced answer is not the right one, and thirty benign rows at one locator is
how a real one stays hidden. The one residual row is `java/util/TreeMap` slot 0
(`comparator`), a separate instance of the same gap, untouched here.

**`Properties.stringPropertyNames()` mutated a live view.** It took
`native_map_key_set(this)` — a CACHED LIVE VIEW, the same instance `keySet()`
hands out — and added the defaults' keys to it. That merged the chain's keys
into a `keySet()` that must not walk the chain, and mutated a set the caller
may already hold. In `--synthetic-jdk` mode it did not get that far:
`native_hs_add` on a view carrier raises `UnsupportedOperationException`, so the
method threw outright on any `Properties` with a defaults chain. It now builds
one fresh set from the union. `CollectionSlotFloor` in synthetic mode goes 8
failures -> 6, and the two that cleared are exactly `Properties
stringPropertyNames spans the chain` and `... sees the inherited key`. The other
six are pre-existing and unrelated (`IdentityHashMap`,
`ConcurrentLinkedQueue`, a `LinkedBlockingQueue` view, `EnumMap`); the 165
`field index OOB` warnings are identical before and after.

That rewrite also exposed a latent GC hazard in `make_hashset_with_elements`,
which every caller shares: it pinned the caller's `elems` AFTER three
allocations (`alloc_ref_array`, `alloc_object`, `try_alloc_synthetic`, and in
the fallback branch one `native_map_put` per element), each of them a GC point.
A caller holding bare `ObjectRef`s gathered out of a live collection — which is
what walking a `defaults` chain into a `Vec` produces — could have every one of
them left stale before anything pinned it. The pin is hoisted above the first
allocation and its base is now the truncation point for both branches, guarded
with `.min()` because `pin_value_slice` answers `usize::MAX` for a slice with no
object elements.

## 6. What was investigated and NOT shipped

`ArrayDeque.iterator()` was diagnosed as reaching `native_al_iterator` through
the `java/util/Collection.iterator()` interface native in synthetic mode, where
`al_state` would read the ring buffer as `elementData` and `head` as `size`. A
snapshot-and-wrap branch was written for it, and then **removed**, because the
defect does not reproduce: `CRATONVM_DBG_LAYOUT=1` shows
`java/util/ArrayDeque$DeqIterator` minted in BOTH modes, so `ArrayDeque` runs
its own real `iterator()` bytecode and never reaches the interface native at
all. `--synthetic-jdk` implies `--real-jdk` and requires a real runtime image,
so there is no supported configuration in which that branch is live.
`probes/ArrayDequeIter.java` covers the case — `addFirst` and `addLast`,
`size`, `toString` and an explicit `iterator()` walk — and PASSES on all four
binaries (baseline and new, both modes), matching HotSpot.

Shipping it would have been a dead branch in a shared native carrying a long
comment asserting a mechanism that does not occur. The probe is kept; if the
trace is ever reproduced, it is the thing that will show it.
