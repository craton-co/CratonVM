# `java.lang.reflect.Array`'s primitive accessors do not type-check

**Status:** FIXED 2026-08-06. `probes/ReflectArrayContractProbe` is identical to
a HotSpot 25 control on all **341** rows; it was 212 wrong.

**Reproducer:** `probes/ReflectArrayContractProbe` — the full 11-array ×
9-accessor matrix, both setter matrices, the check-precedence cases and the
`newInstance` edge cases.

## What it was

`register_reflect_array_natives` mapped 18 `Array.*` methods onto **ten**
implementations, and the primitive accessors collapsed onto two of them:
`getInt`, `getBoolean`, `getByte`, `getShort` and `getChar` were all
`native_array_get_int`, which returned the raw element whatever the array held.
The setters collapsed the same way. So the *requested* type — the one piece of
information the whole contract turns on — never reached the call site.

The visible cost was not a wrong exception but a **fabricated value**:

```java
Array.getInt(new long[4], 0)     // returned 0; HotSpot: IllegalArgumentException
Array.getInt(new Object[4], 0)   // returned 0; HotSpot: IllegalArgumentException
Array.getLong(new int[]{7}, 0)   // returned 0; HotSpot: 7L
```

The first narrower probe (`PreconditionsFormatterProbe`, while closing the
AIOOBE record) showed **3** wrong rows. Measuring the actual matrix showed
**212**. Three cells of a 341-cell contract is not a sample.

## What HotSpot does

One widening table, read in both directions — `widens_to(from, to)`, JLS
§5.1.2. `Array.getX(a, i)` asks `widens_to(element_type, X)`; `Array.setX(a, i, v)`
asks `widens_to(X, element_type)`. Same relation, arguments swapped; getting the
direction wrong is invisible for the eight same-type cells and wrong for every
other one. `boolean` converts to and from nothing, and `char` is the asymmetry —
`char` widens to `int`, `short` does not widen to `char`, because neither range
contains the other.

Four distinct `IllegalArgumentException` wordings, plus a null-message case,
and they are not interchangeable:

| condition | class | message |
|---|---|---|
| receiver is null | `NullPointerException` | *(null)* |
| receiver is not an array | `IllegalArgumentException` | `Argument is not an array` |
| primitive accessor, reference array | `IllegalArgumentException` | `Argument is not an array of primitive type` |
| widening refused on a primitive array | `IllegalArgumentException` | `argument type mismatch` |
| `set(Object)` unassignable on a reference array | `IllegalArgumentException` | `array element type mismatch` |
| `set(primitiveArray, i, null)` | `IllegalArgumentException` | *(null)* |

**The check order is not uniform, and that is measurable.** "Not an array of
primitive type" runs *before* the bounds check; the widening check runs *after*
it:

```text
Array.getInt(new Object[4], 9)   IllegalArgumentException   (type wins)
Array.getInt(new long[4],   9)   ArrayIndexOutOfBounds...   (bounds wins)
Array.set(new int[4], 9, "x")    ArrayIndexOutOfBounds...   (bounds wins)
```

Three checks, three positions. `probes/ReflectArrayContractProbe`'s
"bounds vs type" rows pin each one, because a reordering changes the exception
*class* and therefore control flow.

## Fixed

Each accessor now carries the type it was asked for, bound at registration, and
they share one `widens_to` table plus one conversion routine. `Array.set(Object)`
unboxes with the same widening rule the typed setters use, so
`set(long[], Integer)` succeeds and `set(int[], Long)` does not.

Three `newInstance` argument checks were also missing — all three allocated and
returned an array where HotSpot throws:

```text
Array.newInstance(null, 1)               NullPointerException
Array.newInstance(void.class, 1)         IllegalArgumentException (null message)
Array.newInstance(int.class, new int[0]) IllegalArgumentException (null message)
```

`RuntimeError::IllegalArgumentException` gained the empty-message-means-null
convention `UnsupportedOperationException` already had, for the same reason: the
variant holds a `String`, so a site needing a null `getMessage()` could not say
so. No site in the workspace produces a deliberate empty IAE message, so the
marker is unambiguous.

## What this was split from

The *bounds* half of this same surface was fixed a day earlier, along with the
VM-wide AIOOBE detail message — `Array.get(new int[4], 9)` returned `0` and
`Array.set` dropped the write. See
`array-index-out-of-bounds-has-no-detail-message-FIXED-20260806.md` in this
tree. That record's closing section named these three rows as a separate
contract and filed them rather than absorbing them, which is why they had a
probe to grow rather than a symptom to rediscover.
