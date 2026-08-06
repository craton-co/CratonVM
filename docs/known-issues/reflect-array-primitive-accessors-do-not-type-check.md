# `java.lang.reflect.Array`'s primitive accessors do not type-check

**Status:** OPEN. Found 2026-08-06 while closing
`array-index-out-of-bounds-has-no-detail-message`; the *bounds* half of that
surface was fixed there, this is the *conversion* half and is a separate
contract.

**Reproducer:** `probes/PreconditionsFormatterProbe`, the
`Array domain: reflective access` rows.

```
                          HotSpot 25                                          CratonVM
Array.getInt(Object[4],0) IllegalArgumentException:                           NO-THROW, returns 0
                            Argument is not an array of primitive type
Array.getInt(long[4],0)   IllegalArgumentException: argument type mismatch    NO-THROW, returns 0
Array.set(int[4],0,"x")   IllegalArgumentException: argument type mismatch    IllegalArgumentException:
                                                                                Array.set value is not
                                                                                a boxed primitive
```

The first two are the ones that matter: they return a **fabricated value**
rather than throwing, so a caller reading `Array.getInt` off a `long[]` gets `0`
and no indication anything was wrong. The third has the right class and only the
wrong wording.

## Why it happens

`register_reflect_array_natives` registers 18 `Array.*` methods onto **ten**
implementations, and the primitive getters all share one:
`getInt`, `getBoolean`, `getByte`, `getShort` and `getChar` are every one of
them `native_array_get_int`, which returns the raw element whatever the array's
element type is. The setters collapse the same way onto
`native_array_set_int`.

So the *requested* type is not available at the call site — the native cannot
tell `getInt` from `getChar`, which is exactly the information HotSpot's
`Reflection::array_get` uses to decide between a widening conversion, an
`IllegalArgumentException`, and a successful read.

HotSpot's rule, per `java.lang.reflect.Array`:

* receiver not an array → `IllegalArgumentException: Argument is not an array`
  *(already correct here)*;
* receiver is a **reference** array and a primitive getter was called →
  `Argument is not an array of primitive type`;
* element type not widening-convertible to the requested type →
  `argument type mismatch`. Widening is allowed, so `Array.getLong(int[4], 0)`
  succeeds and returns `0L` — measured, and the probe pins it as a NO-THROW row
  so a fix cannot over-correct into rejecting it.

## What must change

Give each accessor its own registration carrying the requested primitive type,
and share one conversion routine that implements the widening matrix, rather
than five registrations pointing at one untyped body. The bounds check added in
2026-08-06 (`reflect_array_index`) is already shared this way and is unaffected.

## What was fixed and is not this

The same surface had a **missing bounds check** — `Array.get(new int[4], 9)`
returned `0` and `Array.set(new int[4], 9, v)` dropped the write — and
`Array.newInstance(int.class, -1)` clamped a negative length to zero instead of
throwing `NegativeArraySizeException`. Both are fixed; see
`array-index-out-of-bounds-has-no-detail-message-FIXED-20260806.md` in the
internal tree.
Those were index bugs. This one is not, which is why it is its own record rather
than a residual line in that one.
