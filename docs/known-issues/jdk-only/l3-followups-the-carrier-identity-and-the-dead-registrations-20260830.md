# L3 follow-ups — the carrier identity, and the registrations no door reaches — 2026-08-30

The L3 `java.util` lane closed on 2026-08-30 with every one of its recorded
residuals fixed and its 23-probe corpus 0-diff against HotSpot 25.0.4+7 in both
modes. Its record is retired to
`internal/jdk-only/l3-java-util-collections-1879-rows-and-69-defects-20260828.md`.

This page carries the two things that lane FOUND and does not own. Neither is a
loose end of the original scope; both were produced by probes written to close
it, and both still pose a question.

> **STAYS IN `known-issues/`, and here is the test it passes.** `03d2bd990`
> ("retire 107 closed records") swept this page into `internal/` on 2026-09-02
> and it was moved back. The convention is that `internal/` is for records with
> nothing open, and both sections below name something open with no owner
> holding it:
>
> * §1 is a compatible-mode carrier whose remedy `alloc_immutable_wrapper`'s own
>   doc assigns to `H0-2` §4 / `P4A` N1b -- ASSIGNED is not CLOSED, and 15 rows
>   of `apps/probes/ImmutableSplProbe` still differ because of it;
> * §2 is a work-list of thirteen registrations no dispatch door reaches, which
>   `tools/dead-registration-census.py` now enumerates and nobody has yet
>   deleted.
>
> The lane that produced them is closed; these are not. A retired page is
> uncitable from outside `internal/` and reads as answered, which is the wrong
> signal for both.

---

## 1. Every remaining differing row is a class NAME

`apps/probes/ImmutableSplProbe` is the only probe in the `java.util` corpus that
still differs, and it differs on no characteristics row. Fifteen rows in
compatible mode, two under `--jdk-only`, all of them a `getClass().getName()`:

```text
compat  Map.of(1).keySet()    Collections$UnmodifiableSet   HotSpot AbstractMap$1
compat  Map.of(1).values()    Collections$UnmodifiableColl  HotSpot AbstractMap$2
compat  List.of(1).spliterator()  java.util.Spliterator     HotSpot Collections$2
strict  Collections.unmodifiableSet(hashSet).spliterator()
                              java.util.Spliterator         HotSpot Spliterators$IteratorSpliterator
```

### 1a. The compatible-mode carrier — already owned, already diagnosed

The first two are `cratonvm/internal/Unmodifiable*`, and the remedy is recorded
in the tree at `alloc_immutable_wrapper`'s doc comment, which concludes:

> Retiring stubs moves compatible mode by zero; **retagging moves compatible
> mode by zero too.** `H0-2` §4 is a compatible-mode measurement, so its remedy
> has to be a compatible-mode change — retiring this carrier at its producers,
> or `P4A` N1b option (c). Not a tag.

The measurement that supports it, taken 2026-08-30 on one probe run: **19
`cratonvm/internal/Unmodifiable*` rows have invocations in a compatible run and
ZERO in a strict one.** That is exactly why strict already answers every mask
and every class name correctly on its own, and it is the cleanest available
argument that the carrier — not any individual answer it gives — is the defect.

The family is wired through 71 sites in `native-collections/src/lib.rs`.
Retiring it means `List.of`/`Set.of`/`Map.of`/`Collections.unmodifiable*`/
`copyOf` stop being fabricated in the DEFAULT mode, which is a change no
`java.util` probe corpus can validate on its own — Spring, H2 and Kafka all use
these constantly. It wants its own lane, not a rider on someone else's.

### 1b. `java.util.Spliterator` is an INTERFACE, and we instantiate it

This one is worth separating, because it is impossible rather than merely
different. No real JVM can report `java.util.Spliterator` from `getClass()`;
an interface has no instances. This crate mints its spliterator as a concrete
object whose class name IS the interface, so:

```java
new HashSet<>(List.of("a")).spliterator().getClass().getName()
    // HotSpot   java.util.Spliterators$IteratorSpliterator
    // CratonVM  java.util.Spliterator
```

It is visible under `--jdk-only` too, and by a route that has nothing to do with
the compatible-mode carrier: `Collections.unmodifiableSet(hashSet)` is real
`java.util.Collections$UnmodifiableSet` bytecode there, and it delegates to the
backing set's `spliterator()`, which is ours.

**It has a second consequence, and that is the reason to fix it rather than
record it.** Registrations made on `java/util/Spliterator` are reachable —
because our own objects carry that class name — where every other
interface-named registration is dead (§2). So the same name means "dead
registration" in one place and "live registration" in another, and no reader can
tell which without producing an object and measuring. See §2 for the case where
that ambiguity is load-bearing.

No behavioural row is known to depend on it: every characteristics and
`estimateSize` row in `ImmutableSplProbe` matches. The risk is an application
that switches on a concrete spliterator type, and `instanceof Spliterator.OfInt`
is the shape to check first.

---

## 2. Thirteen registrations that no dispatch door can reach

`apps/probes/DeadDoorProbe` calls each of these with the most favourable
receiver available and then reads the native registry back. All stay at
`invocations = 0`:

```text
  AbstractCollection.toArray()      AbstractSet.hashCode()
  Collection.stream()               Collection.toArray(IntFunction)
  Map.forEach(BiConsumer)           SequencedMap.pollFirstEntry/pollLastEntry
  TimeZone.getOffset(J)             TimeZone.getDisplayName() x2
  TimeZone.getOffsets(J[I)          TimeZone.setDefaultZone()
  Spliterator.forEachRemaining      (see below -- NOT dead)
```

**The rule.** An instance-method registration on an abstract class or an
interface is unreachable by every door. `dispatch_virtual` probes the registry
with the RECEIVER's runtime class; the lambda and stackless paths probe with the
DECLARING class of the resolved method, which for `plain.forEach` is
`AbstractMap` and never `Map`. The probe exercises both, including bound method
references, which is the door that reaches a declaring class.

**The control is in the same dump.** `TimeZone.getTimeZone` (inv=4) and
`TimeZone.getDefault` (inv=1) fire normally, because a STATIC call names the
class directly. So this is not a broken counter — the counter works, on the same
class, in the same run.

For `TimeZone` the point is sharper: **no factory ever returns a
`java.util.TimeZone`.** `getTimeZone("America/New_York")`, `getDefault()` and
`getTimeZone("UTC")` all hand back `sun.util.calendar.ZoneInfo`, and `TimeZone`
is abstract, so no instance of the registered class can exist at all.

### Why this is not a delete-them-all patch

`Spliterator.forEachRemaining` is in the list above and is NOT dead, for the
reason in §1b: this crate produces objects whose class name is that interface,
and the corpus exercises one of that class's two registrations. The rule
therefore reads "dead unless something produces a carrier with that name" — and
that question is per-row, answerable only by finding the producers.

The cost of leaving them is not runtime; it is that a dormant registration is a
trap that arms itself when something finally does produce a matching class.
That is not hypothetical here: a `java/util/PriorityQueue$Itr` row sat in a
2-field-snapshot registrar since before anything minted that class, and the day
this lane started minting it, the dormant row won the slot (`owns=True inv=4`)
over the registration that matched the shape actually produced, and killed
`PqOptionalShadowSweep` at row 54.

### What a fix looks like — the missing tool now exists

Per row: find every producer of an object whose class name equals the registered
class. If there is none, delete the registration. The `--jdk-only` census and
`dump_synthetic_stubs` both enumerate registrations and neither enumerates
producers; **`tools/dead-registration-census.py` does** (added 2026-08-30). It
crosses two mechanical inputs — `javap` on the image for which names are
abstract or interfaces, and a grep over the four allocation helpers for which
names this VM mints — and reports the rows that can have no receiver.

Its first run, over `java/util/*`:

```text
  rows on abstract/interface classes NOTHING mints:  199
  rows on abstract/interface classes THIS VM mints:  284
```

The 284 are the exception this page is about: `Stream`, `IntStream`,
`LongStream`, `DoubleStream`, `Collector` and `Spliterator` are all interfaces
that CratonVM mints as concrete carriers, so registrations on those names are
LIVE. The 199 span eighteen `java.util.function.*` interfaces, `BaseStream`,
`ReferencePipeline`, two `java.util.logging` abstract classes, and the
`java.util` set §2 already names.

**Validated, and the blind spot measured.** `apps/probes/LambdaClassProbe` asks
the one question that would overturn the function-interface verdict — is a
lambda's runtime class its interface's name? It is not: lambdas and method
references are `$$Lambda` hidden classes here exactly as on HotSpot, and
anonymous and named implementations carry their own names. So those rows really
have no receiver.

The same probe records the trap that makes a naive check wrong.
`getClass()` is NOT the name dispatch uses:
`List.of("a").stream().getClass()` answers
`java.util.stream.ReferencePipeline$Head` while the object's INTERNAL name is
`java/util/stream/Stream`, because a reported-name mapping sits in front of it.
A runtime `getClass()` reading would therefore have called the stream carriers
un-minted and invited deleting 284 live rows.

**Still not a delete list on its own.** The script is deliberately conservative
in the safe direction — a false "minted" leaves a dead row in place, a false
"not minted" would invite deleting a live one — and a candidate still wants the
`DeadDoorProbe` confirmation in §2: call the method through every door, dump the
registry, and check the count is still zero, with the static rows on the same
class as the control.

Deleting them moves `stub_ratchet`, `registrar_drift` and
`registrar_reachability`, all paired ratchets that want the removed rows NAMED.
The census output is that list.

Deleting registrations moves `stub_ratchet`, `registrar_drift` and
`registrar_reachability`, all of which are paired ratchets that want the removed
rows NAMED rather than a re-frozen number.

---

## 3. Reproduce

```bash
CV=target/release/cratonvm
"$JDK/bin/javac" -d apps/probes/out \
    apps/probes/DeadDoorProbe.java apps/probes/ImmutableSplProbe.java
for C in DeadDoorProbe ImmutableSplProbe; do
  "$JDK/bin/java" -cp apps/probes/out "$C" > /tmp/$C.hs 2>/dev/null
  "$CV" --java-home "$JDK"            -cp apps/probes/out "$C" > /tmp/$C.compat 2>/dev/null
  "$CV" --java-home "$JDK" --jdk-only -cp apps/probes/out "$C" > /tmp/$C.strict 2>/dev/null
  diff /tmp/$C.hs /tmp/$C.strict
done

# The invocation counts the §2 argument rests on:
"$CV" --java-home "$JDK" --jdk-only --dump-native-registry=/tmp/reg.json \
      -cp apps/probes/out DeadDoorProbe
```

Read the ROW COUNT and the trailing `DONE <probe>` before reading any diff.
