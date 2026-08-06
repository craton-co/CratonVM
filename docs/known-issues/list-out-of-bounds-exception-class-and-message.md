# `List` out-of-bounds: exception class and message

**Status:** PARTIALLY FIXED 2026-08-05. Four rows remain, all of them the
`List.of(a)` / `List.of(a,b)` shape, and the reason they remain is stated below
rather than hand-waved.

**Reproducer:** `probes/ListOutOfBoundsProbe`.

## HotSpot is not uniform, and that is the whole difficulty

The obvious statement of this bug — "`List.get` out of range should throw
`IndexOutOfBoundsException`" — is **false**. Measured against a HotSpot 25
control, the JDK uses four different wordings and two different classes,
depending on which implementation the list is:

| receiver | class | message |
|---|---|---|
| `List.of("a")`, `List.of("a","b")` (`ImmutableCollections.List12`) | `IndexOutOfBoundsException` | `Index: 3 Size: 1` |
| `List.of()` and `List.of(a,b,c,d)` (`ListN` / empty) | **`ArrayIndexOutOfBoundsException`** | `Index 4 out of bounds for length 4` |
| `Arrays.asList(...)` | **`ArrayIndexOutOfBoundsException`** | `Index 5 out of bounds for length 2` |
| `ArrayList.get/set/remove` | `IndexOutOfBoundsException` | `Index 5 out of bounds for length 2` |
| `ArrayList.add(int, E)` | `IndexOutOfBoundsException` | `Index: 9, Size: 2` |
| `ArrayList.subList(...).get` | `IndexOutOfBoundsException` | `Index 1 out of bounds for length 1` |

`ListN` and `Arrays$ArrayList` index their backing array directly, so the array
access itself throws; `List12` and `ArrayList` bounds-check first. Note that
`add` uses `rangeCheckForAdd`, whose text is different again — comma, and
"Size" rather than "length".

CratonVM threw `ArrayIndexOutOfBoundsException` with a **null message** for
every one of these.

## Fixed

The receivers where HotSpot throws the plain class now do, with HotSpot's exact
message: `ArrayList.get` / `set` / `remove(int)`, `ArrayList.add(int, E)` (with
its distinct wording), and `ArrayList.subList(...).get` / `set`.

Class-correctness across the probe went from **11 rows wrong to 4**, with **zero
newly wrong** — checked as a set comparison against the pre-change binary, not
as a count, because the first attempt at this fix *did* regress three rows and
only that check caught it.

## Not fixed, and why

`List.of("a").get(3)` still throws `ArrayIndexOutOfBoundsException` where
HotSpot throws the plain class.

The JDK splits on `ImmutableCollections.List12` (1-2 elements) versus `ListN`
(0 or 3+). Two of those three cases want the *subclass*, which is what we
already throw. Getting the third right means telling a 1-2 element `List.of`
from `Collections.unmodifiableList(...)` wrapping a 1-2 element `ArrayList` —
which wants the plain class — and **this VM funnels every unmodifiable and
immutable list through one synthetic class** (`cratonvm/internal/UnmodifiableList`),
so that discriminator does not exist at the throw site.

The available options were: reproduce the split by guessing from size alone
(wrong for `Collections.unmodifiableList`), or hold this path where it is. The
second is right for 0 and 3+ elements and unchanged for the rest, so that is
what `native_unmod_get` does, with the reasoning recorded at the site.

Closing the remaining four needs the synthetic list class to carry its
provenance — which is a modelling change, not an exception change, and belongs
with whoever owns `cratonvm/internal/UnmodifiableList`.

## Also still open

`Arrays.asList(...)` and `List.of()` / `List.of(a,b,c,d)` have the right class
but a **null message** where HotSpot has `"Index 5 out of bounds for length 2"`.
That is not a `List` defect: `RuntimeError::ArrayIndexOutOfBoundsException`
carries an index and the Java-exception mapping discards it, so **every**
`ArrayIndexOutOfBoundsException` this VM throws has a null message, including
plain array accesses. Fixing it is the same shape as the
`StringIndexOutOfBoundsException` message work of 2026-08-05, but across ~202
construction sites, and it is VM-wide rather than collection-specific.
