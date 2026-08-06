# `List` out-of-bounds: exception class and message

**Status:** FIXED 2026-08-06. The four `List.of(a)` / `List.of(a,b)` rows the
previous pass left open are closed, and so is the null-message half. The reason
they were left open was wrong, not merely incomplete — see below.

**Reproducer:** `probes/ListOutOfBoundsProbe`.

## HotSpot is not uniform, and that is the whole difficulty

The obvious statement of this bug — "`List.get` out of range should throw
`IndexOutOfBoundsException`" — is **false**. Measured against a HotSpot 25
control, the JDK uses several wordings and two classes, depending on which
implementation the list is:

| receiver | class | message |
|---|---|---|
| `List.of("a")`, `List.of("a","b")` (`ImmutableCollections.List12`) | `IndexOutOfBoundsException` | `Index: 3 Size: 1` |
| `List.of()` and `List.of(a,b,c,d)` (`ListN` / empty) | **`ArrayIndexOutOfBoundsException`** | `Index 4 out of bounds for length 4` |
| `Arrays.asList(...)` | **`ArrayIndexOutOfBoundsException`** | `Index 5 out of bounds for length 2` |
| `ArrayList.get/set/remove` | `IndexOutOfBoundsException` | `Index 5 out of bounds for length 2` |
| `ArrayList.add(int, E)` | `IndexOutOfBoundsException` | `Index: 9, Size: 2` |
| `ArrayList.subList(...).get` | `IndexOutOfBoundsException` | `Index 1 out of bounds for length 1` |
| `LinkedList` (`get`/`set`/`add`/`remove`) | `IndexOutOfBoundsException` | `Index: 5, Size: 2` |
| `Collections.singletonList("a").get(1)` | `IndexOutOfBoundsException` | `Index: 1, Size: 1` |
| `Collections.emptyList().get(0)` | `IndexOutOfBoundsException` | `Index: 0` — no size at all |
| `Collections.unmodifiableList(x)` | *whatever `x` throws* | *whatever `x` says* |
| `ArrayList.subList(0, 5)` | `IndexOutOfBoundsException` | `toIndex = 5` |

`ListN` and `Arrays$ArrayList` index their backing array directly, so the array
access itself throws; `List12`, `ArrayList` and `LinkedList` bounds-check first.
Note the three near-identical `Index:` forms — `List12` uses a **space**,
`ArrayList.add` and `LinkedList` use a **comma**, and `EmptyList` names no size.
Those are not typos to normalise.

`Collections.unmodifiable*` is a *view*: it delegates, so its answer is its
backing's. That is why the same wrapper gives three different results over an
`ArrayList`, an `Arrays$ArrayList` and a `LinkedList`.

## What the previous pass got wrong

It recorded that the remaining four rows could not be fixed:

> this VM funnels every unmodifiable and immutable list through one synthetic
> class (`cratonvm/internal/UnmodifiableList`), so that discriminator does not
> exist at the throw site

The **class** is shared; the **receiver** is not. Slot 1 of every wrapper is
`UNMOD_FIELD_IMMUTABLE`, stamped by `freeze_result` for the `*.of`/`copyOf`
factories and left clear by `Collections.unmodifiable*`. The VM was already
making precisely this split, from precisely these two inputs (marker + size),
one crate over: `getclass_immutable_marker` in `native-builtins/src/lib.rs`
reports `ImmutableCollections$List12` versus `$ListN` for `getClass()`.

So the discriminator was two lines from the throw site the whole time. The
options were never "guess from size alone" versus "hold the line" — reading the
marker was available and is what `unmod_list_oob_error` now does.

This is worth stating plainly because the doc's reasoning read as settled, and
a settled reason not to fix something is exactly what stops the next person
from looking.

## Fixed

* **`List.of(a)` / `List.of(a,b)`** — plain `IndexOutOfBoundsException`,
  `"Index: 3 Size: 1"`. The four rows this doc was left open for.
* **`Collections.unmodifiableList(x)`** — no longer pre-empts its backing. It
  used to answer for every backing with one class and one wording; it now stands
  aside, so an `ArrayList` backing answers with the plain class, an
  `Arrays$ArrayList` with the subclass, and a `LinkedList` with its own comma
  form — as HotSpot does.
* **`LinkedList`** positional `get`/`set`/`add(int,E)`/`remove(int)` — these
  threw the `ArrayIndexOutOfBoundsException` **subclass** for every index, which
  is the direction that breaks a `catch`. Now the plain class with
  `LinkedList.outOfBoundsMsg`'s wording.
* **`Arrays.asList(...).get`** — right class already, now the array-access
  wording rather than the index-only one.
* **`Collections.singletonList` / `emptyList`** — plain class, and `emptyList`'s
  message correctly names no size.
* **`subList(from, to)`** range check — `"fromIndex = -1"` / `"toIndex = 5"`
  instead of an index-only AIOOBE.
* **`listIterator(int)`** — plain class, with the space/comma split.
* **The null-message half** ("Also still open" in the previous revision) is
  closed by the VM-wide AIOOBE widening; see
  `array-index-out-of-bounds-has-no-detail-message-FIXED-20260806.md`.

`native-collections/tests/mock_arraylist.rs`'s
`get_out_of_bounds_throws_aioobe` had been **red on `dev` itself** since
`ArrayList.get` was corrected on 2026-08-05: it asserted the subclass HotSpot
does not throw. The test was the stale half; it is now
`get_out_of_bounds_throws_ioobe` and checks the message as well as the class,
because this family has already shipped a right-class/wrong-message state once.

## Not fixed, and named

`arrayList.subList(...).getClass()` reports
`cratonvm.internal.ArrayListSubList` where HotSpot says
`java.util.ArrayList$SubList`. That is the `getClass()` display map, not a
bounds contract — the same table `getclass_immutable_marker` feeds — and it is
unrelated to anything above.
