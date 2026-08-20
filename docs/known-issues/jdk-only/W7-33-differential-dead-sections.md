# W7-33 — the two dead probe sections: a null the JDK reads as comodification, and a `modCount` written over a monitor

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** The
> `## Out-of-file patch (not applied)` is **APPLIED** — commit `aab87e003`
> *fix(collections): the two W7-33 residuals -- PriorityQueue null refusal,
> Stack's own exception type*. `RuntimeError::EmptyStackException` at
> `types/src/error.rs:1088-1098`, mapped at `:1633`, exhaustiveness at `:1720`,
> raised at `native-collections/src/lib.rs:35632`. No `"Stack is empty"`
> `NoSuchElementException` remains.
>
> * **Headline and out-of-file patch: CLOSED in source, unverified.**
> * **Residual: APPLIED 2026-08-12, unbuilt** — both additions the follow-up
>   specifies are now in `classloading/src/class_manager.rs`
>   (`jdk_superclass` → `java/lang/RuntimeException`, and the
>   `synthetic_stub_fields` arm), anchored on the quoted blocks rather than on
>   line numbers. Neither can move either shipping mode or any Compatible-mode
>   ratchet, and **nothing in the tree can observe them**, because no suite runs
>   `--synthetic-jdk` MODE — so this closes source-only and deliberately without
>   a fixture assertion. It is not untested-but-testable; it is unobservable
>   here. W7-36-differential-view-families.md named the two additions.

**Status: BOTH DIAGNOSED AND FIXED IN SOURCE 2026-08-12, NOT REBUILT.**

Every measurement below was taken by running the already-built binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` against Temurin
`jdk-25.0.3.9-hotspot` on windows/x64 — one binary, the same class files on
both sides, `-Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US`
pinned on both. Nothing here claims a source change works. Every "before" row
is an observation; every "after" row is either a claim about source or a
measurement taken with the poisoning statement excised from the probe, and each
is labelled as such.

Branch: `fix/differential-dead-sections-20260812`.
File changed: `native-collections/src/lib.rs`, and nothing else.

Predecessor: W7-32-round-2-differential-run (the 96-line differential this
closes 53 lines of), W7-16-arraydeque-and-linkedlist-residuals (the `+ 1`
buffer invariant that the `ArrayDeque` defect here sits directly on top of),
W7-1-treemap-views-and-iterator-remove-contract (the `modCount` this campaign
introduced, and which the `ConcurrentModificationException` below was wrongly
suspected of).

---

## The shape both defects share

Neither throw came from CratonVM. **Both came from real JDK bytecode, correctly
refusing state that a CratonVM native had corrupted.** The exception was raised
several calls downstream of the native that caused it, by a class the native
never touches, which is why a section fence reporting the exception type named
neither cause.

That is the diagnostic rule this record is worth keeping for: when a
`--real-jdk` run throws out of `java.base`, the first question is not "which of
our natives throws this" — grep will answer "none" — but **"which of our natives
wrote the field this JDK method just read"**.

---

## Part 1 — `dequeEdges`: `ConcurrentModificationException`

### The statement that throws

`probes/ShadowDifferentialProbe.java:1153`:

```java
line("ArrayDeque.descendingIterator", joinIter(dq.descendingIterator()));
```

Bisected off the transcript, not guessed: the last line CratonVM printed before
the fence was `ArrayDeque.removeLastOccurrence` (probe line 1152) and the next
observable HotSpot prints is `ArrayDeque.descendingIterator`.

### It is not an over-throw, and it is not the `modCount`

The suspicion on the table was that this crate's new `modCount` was being
bumped where the JDK would not bump it. It is not: **`CopyOnWriteArrayList` is
the only class in this file whose `modCount` handling is wrong, `ArrayDeque`
has no `modCount` write at all, and the CME here is raised by JDK bytecode.**

Reduced to nine lines and run on the binary:

```java
ArrayDeque<String> d = new ArrayDeque<>();
d.add("a"); d.add(null); d.add("b");
for (Iterator<String> it = d.descendingIterator(); it.hasNext();) it.next();
```

```text
java.util.ConcurrentModificationException
    at java.util.ArrayDeque.nonNullElementAt(ArrayDeque.java:268)
    at java.util.ArrayDeque$DescendingIterator.next(ArrayDeque.java:746)
```

The same iterator over a null-free deque is clean, on the same binary, in the
same process:

| deque | `descendingIterator()` drained |
|---|---|
| `["a", "b"]` | `b;a;` — matches HotSpot |
| `["a", null, "b"]` | `ConcurrentModificationException` |

### The JDK rule

`ArrayDeque.nonNullElementAt` is three lines:

> the element is read, and if it is `null` a `ConcurrentModificationException`
> is thrown

`ArrayDeque` refuses `null` on the way *in* — `addFirst` and `addLast` both open
with `if (e == null) throw new NullPointerException();` — precisely so that a
null read on the way *out* can only mean one thing: another thread nulled a
slot mid-iteration. The null-refusal and the comodification check are one
mechanism, and CratonVM had implemented the second half without the first.

So the CME is HotSpot's own rule, applied to state HotSpot would never have
allowed to exist. The defect is upstream: `ArrayDeque.add(null)` returned
`true`.

Measured, before the fix:

| observable | HotSpot | CratonVM |
|---|---|---|
| `ArrayDeque.addNull` | `java.lang.NullPointerException` | `no-throw` |
| `ArrayDeque.addFirstNull` | `java.lang.NullPointerException` | `no-throw` |
| `ArrayDeque.offerNull` | `java.lang.NullPointerException` | `no-throw` |
| `ArrayDeque.sizeAfterRefusedNulls` | `0` | `3` |
| `ArrayDeque.content` | `[a, x, y, x, z]` | `[a, null, null, null, x, y, x, z]` |
| `ArrayDeque.descendingIterator` | `z;y;a;` | *section dead* |

### The fix

`ad_refuse_null` in `native-collections/src/lib.rs`, called from
`native_ad_add_first` and `native_ad_add_last` before anything is pinned or
grown. Those two are the only funnels: `add`, `offer`, `offerFirst`,
`offerLast`, `push` and `addAll` all delegate to them here exactly as they
delegate to `addFirst`/`addLast` in the JDK, so one check per funnel covers the
whole registered surface.

The `NullPointerException` carries no message, because HotSpot's does not and
`thrownDetail` prints a message when there is one.

**The one other caller of those funnels is the `java/util/Queue` interface
bridge** (`registry.register("java/util/Queue", "offer"/"add", …,
native_ad_offer/native_ad_add)`), which is a concern only if a null-tolerant
`Queue` routes through it. The null-tolerant `Queue` in the JDK is
`LinkedList`, and it does not — measured on the binary, in **both** modes:

```text
LinkedList.asQueue.offerNull=no-throw    ll=[null] size=1
```

### Claimed, not verified

On a rebuilt binary the four `ArrayDeque` null observables should read
`java.lang.NullPointerException` ×3 and `0`, matching HotSpot. That is a claim
about source. What *is* measured is everything downstream — see Part 3.

---

## Part 2 — `concurrentAndAtomic`: `NullPointerException`

### The statement that throws

`probes/ShadowDifferentialProbe.java:1349`:

```java
line("COW.addAllAbsent", cow.addAllAbsent(List.of("a", "qq")) + ":" + cow);
```

With `KRUN_STACK=1`, on the binary:

```text
java.lang.NullPointerException: Cannot enter synchronized block because "this.lock" is null
    at java.util.concurrent.CopyOnWriteArrayList.addAllAbsent(CopyOnWriteArrayList.java:795)
```

Again real JDK bytecode, again refusing state we wrote.

### Which operation nulls `lock`

`addAllAbsent` is only the *reader* of the corrupted field. A twelve-row bisect
— build a fresh `CopyOnWriteArrayList`, apply one operation, then probe with
`addAllAbsent` — names the writers:

```text
fresh           lock=OK      set             lock=OK
add             lock=OK      remove          lock=OK
addIfAbsentNew  lock=NULL    iterator        lock=OK
addIfAbsentDup  lock=OK      addAtIndex      lock=OK
addAll          lock=NULL    clear           lock=OK
                             toString/size   lock=OK
```

The two that fail are exactly the two that reach `cowal_bump_mod_count` —
`addIfAbsent` only on the branch where the element was absent (the duplicate
branch returns before the bump), and `addAll`. `bulkRemove` is the third call
site and was not exercised by the probe.

### The root cause: a field index resolved against a class the receiver does not extend

```rust
ctx.resolve_field_index("java/util/AbstractList", "modCount")
```

`resolve_field_index(class, field)` answers for the **named** class's
hierarchy, not the receiver's. `CopyOnWriteArrayList` does not extend
`AbstractList` at all —

```text
public class java.util.concurrent.CopyOnWriteArrayList<E>
    implements java.util.List<E>, java.util.RandomAccess, java.lang.Cloneable, java.io.Serializable
  final transient java.lang.Object lock;
  private volatile transient java.lang.Object[] array;
```

— and has exactly two instance fields. `AbstractList`'s `modCount` is *its*
first instance field, so the two indices collide and every bump stored an `Int`
over the monitor object the class synchronises on. The list's *contents* stayed
correct throughout (`COW.addIfAbsentNew=true:[a, b, zz]` matches HotSpot),
because `array` is the slot the natives resolve by name and never the one the
bump lands on. Only the lock died, and only silently, until real bytecode
reached a `synchronized (lock)`.

A range check would not have caught it: the bad index is `0`, comfortably in
range for a two-field object.

### This hazard was already documented — in the other helper

`al_mod_count_slot`, added to this same file this session, opens with a doc
block naming this exact failure mode ("a resolver that answered `Some(0)` there
would have us store an `Int` over the backing-array REFERENCE") and guards
against it with two exclusions plus a range check. `cowal_bump_mod_count` was
written separately and inherited none of it. **A guard written for one helper
is not a guard on the invariant** — the second implementation of the same
primitive is where to look, which is the shape
`reference_g1_refill_tlab_carved_unaligned_tlabs` records for the collectors.

### The fix

`cowal_bump_mod_count` now asks the **receiver's own class**
(`resolve_field_index_by_class_id(class_id_of_object(this), "modCount")`),
refuses any index that collides with `lock` or `array`, refuses an
out-of-range index, and refuses a slot that does not already hold an `Int`.

On the real JDK layout the answer is `None` and nothing is written — which is
the *correct* semantics rather than merely the safe one. A real
`CopyOnWriteArrayList` has no `modCount`, its iterator is a snapshot, and it
never raises `ConcurrentModificationException`; the bump only ever meant
anything for the legacy synthetic stub that mirrored `ArrayList`, and that stub
reaches none of the three call sites (all three sit on the named-`array`
branch, which the stub does not take).

### Verified on the current binary

The `lock` clobber, not `addAllAbsent`, is the whole defect. Reach the probe's
exact COW state without the clobbering call and the observable already matches
HotSpot on today's binary:

```java
CopyOnWriteArrayList<String> cow = new CopyOnWriteArrayList<>(List.of("a","b"));
cow.add("zz");                         // `add` does not bump; `addIfAbsent` does
cow.addAllAbsent(List.of("a","qq"));
```

```text
CratonVM: COW.addAllAbsent=1:[a, b, zz, qq]
HotSpot:  COW.addAllAbsent=1:[a, b, zz, qq]
```

---

## Part 3 — the differential, re-taken with both sections alive

Re-running the unmodified probe would need a rebuilt binary, which this branch
may not produce. So the sections were brought back to life on **today's**
binary instead: a scratchpad copy of the probe with the five poisoning
observables excised — the three `ArrayDeque` null adds plus
`sizeAfterRefusedNulls`, and `COW.addAllAbsent`.

That excision is not an approximation for the `ArrayDeque` half. A fixed VM
*refuses* those three adds, so the deque it leaves behind is empty — byte-for-byte
the state a probe that never called them leaves behind. For the COW half it
removes one observable, measured separately above.

Both sections now reach the end. **No `SECTION-DIED`, `PROBE-DONE` on both
sides.**

| | baseline | sections alive |
|---|---|---|
| divergent observables | **96** | **43** |
| sections dead | 2 | **0** |

Of the 53 lines closed, 48 were absence — observables that existed only on the
HotSpot side because the CratonVM transcript stopped — and 5 are the excised
statements, 1 of which (`COW.addAllAbsent`) is measured converged above and 4 of
which are claims about source.

### Families, before and after

| family | W7-32 | sections alive | note |
|---|---|---|---|
| ArrayDeque | 15 | **0** | 11 measured clean; 4 excised, claimed |
| ABQ | 10 | **0** | every one was absence |
| AtomicInteger | 9 | **0** | every one was absence |
| LinkedList | 4 | **0** | every one was absence |
| ConcurrentLinkedQueue | 3 | **0** | every one was absence |
| AtomicReference | 2 | **0** | absence |
| COW | 1 | **0** | measured converged, Part 2 |
| AtomicLong | 1 | **0** | absence |
| AtomicBoolean | 1 | **0** | absence |
| LongAdder | 1 | **0** | absence |
| ArrayDequeAsStack | 1 | **0** | absence |
| Stack | 4 | **1** | 3 were absence; 1 real, below |
| PriorityQueue | 3 | **1** | 2 were absence; 1 real, below |
| format | 12 | 12 | untouched by this branch |
| Throwable | 5 | 5 | |
| TreeMap | 4 | 4 | |
| subList | 3 | 3 | |
| VM | 3 | 3 | |
| TreeSet | 3 | 3 | |
| CHM | 2 | 2 | see residuals |
| stream / Map / HashMap / keySet / values / unmodifiableList / Enum / Random / NumberFormat | 1 each | 1 each | |

**The six concurrent/atomic families the round-2 widening was built to size —
ABQ, AtomicInteger, AtomicLong, AtomicBoolean, AtomicReference, LongAdder — are
clean. So are ArrayDeque, LinkedList and ConcurrentLinkedQueue.** All 41 of
those lines were the fence, not defects. The next lane's real target list is
`format` (12), `Throwable` (5), `TreeMap` (4), `subList` (3), `VM` (3),
`TreeSet` (3) and `CHM` (2).

---

## Residuals this made visible, deliberately not fixed here

Both were previously hidden behind the `dequeEdges` fence and are newly
*measurable* because of Part 1. Neither is fixed on this branch: they are
different defects from the two throws, and this branch is not rebuilt, so
folding unverifiable changes into it would only make the numbers above harder
to trust.

**R1 — `PriorityQueue.add(null)` does not throw.** In file, one line.

```text
HotSpot:  PriorityQueue.nullAdd=java.lang.NullPointerException
CratonVM: PriorityQueue.nullAdd=no-throw
```

`PriorityQueue.offer` opens with `if (e == null) throw new
NullPointerException();` for the same reason `ArrayDeque` does — a null has no
place in a comparison heap. `native_pq_add` in `native-collections/src/lib.rs`
takes `elem` and never checks it. The fix is `ad_refuse_null(elem)?;` (the
helper is not `ArrayDeque`-specific, only its name is) immediately after `elem`
is bound, before `pin_native_root`.

**R2 — `Stack.pop()` on an empty stack throws the wrong exception.**

```text
HotSpot:  Stack.popOnEmpty=java.util.EmptyStackException
CratonVM: Stack.popOnEmpty=java.util.NoSuchElementException
```

`native_stack_pop` and `native_stack_peek` in `native-collections/src/lib.rs`
both raise `RuntimeError::NoSuchElementException { message: "Stack is empty" }`.
`java.util.Stack` raises `java.util.EmptyStackException`, which is not a
`NoSuchElementException` — it extends `RuntimeException` directly, so a
`catch (EmptyStackException e)` in application code does not fire. **This one
needs an out-of-file change** (see below) and so is recorded rather than made.

**R3 — `CHM.reduceValues` and `CHM.searchKeys` answer `null`.** Unchanged by
this branch, and now the only two `CHM` divergences.

```text
HotSpot:  CHM.reduceValues=6      CHM.searchKeys=found
CratonVM: CHM.reduceValues=null   CHM.searchKeys=null
```

Neither method is registered anywhere in the tree, so both run real JDK
bytecode. Ordinary iteration of the same map is fine on the same run
(`CHM.sortedContent={a=6}` matches, and that is `new TreeMap<>(chm)` walking
`entrySet`), so this is the bulk-operation `Traverser`/`BulkTask` path reading
raw `table` rather than the map being empty. A `null` from `reduceValues` is
also indistinguishable from "the map reduced to nothing", which is the quiet
shape the round-2 widening was built to catch.

---

## The R2 follow-up, restated with exact text — 2026-08-12

Re-verified in the tree before writing this: `RuntimeError::EmptyStackException`
exists (`types/src/error.rs:1098`), is mapped to `java/util/EmptyStackException`
with a `None` message at `:1633`, is in the exhaustiveness loop at `:1720`, and
is raised from `native-collections/src/lib.rs`. So the **headline** half of R2 is
in the tree. What is still not applied is the `--synthetic-jdk` half, and it is
still in `classloading/src/class_manager.rs`, which this lane does not own
either. Two additions, with the surrounding text as it stands today:

**1. `jdk_superclass` — without this a fabricated `EmptyStackException` defaults
to `java/lang/Object` and `catch (RuntimeException)` misses it.** Current:

```rust
        // java.util exceptions
        "java/util/NoSuchElementException"
        | "java/util/ConcurrentModificationException"
        | "java/util/InputMismatchException" => "java/lang/RuntimeException",
```

becomes

```rust
        // java.util exceptions
        "java/util/NoSuchElementException"
        | "java/util/ConcurrentModificationException"
        | "java/util/EmptyStackException"
        | "java/util/InputMismatchException" => "java/lang/RuntimeException",
```

**2. `synthetic_stub_fields` — the two-slot throwable shape, beside its
neighbour.** Current:

```rust
        | "java/util/NoSuchElementException"
        | "java/util/InputMismatchException"
```

becomes

```rust
        | "java/util/NoSuchElementException"
        | "java/util/EmptyStackException"
        | "java/util/InputMismatchException"
```

Both are inside `match` arms over slash-form class names; the only risk is
picking the wrong `match`, so anchor on the two quoted blocks rather than on line
numbers. Neither addition can move either shipping mode — a fabricated
`java/util/EmptyStackException` exists only under `--synthetic-jdk`, where the
real class file is absent — so no ratchet taken in Compatible mode moves either.

**Nothing in-tree can observe the difference**, because no suite runs
`--synthetic-jdk` mode. That is the reason this is a source-only close and not a
verified one, and it is also why the *headline* now has a witness and the
follow-up cannot.

### Coverage for the headline, which R2 did not have

`regression-suite/src/RExceptions.java` (`CORE_CLASSES`, so it runs on a default
invocation) now asserts both halves of the pair, and asserts the **type** rather
than merely that something was thrown:

```java
        boolean ese = false, eseWrongType = false;
        try { new Stack<String>().pop(); }
        catch (EmptyStackException e) { ese = true; }
        catch (NoSuchElementException e) { eseWrongType = true; }
        check(ese && !eseWrongType, "Stack.pop() on empty throws EmptyStackException");
```

plus the same for `peek()`. The `NoSuchElementException` arm is what makes it
non-vacuous: `EmptyStackException` is not a `NoSuchElementException`, so a
single `catch (EmptyStackException)` around the old behaviour would have died
uncaught instead of naming the wrong type it got. Both checks fail on the
pre-`aab87e003` behaviour, and `peek()` is included because `pop` calls `peek` in
the JDK and covering one of a pair is how the pair drifts apart again.

## ~~Out-of-file patch (not applied)~~ — APPLIED, see the reconciliation banner

For **R2** only. `java.util.EmptyStackException` has no `RuntimeError` variant,
so `native_stack_pop`/`native_stack_peek` cannot raise it today.

`types/src/error.rs`, beside `NoSuchElementException`:

```rust
    /// `java.util.EmptyStackException`. NOT a `NoSuchElementException` — it
    /// extends `RuntimeException` directly, so a `catch (EmptyStackException)`
    /// in application code does not fire on the wrong one. `java.util.Stack`
    /// is the only thrower.
    EmptyStackException,
```

plus the matching arm wherever `RuntimeError` maps a variant to its Java class
name (the same place `ConcurrentModificationException`, which is likewise
field-less, is handled).

Then, in `native-collections/src/lib.rs` — this file *is* on this branch, so
this half lands the moment the variant exists — replace both

```rust
        return Err(
            cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "Stack is empty".to_string(),
            }
            .into(),
        );
```

with

```rust
        return Err(cratonvm_types::error::RuntimeError::EmptyStackException.into());
```

in `native_stack_pop` and `native_stack_peek`.

---

## What was NOT changed

- `probes/ShadowDifferentialProbe.java`. The variant used for Part 3 is a
  scratchpad copy; the probe in the tree is untouched, and both of its stated
  properties (fenced sections, bounded loops) are intact. No `-after-100`
  marker appeared in either run, so nothing that terminates on HotSpot failed
  to terminate here.
- `native-builtins/src/phases_late/concurrent.rs`. The `concurrentAndAtomic`
  death was named `NullPointerException` in the concurrent/atomic surface and
  the file was on this branch for that reason, but the cause was in
  `native-collections` and this file needed no change.
- Compatible mode. The `ArrayDeque` null refusal applies in both modes, and
  should: it is the JDK's rule for `ArrayDeque` in both. The `modCount` change
  cannot reach the synthetic-stub layout at all — all three call sites are on
  the named-`array` branch, which a stub with unnamed slots does not take — so
  the synthetic path is bit-identical.

---

## Is any of this scheduled? — 2026-08-12 (doc-only lane)

Checked against the tree, because the reconciliation banner above closes items
"in source" and a source close is only as good as what re-reads it later.

**The source claims are all verified present.** `RuntimeError::EmptyStackException`
at `types/src/error.rs:1097-1098`, mapped at `:1633`, in the exhaustiveness loop
at `:1720`; no `"Stack is empty"` `NoSuchElementException` string remains
anywhere in `native-collections/src/lib.rs`. **Both** `classloading/src/class_manager.rs`
additions the R2 follow-up specifies are in the tree —
`| "java/util/EmptyStackException"` in the `jdk_superclass` arm at `:10673` and
in the `synthetic_stub_fields` arm at `:12116`, each with a comment naming the
`RuntimeException`-directly rule. So the banner's *"APPLIED 2026-08-12, unbuilt"*
is accurate on both halves.

**The headline's coverage is genuinely scheduled.** `RExceptions` is in
`CORE_CLASSES` (`regression-suite/run.sh:106`), and `RExceptions.java:206-220`
carries both the `pop()` and `peek()` pairs with the wrong-type arm. That is a
default-invocation gate, and it discriminates: `EmptyStackException` is not a
`NoSuchElementException`, so the pre-`aab87e003` behaviour trips
`eseWrongType`.

**The rest of this record's evidence is scheduled by nothing, and the number is
worse than "not by `run.sh`".** Every before/after row in Parts 1–3, all 96 and
all 43 divergent observables, and the whole family table come from
`probes/ShadowDifferentialProbe.java`. Measured:

* the string `probes` appears **zero** times in `regression-suite/run.sh`, at any
  `SUITE=` value — so **no suite run, however green, can discharge anything
  here**;
* the only scheduled consumer of `probes/` is
  `scripts/jdk-only-strict-probes.sh` (`.github/workflows/ci.yml:315`, `:1404`),
  whose `PROBE_LIST` default is three names —
  `JdkOnlyCensusLoadProbe JdkOnlyBreadthProbe JdkOnlyPlatformProbe` — out of
  **449 `.java` files in `probes/`**;
* `ShadowDifferentialProbe` is named by **no `.sh` and no `.yml`** in the tree.
  Its only in-tree references are Rust doc comments in
  `native-builtins/src/lang_class.rs` and `lang_string.rs` describing cases it
  once measured.

That is not an argument for scheduling it. A 1,400-line differential probe
diffed against HotSpot is the wrong shape for a gate — it goes red on any
unrelated formatting drift, which is the failure mode `run.sh`'s `extract()`
filter exists to avoid (W7-60). The point is narrower and it is a **standing
caveat for every reader of this record**: the 43 remaining divergent
observables, the residual list R1/R2/R3, and the "next lane's real target list"
(`format` 12, `Throwable` 5, `TreeMap` 4, `subList` 3, `VM` 3, `TreeSet` 3,
`CHM` 2) are **snapshots taken by hand on one binary on one day**. Nothing
re-takes them, nothing notices when one converges, and nothing notices when one
regresses. A lane that wants a number from this record must re-run the probe
itself; it must not infer from a green suite that the numbers still hold.

**R1 and R3 disposition.** R1 (`PriorityQueue.add(null)`) is closed in source by
`W7-36-differential-view-families.md` Part 1 and is covered by `RJdkViews`'s
`sortedContainerRefusals()`, which **is** scheduled (`RJdkViews` in
`CORE_CLASSES`). R3 (`CHM.reduceValues` / `searchKeys`) is closed in source by
W7-36 Part 1 R3 and has **no scheduled witness at all** — its only observable is
the probe. The long "deliberately still unregistered" list W7-36 leaves behind
(`reduceKeys`, `reduceEntries`, `searchValues`, `searchEntries`, the `…ToInt/Long/Double`
family, `forEachKey`/`forEachValue`/`forEachEntry`) is in the same position, and
for that list there is now a cheaper instrument than the probe: a
`--jdk-only-report` run emits `native-shadows-bytecode` rows carrying class,
method and descriptor, which answers "is this triple registered on this build"
directly and without a differential.
