# `ArrayIndexOutOfBoundsException` from an array access carries no detail message

**Status:** PARTIALLY FIXED 2026-08-06 (`510f12fc6`). There is now a message
where there was `null`; it is not yet HotSpot's *wording*, because that needs
the array length and the variant still does not carry one. The remaining work is
exactly the widening described under "What must change" below.

Low severity throughout — the **class** is right everywhere, so nothing catches
differently. This is a diagnosability gap, not a control-flow one.

**Reproducer:** `probes/PreconditionsFormatterProbe`, the two `Array domain`
rows, and `docs/known-issues/repros/list-get-oob/LG2.java`.

```
                       HotSpot 25                                                CratonVM before   CratonVM now
int[] load oob         Index 9 out of bounds for length 4                        null              Array index out of range: 9
System.arraycopy oob   arraycopy: last source index 9 out of bounds for int[4]   null              (unchanged — different throw path)
```

`as_java_throwable` no longer discards the payload: it returns
`Option<Cow<'_, str>>` and a `synthesised_detail_message` builder supplies the
JDK's own `int`-constructor wording, `"Array index out of range: {index}"`, for
the two variants that carry an operand but have no `String` to borrow
(`ArrayIndexOutOfBoundsException`, `NegativeArraySizeException` — the latter now
matching HotSpot exactly, since HotSpot's message there *is* just the size).

That deliberately stops short of HotSpot's array-access wording, which needs the
length. Using the `int`-constructor text is exact for what the variant knows
rather than a guess at what it does not — and it is what the JDK itself prints
when only an index is available.

## Why it happens

`RuntimeError::ArrayIndexOutOfBoundsException { index: i32 }` carries the index
and nothing else, and `RuntimeError::as_java_throwable` maps it to
`("java/lang/ArrayIndexOutOfBoundsException", None)` — the no-arg constructor,
so `getMessage()` is null by design rather than by accident. HotSpot's message
needs the array **length** as well, which the variant has no room for; the
`arraycopy` message additionally needs the array's type name and which of the
five arguments failed.

## Why it was not fixed alongside the `Preconditions` formatter work

The formatter fix
([record](../internal/preconditions-ignores-the-exception-formatter-FIXED-20260805.md))
was about exception **classes**: `String` callers getting
`ArrayIndexOutOfBoundsException` where `catch (StringIndexOutOfBoundsException)`
was written, and `Objects.check*`/NIO callers getting a *subclass* of the
`IndexOutOfBoundsException` they were promised. Both changed control flow.

This one does not. Widening the variant means touching roughly 40 construction
sites across `native-awt`, `native-builtins`, `native-builtins-crypto`,
`native-io` and `vm`, each of which has to be given the length it did not
previously need — several of them do not have it in scope. That is its own
change with its own review, not a rider on an exception-class fix.

## What must change (still open)

Give the variant the operands HotSpot's message needs, and fill them at the
throw sites that have them:

```rust
ArrayIndexOutOfBoundsException { index: i32, length: Option<i32> }
```

`None` keeps today's message-less behaviour for the sites that genuinely cannot
say (a native that only knows the index), so the migration can be incremental
and the interpreter's own `aaload`/`aastore`/`arraylength` bounds checks — the
ones users actually hit — can be done first. `System.arraycopy` has its own
message shape (`"arraycopy: last source index %d out of bounds for %s[%d]"`)
and wants a dedicated variant or a preformatted message.

## Verification when fixed

The two `PreconditionsFormatterProbe` rows, class and message. Extend the probe
with the `arraycopy` variants HotSpot distinguishes (`srcPos < 0`,
`length < 0`, `last destination index …`) before claiming the family.
