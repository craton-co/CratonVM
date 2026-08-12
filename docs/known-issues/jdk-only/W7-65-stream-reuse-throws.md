# W7-65 — `stream.reuseThrows`: modelling `AbstractPipeline.linkedOrConsumed`

**Date:** 2026-08-12
**Branch:** `fix/stream-reuse-throws-20260812`
**Status:** fixed for reference streams; a named residual set is left open on purpose.

This was the last divergence in the shadow differential. Measured on the
repaired runner (`probes/shadow-differential.ps1`, one class file, stdout and
stderr kept apart):

```
hotspot  stdout 864 lines   cratonvm stdout 864 lines
=== divergent observables: 1 ===
< stream.reuseThrows=java.lang.IllegalStateException
> stream.reuseThrows=no-throw
```

The probe row is `s = src.stream(); s.count(); s.count();` — two terminal
operations on one stream.

It had been declined twice, for a reason that is correct and stays correct:

> a flag set once too often turns a working stream into a throw on the most
> pervasive path in the SB/Tomcat arms

That asymmetry drove every judgement call below. A missed set is a residual
divergence — cheap, and visible in the differential. A spurious set breaks
working pipelines across Spring Boot and Tomcat, and would show up in neither
the differential nor the 70-vector corpus, because both exercise streams
lightly.

---

## 1. What the JDK actually does

Read from `jdk-25.0.3.9-hotspot/lib/src.zip`,
`java/util/stream/AbstractPipeline.java`. Not reconstructed from memory.

`private boolean linkedOrConsumed` (line 135). Two message constants:
`MSG_STREAM_LINKED = "stream has already been operated upon or closed"` and
`MSG_CONSUMED = "source already consumed or closed"`.

**Eight sites SET the flag:**

| # | site | what reaches it |
|---|---|---|
| 1 | `AbstractPipeline(previousStage, opFlags)` — sets on `previousStage` | every intermediate op |
| 2 | `AbstractPipeline(prevPrev, previousStage, opFlags)` — sets on `previousStage` | stage replacement (`GathererOp`) |
| 3 | `linkOrConsume()` | `GathererOp` (two call sites) |
| 4 | `evaluate(TerminalOp)` | every terminal op |
| 5 | `evaluateToArrayNode(generator)` | `toArray` |
| 6 | `sourceStageSpliterator()` | source-stage spliterator handoff |
| 7 | `close()` | sets **unconditionally**, with no preceding check |
| 8 | `spliterator()` | `spliterator()`, and `iterator()` via `Spliterators.iterator(spliterator())` |

**One site CHECKS but does not set:** `onClose(Runnable)` (line 362) throws if
the stage is already linked, then registers the handler without marking.

**Sites that are neither:** `sequential()`, `parallel()`, `unordered()`,
`isParallel()`, `getStreamFlags()`, `hasAnyStateful()`,
`isShortCircuitingPipeline()`. `sourceSpliterator(int)` throws `MSG_CONSUMED`
when the source is spent but never touches `linkedOrConsumed`.

Every expected value in `probes/StreamReuseThrowsProbe.expected.txt` was
measured by running the probe on HotSpot 25 (Eclipse Adoptium
jdk-25.0.3.9-hotspot). Two of those measurements were not what a reading of the
source alone would have predicted, which is why they were measured:
`reuse.onCloseAfterConsume` **does** throw (the check-only site is still a
throw), and `reuse.closeTwice` / `reuse.consumeThenClose` do **not** (`close()`
never checks).

---

## 2. Does the real `AbstractPipeline` bytecode run? No.

This was the question that decided the size of the fix, and the answer is that
none of the eight sites above is executing.

`java.util.stream.Stream` is an **interface**, and CratonVM allocates an object
whose class *is* that interface, with ad-hoc slots:

```
native-collections/src/lib.rs :: make_stream
    try_alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS)
```

`try_alloc_synthetic` resolves the real interface (so this is not a fabricated
class and strict mode does not refuse it) and then allocates `N` untyped slots
on it. Layout:

| slot | meaning |
|------|---------|
| 0 | `Object[]` element snapshot |
| 1 | `Object[]` of `BaseStream.onClose` runnables |
| 2 | undrained lazy source `Spliterator` |
| 3 | deferred-op chain (`cratonvm/stream/LazyOp[]`) |
| 4 | **new in W7-65** — linked-or-consumed |

So there is no `linkedOrConsumed` field waiting to be consulted. The state had
to be added.

**Which modes this applies to.** `register_collections_natives` — which owns
`register_stream_natives` — runs in the **default `--real-jdk` (Compatible)
arm** and in `--jdk-only`, and it runs *last* in `vm_init`'s real-JDK sequence.
It is not the synthetic-only registrar: `native-builtins`'
`register_phase56_*` / `register_p64_stream_modern` live under
`register_synthetic_overrides`, which is a no-op shim when the `synthetic-jdk`
feature is off. Registration is last-write-wins, and the two triples this
record turns on — `("java/util/stream/Stream","count","()J")` and
`("java/util/stream/Stream","toList","()Ljava/util/List;")` — were checked
against every registrar in the workspace: `count` has exactly one registration
anywhere; `toList` has three, and the native-collections one wins in
`--real-jdk` because it registers last (the `phases_early` one is earlier, the
`phases_late` one is synthetic-only). The fix is therefore live on the winning
registrar in **Compatible and `--jdk-only` alike**. No `NativeKind` /
`set_category` block boundary was touched — this change adds no registration.

**Compatible mode is contractually frozen except for genuine HotSpot-parity bug
fixes. This is parity**: HotSpot raises
`IllegalStateException("stream has already been operated upon or closed")` and
CratonVM did not. Per change:

| change | mode(s) |
|--------|---------|
| slot 4 + check-and-set in `stream_elements` | Compatible **and** `--jdk-only` (one funnel, both arms) |
| `STREAM_NUM_FIELDS` 2 → 5, `STREAM_NUM_FIELDS_LAZY` 4 → 5 | both |
| `collect_min_by_max_by_yield_optional` fixture takes a fresh stage per iteration | test only |
| `probes/StreamReuseThrowsProbe*` | probe only |

The two modes reach the *intermediate* ops differently and that matters for the
residuals — see §5.

---

## 3. The funnel census

`native-collections/src/lib.rs` is the file the earlier records mean by "99 call
sites". That figure is `grep -c "stream_elements(ctx"`, which is a substring
match over three distinct functions:

| spelling | sites |
|----------|-------|
| `stream_elements(ctx, …)` | 69 |
| `int_stream_elements(ctx, …)` — infallible wrapper, delegates to the above | 25 |
| `box_primitive_stream_elements(ctx, …)` — unrelated, boxes a `Vec<Value>` | 5 |
| **total matching the substring** | **99** |

Adding `stream_elements_mut(ctx, …)` (8, a one-line alias for the same
function) gives **77 direct callers of the funnel plus 25 through the primitive
wrapper**.

They all bottom out in ONE function, `stream_elements`. That is the finding
that turns this from a 99-site change into a 1-site change: CratonVM's
synthetic stream is an element *snapshot*, and **every** operation that links or
consumes one — intermediate and terminal alike — reads that snapshot through
`stream_elements`. The JDK's eight set-sites collapse onto that single funnel.

### 3.1 Classification of the sites the fix is live at

Receiver is the reference `java/util/stream/Stream`: **35 sites.**

| class | count | sites |
|-------|-------|-------|
| **links a stage** (JDK site 1) | **15** | `filter`, `map`, `flatMap`(receiver), `sorted`, `sorted(Comparator)`, `distinct`, `limit`, `skip`, `peek`, `mapToInt`, and five inline registrar closures — `mapToLong`, `mapToDouble`, `flatMapToInt`, `flatMapToLong`, `flatMapToDouble` |
| **consumes** (JDK sites 4/5/8) | **20** | `forEach`, `count`, `iterator`, `toArray`, `toArray(IntFunction)`, `toList`, `findFirst`/`findAny`, `anyMatch`, `allMatch`, `noneMatch`, `reduce`×3, `min`, `max`, `collect`, `collect(3-arg)`, `spliterator`, `flatMap`(the mapper's inner stream), `Stream.concat` operands (`stream_elements_concat_bounded`) |
| **neither** | **0** | — |

Receiver is a primitive stream (`IntStream`/`LongStream`/`DoubleStream`): **40
direct sites plus the 25 through `int_stream_elements`.** Deliberately left —
see §5.

Helpers that are call sites only because they delegate (`stream_elements_mut`,
`int_stream_elements`): **2**.

### 3.2 Sites that are NOT the funnel, and were checked

* `native_stream_close` — runs the close handlers off slot 1, never reads the
  snapshot. Matches the JDK in the direction that matters: `close()` after a
  consume must not throw, and it does not.
* `native_stream_on_close` — reads slot 1 only.
* `stream_defer_op` — the lazy intermediate-op path; shares the upstream's slot
  0 without reading it.
* `stream_source_elems`, `stream_pull_internal`, `stream_apply_chain_full` —
  the lazy pull machinery reads slot 0 **directly**, so it never re-enters the
  funnel. This is what makes the funnel safe to guard: there is no recursive
  path on which one operation could reach it twice for the same receiver.

That last point was verified mechanically, not by reading: no function in the
file contains two `stream_elements` calls on the same receiver expression. The
only apparent hit — five in `register_stream_natives` — is five separate inline
closures, one operation each.

---

## 4. The fix

One check-and-set, at the top of `stream_elements`, before any work:

```rust
let linked = if class_name == "java/util/stream/Stream" {
    stream_link_or_consume(ctx, stream)
} else {
    Ok(())
};
```

`stream_link_or_consume` is the JDK's `linkOrConsume()`: throw
`IllegalStateException(MSG_STREAM_LINKED)` if slot 4 is already `Int(1)`,
otherwise set it.

**Sites the flag is set at: 1** (covering the 35 classified above).
**Sites deliberately left: everything in §5.**

Three details carry the under-set bias:

1. **Only the exact `Value::Int(1)` reads as linked.** A slot that was never
   written — the allocator zeroes, and `make_stream` writes `Int(0)`
   explicitly — can never be mistaken for a used stream.
2. **Every read and write guards on `object_num_fields > 4`.** A stream with a
   shorter layout is never marked and never refused.
3. **Reference streams only**, by exact class name.

### 4.1 Why the layout grew, and what it does not disturb

`STREAM_NUM_FIELDS` went 2 → 5 and `STREAM_NUM_FIELDS_LAZY` 4 → 5. That makes
slots 2 and 3 *readable* on `make_stream` streams for the first time.
`Heap::try_alloc_object` allocates through `try_alloc_zeroed`, so an unwritten
slot reads back as the zero `Value` and both `stream_lazy_spliterator` (wants
`Object(Some(_))` in slot 2) and `stream_has_chain` (same, slot 3) still answer
"no".

The streams that do **not** grow, and are therefore untouched by this record:

| minted by | slots | linked? |
|-----------|-------|---------|
| `service_loader.rs :: StreamSupport.stream(realSpliterator, false)` | 3 | never |
| `service_loader.rs :: alloc_synthetic_stream` | 1 | never |
| twelve `try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)` sites in `native-builtins` (`Files.list`/`Files.walk` shims, `reflect_invoke`, `phases_late`) | 1 | never |
| in-tree fixtures in `vm/src/vm/tests.rs` (six `alloc_receiver(…, 1)`) | 1 | never |

That first row is the significant one: the `StreamSupport.stream(spliterator,
false)` path — Hibernate's `getResultStream()`, Spring's iteration shims,
`ServiceLoader` — carries three slots and is entirely outside the change.

---

## 5. What was deliberately left, and why

Each of these is a residual divergence taken knowingly, in preference to a
guess that could fire spuriously.

1. **Primitive streams (`IntStream`/`LongStream`/`DoubleStream`).** 25 of their
   call sites go through `int_stream_elements`, which ends
   `.unwrap_or_default()`. A throw raised there would be **swallowed** and the
   stream would silently degrade to empty — strictly worse than the divergence
   it would close. Closing this properly means making those 25 sites fallible,
   which is a separate change with its own review surface.
   *Cost:* `reuse.intStreamTwice` / `longStreamTwice` / `doubleStreamTwice` stay
   `no-throw`.

2. **The deferred intermediate op (`stream_defer_op`), in Compatible mode
   only.** The lazy pipeline is ON by default, so `s.filter(f)` records an op on
   a new stage and never reads `s`'s snapshot — `s` is not marked. Marking there
   would be a *second, independent* set site sitting on the single most
   pervasive path in the Spring Boot and Tomcat arms, and it is exactly the
   shape the two earlier declines were about. Under `--jdk-only` this residual
   does not exist: `cratonvm/stream/LazyOp` is a fabricated stub that strict
   correctly refuses, `stream_defer_op` returns `Ok(None)`, the caller falls
   back to its eager body, and the intermediate op reaches the funnel like any
   other.
   *Cost, Compatible only:* `reuse.linkThenLink`, `reuse.linkThenTerminal` and
   `reuse.terminalThenLink` may stay `no-throw` where the op deferred
   (peek/map/filter/limit/skip/flatMap). The eager intermediate ops — `sorted`,
   `distinct`, `mapToInt`, `mapTo*`, `flatMapTo*` — mark in both modes.

3. **`close()` (JDK site 7).** The JDK sets unconditionally there. Ours does
   not, because I could not rule out, without a build, a VM-internal path that
   closes a synthetic stream earlier than HotSpot would; a mark on such a path
   breaks the *next* legitimate operation. Three lines whenever someone can
   measure it.
   *Cost:* `reuse.closeThenCount` stays `no-throw`.

4. **`onClose()` (the check-only site).** Adding a check there can only ever
   produce throws, never prevent one, so it is off the same reasoning.
   *Cost:* `reuse.onCloseAfterConsume` stays `no-throw`.

5. **The stage-replacement constructor and `linkOrConsume` (JDK sites 2/3).**
   Both exist for `GathererOp`, which CratonVM does not model.

6. **Short-layout streams** — the four rows in §4.1.

---

## 6. Proving the RED, and proving the absence of the over-correction

`probes/StreamReuseThrowsProbe.java`, with
`probes/StreamReuseThrowsProbe.expected.txt` measured on HotSpot 25.

This campaign has repeatedly produced tests that could not fail — four separate
lanes today each wrote an assertion whose truth did not depend on the code under
test. The concrete defence here is that the probe is **two-sided by
construction**, and the two sides are the same pipelines:

* **Section B (`ordinary.*`, 48 rows)** — a wide sample of single-use pipelines
  that must each produce a value: `filter`/`map`/`flatMap`/`sorted`/`distinct`/
  `limit`/`skip`/`peek`, an eight-op chain, all three primitive streams,
  `boxed`, `mapToObj`, `asLongStream`/`asDoubleStream`, five collectors plus the
  3-arg `collect`, `forEach`, `reduce`×2, `anyMatch`/`allMatch`/`noneMatch`,
  `min`/`max`, `iterator`, `spliterator`, `toArray` both forms, parallel and
  `sequential()`-after-`parallel()` and `unordered()`, `Stream.iterate`,
  `Stream.generate`, `Stream.concat`, `Arrays.stream`, a stream consumed inside
  try-with-resources with a close handler, and — the row the JDK explicitly
  permits — two and three **fresh** streams taken off one source object.
* **Section C (`mutation.*`, 16 rows)** — section B's shapes with exactly one
  extra operation injected on an intermediate stage that section B leaves
  alone. Every row must throw.

Why that makes section B's greens load-bearing: *section C is what a build with
no flag at all fails.* A green on B alone proves nothing (the unfixed VM was
green on B). A green on C alone would be satisfied by a flag set on every
stream at birth — which fails B. Only a flag set exactly once, on exactly the
receiver, is green on both. Concretely, of the over-set shapes:

| over-set shape | which row catches it |
|----------------|----------------------|
| set at allocation | every `ordinary.*` row, and the Rust `a_consumed_stream_refuses_a_second_operation` over-set half |
| set twice within one operation | `ordinary.countAfterOps` and the Rust test's first `count` |
| set on the RESULT of an op as well as the receiver | `ordinary.longChainEightOps`, `ordinary.streamHeldInLocal`, and every other multi-op row in B |
| set on a *sibling* stream from the same source | `ordinary.twoPipelinesOneSource`, `ordinary.threePipelinesOneSource` |
| never set | all 16 `mutation.*` rows |

The probe self-checks: `SELFCHECK.ordinaryFailures` must be 0 and
`SELFCHECK.mutationsThatThrew` must be `16/16`; `SELFCHECK.verdict` prints
`PASS` only when both hold. HotSpot 25 measures `PASS`.

There is also a Rust unit test in `native-collections/src/lib.rs`
(`a_consumed_stream_refuses_a_second_operation`,
`a_short_layout_stream_is_never_reported_as_linked`) covering the
set-at-allocation and set-twice shapes plus the short-layout exemption, and
asserting HotSpot's **message text**, not just the class. It does not cover the
"marks the result as well as the receiver" shape, because the mock cannot
dispatch an intermediate op's lambda — section B is the instrument for that, and
the record says so rather than letting the Rust test look more complete than it
is.

### 6.1 An existing test that was asserting an illegal reuse

`collect_min_by_max_by_yield_optional` collected **twice from one stream
object**, which `AbstractPipeline` has always rejected. It now takes a fresh
stage per loop iteration. The assertion it makes — that `minBy`/`maxBy` answer
an `Optional` — is unchanged and is still made twice. Nothing was weakened; an
illegal fixture was made legal.

---

## 7. Blast radius for the Spring / Tomcat arms

**The population that can now throw** is exactly: streams minted by
`native-collections` with the 5-slot layout — `Collection.stream()` on an
intercepted collection, `Stream.of`, `Stream.empty`, `Arrays.stream`,
`IntStream.boxed`, the keyset/values views, and every derived stage an eager
intermediate op produces. Within that population a stream is marked **exactly
once, at one funnel, by the operation that reads its snapshot**.

**What cannot throw:**

* every stream with fewer than 5 slots — including the whole
  `StreamSupport.stream(realSpliterator, false)` family (Hibernate
  `getResultStream`, `ServiceLoader`, the `Files.list`/`Files.walk` shims);
* every primitive stream;
* every real JDK `ReferencePipeline` reaching `stream_elements` through the
  `toArray()` branch — those carry the JDK's own flag and are untouched;
* `close()` and `onClose()`, so try-with-resources over a consumed stream is
  unaffected;
* an intermediate op that deferred under the default lazy pipeline.

**The residual risk, stated plainly.** A false throw needs a synthetic
reference stream whose snapshot is read twice. Application code cannot do that
and still run on HotSpot, which is the strongest evidence available for the
Spring Boot and Tomcat arms: they pass on HotSpot, so they do not reuse a
stream. The exposure is therefore *VM-internal* double consumption. Three
things were checked for it: no function in the file reads the same receiver
through the funnel twice; the lazy pull machinery reads slot 0 directly rather
than re-entering the funnel; and no native anywhere in the workspace invokes a
`java.util.stream.Stream` terminal on a receiver it did not just mint. What is
**not** ruled out without a run is a JIT deopt-and-retry that re-executes a
native stream call — if such a path exists, this change turns it from silently
doing the work twice into a throw. A full Spring Boot / Tomcat arm is the
instrument for that, and it has not been run on this branch.

---

## 8. Files

| file | change |
|------|--------|
| `native-collections/src/lib.rs` | slot 4 + `STREAM_LINKED_MSG` + `stream_is_linked` / `stream_mark_linked` / `stream_link_or_consume`; one call in `stream_elements`; `STREAM_NUM_FIELDS` 2→5, `STREAM_NUM_FIELDS_LAZY` 4→5; explicit slot-4 init in `make_stream`; fresh stage per iteration in `collect_min_by_max_by_yield_optional`; two new tests |
| `probes/StreamReuseThrowsProbe.java` | new, two-sided |
| `probes/StreamReuseThrowsProbe.expected.txt` | new, measured on HotSpot 25 |
| `docs/known-issues/jdk-only/README.md` | the `W7-36` residual row updated |

## 9. What to run

```
javac -d out probes/StreamReuseThrowsProbe.java
java -cp out StreamReuseThrowsProbe                     # PASS (measured)
cratonvm --real-jdk -cp out StreamReuseThrowsProbe      # expect PASS + §5 residuals
cratonvm --jdk-only -cp out StreamReuseThrowsProbe      # expect PASS, fewer residuals
.\probes\shadow-differential.ps1 -Cratonvm .\target\release\cratonvm.exe -Java "…\java.exe"
cargo test -p cratonvm-native-collections
```

The differential should report `divergent observables: 0`. If it does not, read
`SELFCHECK.verdict` in the new probe first: a `FAIL` with
`ordinaryFailures > 0` is an over-set and is the serious outcome, whatever the
differential says.
