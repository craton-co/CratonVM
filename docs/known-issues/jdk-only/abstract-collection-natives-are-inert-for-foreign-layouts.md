# The `java.util.Abstract*` natives are inert for foreign layouts — measured, with the reason

**Status:** CLOSED as a hazard, 2026-08-06. This is a negative result, recorded
because two records and a working note all treat it as an open worry and it is
the cheapest thing in the area to keep re-deriving.

Both L5 records flag `native-collections`' abstract-method registrations as the
reason its 989 `Bridge` rows cannot be reclassified:

> 214 of the registrations are on abstract interface methods that decide dispatch
> for every *user* subclass, not just for `java.util`.

And the standing working note for the area says the natives on
`java/util/Abstract*` intercept every user subclass, that
`collect_collection_elements` returns an empty vec for an unmodelled layout, and
that `AbstractSet.hashCode` therefore answered **0** — a wrong-bucket miss that
surfaces far away as an NPE on a null `Map.get`.

## The measurement

`probes/ForeignLayoutCollectionProbe.java` subclasses `AbstractCollection`,
`AbstractSet`, `AbstractList` and `AbstractMap`, implementing **only** what each
leaves abstract, and holds its elements in a layout nothing can recognise — a
`String` of comma-separated values, split on demand. It then calls the inherited
**concrete** surface (`contains`, `containsAll`, `toArray`, `isEmpty`,
`hashCode`, `equals`, `toString`, `indexOf`, `subList`, `keySet`, `values`,
`containsValue`, `stream().count()`) and prints every observable as
`key=value`.

**All 42 lines are byte-identical to HotSpot 25**, including `hashCode` matching
the specified element-sum, `equals` against a real `Set.of`/`List.of`/`Map.of`,
and the call counters showing the subclass's own `iterator()` / `get()` /
`entrySet()` being driven 9, 10 and 11 times.

## Why — and it is not "the natives did not run"

That was the obvious explanation and it is wrong. The census for the same run:

| native | invocations |
|---|---:|
| `java/util/AbstractCollection.contains(Ljava/lang/Object;)Z` | 8 |
| `java/util/AbstractMap$SimpleEntry.getKey()` | 9 |
| `java/util/AbstractMap$SimpleEntry.getValue()` | 6 |
| `java/util/AbstractCollection.toArray()` | 1 |
| `java/util/AbstractSet.hashCode()` | 1 |

They **ran**, on a layout none of them models, and answered correctly — because
this family has the escape hatch: `collect_collection_elements_or_real` falls
back to the collection's real `size()` + `toArray()`, and
`collect_via_real_iterator_once` covers the plausible-but-null-holed snapshot.
The incident-by-incident fixes the working note complains about add up to
coverage of this whole surface, including the `AbstractSet.hashCode` row it
names.

**Ten rows, not 97 or 214.** A source grep for `"java/util/Abstract*"` finds 97
occurrences across the crates, but in real-JDK mode only **10** registrations
exist: 4 on `AbstractCollection`, 5 on `AbstractMap$SimpleEntry`, 1 on
`AbstractSet`. The larger numbers are string occurrences including
synthetic-jdk-only registrars and non-registration uses. Size a lane off the
census, not off a grep.

## What this closes, and what it does not

**Closed:** the foreign-layout hazard on the `Abstract*` family, for the
inherited-concrete surface, in real-JDK mode. The probe is the regression test.

**Not closed, and the contrast is the useful part:** the identical shape one
package over *is* broken.
[`process-natives-answer-for-user-subclasses.md`](process-natives-answer-for-user-subclasses.md)
has `java.lang.Process`'s concrete natives answering for a user subclass out of
the VM's fixed field layout — `isAlive()` false for a live process, `pid()` 0
where the spec requires `UnsupportedOperationException`.

The difference is not dispatch, and that is worth stating because it looks like
it should be: in both cases the native is registered on the supertype, and in
both cases dispatch reaches it. The difference is whether the native has a
foreign-layout fallback. The collections family asks the object
(`size()`/`toArray()`/`iterator()`); the process family reads
`PROC_FIELD_HANDLE` and `PROC_FIELD_EXIT` by fixed index and believes what it
finds.

So the generalisable rule for adjudicating any native registered on a JDK
supertype is: **does it ask the receiver, or does it index into a layout it
assumes?** The first is safe for arbitrary subclasses; the second is a defect
waiting for its first foreign receiver. `native-collections` passes that test on
the rows measured here; `native-io`'s process family does not.
