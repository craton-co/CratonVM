# A boolean read as a size: every copy of a `Stream.toList()` list kept one element

**Status: FIXED 2026-08-23.**

Opened out of `../gpu/gpullama3-on-the-cratonvm-gpu-20260822.md`, which
recorded it as a residual: CratonVM built 11 prompt ids where HotSpot
built 16 for `"Why is the sky blue?"`, keeping `Why` and dropping
` is`, ` the`, ` sky`, ` blue`, `?`. The model answered a question it
had not been asked, and nothing on the path reported anything.

It was not a tokenizer defect.

## What it was

The native layout probe that reads a foreign collection
(`collect_collection_elements`, `native-collections`) guesses the
receiver's shape from the runtime `Value` of its slots: an `Object[]` in
slot 0 and "an int" in slot 1 means `(elementData, size)`.

CratonVM represents a `boolean` and an `int` in the same `Value::Int`.
So a class laid out `(Object[], boolean)` reads as a collection whose
size **is that boolean**, and a `true` reads as size **1**.

`java.util.ImmutableCollections$ListN` has been exactly that shape since
JDK 20:

```java
static final class ListN<E> extends AbstractImmutableList<E> {
    @Stable private final E[] elements;
    @Stable private final boolean allowNulls;
```

and `Stream.toList()` builds it through
`listFromTrustedArrayNullsAllowed` — that is, with `allowNulls = true`.

Every copy of such a list therefore kept its first element and dropped
the rest.

## Why it survived so long

**The list itself was fine.** Measured on the unfixed binary, for a
six-element list:

| reader | answer |
|---|---|
| `size()` | 6 |
| `get(5)` | 30 |
| `contains` / `indexOf` | correct |
| `iterator()` | 6 |
| `stream().count()` | 6 |
| `toArray()` / `toArray(T[])` | 6 |
| `subList(0,6)` | 6 |
| **`ArrayList.addAll`** | **1** |
| **`LinkedList` / `Vector` / `HashSet.addAll`** | **1** |
| **`new ArrayList<>(src)`** | **1** |
| **`List.copyOf`** | **1** |

Only the paths that COPY were wrong, and all of them were wrong
identically, because they share one source-reading helper. Any test that
exercises a list's own accessors — the obvious thing to write — passes
against this defect completely.

It is also invisible from the construction site. `Stream.of(...)
.toList()`, `IntStream.range(..).boxed().toList()`, a filtered or mapped
`toList()`, and `List.of(...)` all copy correctly on CratonVM: the first
four because CratonVM's own `toList` natives answer them with a real
`ArrayList`, and `List.of` because it builds a ListN with
`allowNulls = false`, whose `0` makes the probe fall through to the
correct path by accident. Only a stream shape that falls through to JDK
bytecode AND allows nulls reaches the bad branch —
`Arrays.stream(int[]).boxed().toList()` is one, and it is what
GPULlama3's `encodeAsList` returns.

## The second instance, not the first

Three hundred lines above the probe there is this, already in the tree:

> `kotlin.collections.ArrayAsCollection` ... has layout
> `(values: Object[], isVarargs: Boolean)`. The generic "f0 = array,
> f1 = int size" ArrayList-shape heuristic a few lines below reads field
> 1 as a `Value::Int` and matches it regardless of whether the real
> field is an int or a boolean ... so `isVarargs=true` gets misread as
> `size=1`, and every `new ArrayList<>(mutableListOf(a, b, ...))` copy
> silently truncates to just the FIRST element.

Same defect, same mechanism, written down and fixed by special-casing
that one class name. So this was a known hazard patched as a specific
bug, which is why it came back on a different class.

## The fix

At the root: the probe now consults the **declared descriptor** of slot
1 (`declared_fields`, memoised per `(ClassId, slot)` because the probe
runs on every `addAll` and copy constructor in the process) and refuses
the `(array, size)` shape when that slot is positively known not to be
`int`.

It rejects only on positive evidence. Where a layout is fabricated and
carries no field metadata — synthetic-JDK mode — the descriptor is
`None` and the probe behaves exactly as before.

Plus a direct arm for `ImmutableCollections$ListN` / `$SetN` that reads
their `elements` array by name, so no call site depends on the
`toArray()` fallback. The two differ in one respect that matters: a
ListN's array IS the element sequence and may legitimately hold nulls,
while a SetN's is an open-addressed table whose nulls are empty slots.

## The vector

`regression-suite/src/RStreamToListCopy.java`, 31 checks. It passes on
Temurin 25.0.3+9 and, on the unfixed CratonVM, fails with

```
AssertionError: ArrayList.addAll content:
  expected [10445, 374, 279, 13180, 6437, 30] got [10445]
```

Every assertion copies INTO something and measures the copy, because
the accessors all agreed. The destinations are varied on purpose —
ArrayList, LinkedList, Vector, HashSet, LinkedHashSet, the copy
constructor, `List.copyOf`, and positional `addAll(int, Collection)` —
because the defect was in the shared reader, so a single destination
would have read as a single native's bug. Every other stream source is
asserted too, so a fix that special-cases one construction path cannot
be mistaken for a fix. A 257-element case crosses any small-size
special casing.

## Confirmation on the workload it came from

```
HotSpot   PROMPT_IDS n=16 [128000, 128006, 882, 128007, 198, 10445, 374,
                           279, 13180, 6437, 30, 128009, ...]
CratonVM  PROMPT_IDS n=16 [128000, 128006, 882, 128007, 198, 10445, 374,
                           279, 13180, 6437, 30, 128009, ...]
```

identical, where before the fix CratonVM answered 11.
