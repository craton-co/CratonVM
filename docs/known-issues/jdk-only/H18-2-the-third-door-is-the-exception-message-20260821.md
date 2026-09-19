# H18-2 — the third door is the exception message, and the fix that was written to close it was applied to one stamp of eleven

**Status: FIXED IN SOURCE, NOT COMPILED** (the leak). **OPEN** (the `entrySet`
display name, §3). Landed with `H18-1` at `a73aea08b`.

**Date** 2026-08-21
**Lane** H18
**Binary** `C:/craton/cratonvm-r5.exe` @ `9eef86699` — prebuilt, pre-patch
**Oracle** HotSpot 25.0.3+9

Claims are **MEASURED** (ran it) or **ARGUED** (read it).

---

## 1. `cce_display_class_name` is a one-of-eleven fix

`vm/src/runtime/interpreter/lambda.rs:284` exists, by its own doc comment, to
stop a VM-generated `ClassCastException` exposing the private stamp:

> *"`Object.getClass()` deliberately translates that stamp to the corresponding
> JDK implementation class, but a VM-generated `ClassCastException` previously
> exposed the private stamp instead. Besides being observably unlike HotSpot,
> that broke `LambdaSafe`: it identifies an erased-generic mismatch by comparing
> the exception prefix with `argument.getClass().getName()`."*

Its second statement is `if raw_name != "cratonvm/internal/UnmodifiableMap" {
return raw_name.to_string(); }`. **One of the eleven stamps
`vm_init.rs:1746-1820` registers.** The other ten fell through and printed the
private name.

**MEASURED**, Compatible mode, two runs, verbatim:

```
(AbstractMap) Map.of("k","v")
  CCE: class java.util.ImmutableCollections$Map1 cannot be cast to class java.util.AbstractMap
       (… both in module java.base of loader 'bootstrap')

(AbstractCollection) List.of(1,2,3)
  CCE: class cratonvm.internal.UnmodifiableList cannot be cast to class java.util.AbstractCollection
       (cratonvm.internal.UnmodifiableList is in unnamed module of loader 'bootstrap'; …)

(RandomAccess) List.of(1,2,3)
  CCE: class cratonvm.internal.UnmodifiableList cannot be cast to class java.util.RandomAccess …

(RandomAccess) Collections.unmodifiableList(new ArrayList<>(List.of(1)))
  CCE: class cratonvm.internal.UnmodifiableList cannot be cast to class java.util.RandomAccess …
```

HotSpot throws **none** of these four — every one of them is a cast that should
have succeeded, which is `H18-1`'s defect. But the two shapes of message are a
second defect underneath it, and they survive `H18-1`'s fix independently,
because a *genuinely* failing cast on one of these receivers still has to print
something:

```
NEGATIVE CONTROL, must throw on both VMs:
  (AbstractCollection) Collections.unmodifiableList(…)
  HotSpot : class java.util.Collections$UnmodifiableRandomAccessList cannot be cast to …
  CratonVM: class cratonvm.internal.UnmodifiableList cannot be cast to …
```

**The two failure modes are not the same size.** The map row prints a *plausible
false sentence* — `ImmutableCollections$Map1` genuinely is an `AbstractMap`, so
the message asserts something the JDK's own class file contradicts. The list row
prints a *private VM class name into application-visible text*, which is the
`LambdaSafe` breakage above, still live for every `List`, `Set`,
`SortedSet`, `NavigableSet` and `Collection` receiver on the day this was
measured.

### 1.1 What landed

`cce_display_class_name` now reads `unmod_stamp_display_name` — the same table
`op_instanceof` and `op_checkcast` consult for the **decision**. The
size-discrimination (`Map1` vs `MapN`, `List12` vs `ListN`, `Set12` vs `SetN`)
stays at this call site, because a message wants the size and a subtype answer
provably does not (`H18-1` §4). It is read from the backing collection's `size`
**field**, never through `invoke_virtual("size")`, so it remains safepoint-free
— which matters here for a reason specific to this caller: this runs while the
interpreter is *building an exception* from a receiver it holds as a bare local.

Behaviour for maps is preserved exactly, including both fallbacks (no backing
object → `MapN`; marker clear → `Collections$UnmodifiableMap`). For lists and
sets whose backing has no `size` field — an array-backed or foreign layout —
the `…N` form is kept, which is what HotSpot reports for everything except the
one- and two-element cases. **Stated so it is not read as exact:** that residue
is a known imprecision, not a closed cell.

## 2. Why this is the interesting half

`H0-2` and `H15-2` both frame the defect as *reflection vs opcode*. With the
message counted it is **three doors, and the two that are right are the two
nobody dispatches on**:

| door | reads | consumer |
|---|---|---|
| `Class.isInstance` | `getclass_display_class_id` (native-builtins) | reflection |
| `ClassCastException` text | `cce_display_class_name` (vm/lambda.rs) | error reporting |
| `instanceof` / `checkcast` | the raw stamp | **every branch in every program** |

The alias was implemented twice, for the two consumers that only ever *describe*
the object, and never for the one that *decides* on it. That is a sharper
statement of the `a-reflective-native-and-its-bytecode-opcode-are-twins-that-drift`
species than either prior record makes, and it is why `H18-1` routes all three
through one function rather than adding a third reader.

## 3. NEW, OPEN — `entrySet()`'s display class is wrong, and only in Compatible mode

`H0-2` N3 asks for the other `GetClassDisplay` arms to be audited and states
that `Collections$UnmodifiableSortedSet` and `…NavigableSet` "were never probed
at all". Probed now — 5 receivers × up to 4 type tests, plus `getClass()`.

**MEASURED.** The subtype cells are **all clean**: `unmodifiableSortedSet`,
`unmodifiableNavigableSet`, `unmodifiableCollection` and
`unmodifiableMap().keySet()` agree with HotSpot on `AbstractCollection`,
`AbstractSet`, `SortedSet`, `NavigableSet`, `Set` and `Collection`, in both
`instanceof` and `isInstance`. **N3's suspicion does not reproduce for those
four families**, and that bounds `H18-1`'s defect to the three the earlier
records named.

**One `getClass()` name does diverge, and it is not in any record:**

| | `Collections.unmodifiableMap(m).entrySet().getClass()` |
|---|---|
| HotSpot 25.0.3+9 | `java.util.Collections$UnmodifiableMap$UnmodifiableEntrySet` |
| CratonVM `--jdk-only` | `java.util.Collections$UnmodifiableMap$UnmodifiableEntrySet` |
| **CratonVM Compatible** | **`java.util.Collections$UnmodifiableSet`** |

The tree knows: `collection_display_kind` maps
`cratonvm/internal/UnmodifiableEntrySet` to `GetClassDisplay::CollSet` with a
comment admitting *"real JDK's `Collections$UnmodifiableMap$UnmodifiableEntrySet`
is a distinct inner class, but the existing keySet()/entrySet() display was
already unified under CollSet before this class existed."* **A comment
acknowledging a divergence is not a measurement of it, and this is the first
time the divergence has been put on a page.** No subtype cell moves — both
classes are non-`AbstractSet` wrappers over `UnmodifiableCollection` — so this
is a name defect only, and `H18-1`'s table deliberately mirrors it rather than
silently correcting one door out of three.

**Not fixed here**: the authority for a `getClass()` name is
`native-builtins/src/lib.rs`, outside this lane's ownership. The change is one
new `GetClassDisplay` variant plus one `collection_display_kind` row.

## 4. Three things found in passing, each cheap to close

### 4.1 The filed probe does not compile

`regression-suite/probes/OpcodeVsReflectionProbe.java` declares `public class
InstOf2`. **MEASURED:**

```
OpcodeVsReflectionProbe.java:2: error: class InstOf2 is public,
  should be declared in a file named InstOf2.java
```

It was landed at `b537c5623` as the artefact of `H15-2`'s independent
reproduction. Anyone who reaches for it to re-check that lane's table gets a
javac error, not a table. One-word fix; left alone because
`regression-suite/**` is outside this lane's ownership.

### 4.2 `getclass_backing_is_navigable_set` has no callers

`native-builtins/src/lib.rs:26320`. It is a **private** `fn`, so its own file is
the entire set of places it could be called from, and it occurs there exactly
once — its definition. Its doc comment says it is *"used to pick between
`Collections$UnmodifiableSortedSet` and the NavigableSet subclass
`Collections$UnmodifiableNavigableSet` for `getClass()` display"*, and
`getclass_display_class_id`'s `CollSortedSet` arm does no such thing: it returns
`UnmodifiableSortedSet` unconditionally. The discrimination the comment
describes is really performed by the **stamp** — `vm_init.rs` registers
`UnmodifiableSortedSet` and `UnmodifiableNavigableSet` as two separate classes —
which is why the probe became unnecessary and, apparently, why nobody noticed it
stopped being called. §3 measures that both arms are correct today, so this is
dead weight rather than a missing feature. **A doc comment describing behaviour
no caller can produce is the `a-no-op-native-whose-comment-explains-why`
shape.**

### 4.3 These stamps declare one field and allocate two

`vm_init.rs:1810` registers all eleven with
`ensure_bootstrap_compat_class(&mut class_manager, name, 1)`.
`native-collections`' `alloc_unmod_wrapper` allocates them with
`try_alloc_synthetic(ctx, class_name, 2)` and writes slot 1 as
`UNMOD_FIELD_IMMUTABLE`.

**The class says 1; the object has 2.** Anything that bounds-checks slot 1
against the *class's* `num_total_fields` — which is the shape
`proxy_instance_satisfies_target` uses a few lines away in the same file, with
the comment *"that is out of bounds and this helper is hot"* — would reject
every one of these receivers while the slot is really there. `H18-1`'s patch
bounds-checks against the **object header** (`heap.num_fields`) for exactly this
reason. Nothing is currently broken by the mismatch; it is a landmine with a
tripwire already installed nearby.

## 5. What I did NOT verify

* **None of this is compiled or run against a suite.** See `H18-1` §8.
* **§3's clean subtype cells are one receiver per family.** A `TreeSet`-backed
  sorted set and an `ArrayList`-backed collection; other backings were not
  built. `unmodifiableSortedMap`/`unmodifiableNavigableMap` were not probed at
  all — they have no stamp of their own in the eleven, which is itself worth
  someone's afternoon.
* **The `LambdaSafe` consumer named in §1 was not run.** That the leak breaks it
  is the tree's claim, quoted, not this lane's measurement. What is measured is
  the leak.
* **`Collections$ReverseComparator2`** — `H15-2`'s parting note about
  `Comparator.naturalOrder().reversed()` yielding a different class name than
  HotSpot on the same JDK — was **not** investigated here either. Still
  unclaimed by any lane.

## NOMINATIONS

**N1 — give `UnmodifiableEntrySet` its own display.** §3. One
`GetClassDisplay` variant and one `collection_display_kind` row in
`native-builtins`, plus the mirroring row in `unmod_stamp_display_name` so the
three doors stay in step. Small, and it closes a cell that is wrong in exactly
one of the three arms.

**N2 — rename `OpcodeVsReflectionProbe.java`'s class, or the file.** §4.1. It is
the reproduction artefact for `H15-2` and it cannot be run.

**N3 — delete `getclass_backing_is_navigable_set` or wire it up.** §4.2. Decide
which, and if deleting, delete the doc comment's claim with it — a future reader
otherwise concludes the sorted/navigable discrimination is field-driven when it
is stamp-driven.

**N4 — audit slot-1 readers of the eleven stamps for class-side bounds checks.**
§4.3. `grep` for `num_total_fields` near `UNMOD_` and near
`cratonvm/internal/Unmodifiable`. The mismatch is silent today and will not stay
silent.
