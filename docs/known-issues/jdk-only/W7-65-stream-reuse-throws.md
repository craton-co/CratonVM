# W7-65 — `stream.reuseThrows`: modelling `AbstractPipeline.linkedOrConsumed`

**Date:** 2026-08-12
**Branch:** `fix/stream-reuse-throws-20260812`
**Status:** fixed for reference streams; a named residual set is left open on purpose.

> **RE-MEASURED ON A BINARY 2026-08-30 (lane L3), and the residual set is NOT
> the one named here.** This record's update says "Nothing here has been built
> or run"; `apps/probes/StreamReuseProbe` now runs it — 161 rows, every shape,
> both modes, with the exception TYPE and MESSAGE on every row.
>
> **`--jdk-only` is 0-diff on all 161 rows.** Strict drops the synthetic stream
> carriers and runs java.base's own `AbstractPipeline`, which models
> `linkedOrConsumed` correctly by construction. Everything below is compatible
> mode only.
>
> **§5.1 "primitive streams" is too wide.** `Arrays.stream(int[])` and
> `IntStream.of` model reuse correctly today; `IntStream.range`, `mapToInt` and
> `LongStream.range` do not. The split is not primitive-vs-reference.
>
> **§5.6 "short-layout streams" names the wrong property.** `List.of()` and
> `List.of("a")` model reuse correctly; `Collections.singleton`,
> `Collections.emptySet`, `ArrayDeque`, `Arrays.stream(T[])` and
> `parallelStream` do not — and an empty `ArrayList` does. Size is not the
> discriminator.
>
> **What the discriminator actually is: `java/util/stream/Stream` HAS TWO
> PRODUCERS, AT TWO DIFFERENT WIDTHS.** `native-collections` mints it
> `STREAM_NUM_FIELDS` = 5 wide, where slot 4 is the linked-or-consumed flag.
> `native-builtins/src/phases_late/streams.rs` — a second, parallel stream
> implementation ("P56") — mints the SAME class **one field wide** and reads its
> elements from field 0. `stream_mark_linked` opens with "no-op on a stream with
> no slot for it", so on a P56 stream the flag is silently dropped and every
> reuse check passes.
>
> That is `two-producers-of-one-carrier-class-is-a-failure-family`, and it
> explains all three lists above at once: a source reaches the modelled
> behaviour or not according to which crate minted its stream, which correlates
> with neither element type nor size.
>
> **FIXED in the same commit as this note:** the lazy intermediate-stage
> builder now links its source. `stream_link_or_consume`'s doc argues the JDK's
> eight sites collapse onto the one funnel in `stream_elements` because every
> operation reads the snapshot through it — true of every EAGER operation, and
> false of a lazy one, which appends to the op chain and never drains. Two of
> the JDK's eight sites are the intermediate-stage constructors, and
> `stream_make_lazy_derived` is both:
>
> ```text
>   ref filter then filter second     HotSpot IllegalStateException   was ok
>   ref map then map second           HotSpot IllegalStateException   was ok
>   ref parent after child linked     HotSpot IllegalStateException   was 3
> ```
>
> **STILL OPEN, and now precisely bounded:** the 11 rows above whose stream is a
> P56 mint, plus three CLASS-NAME rows — `filter`, `map` and `mapToInt` answer
> `ReferencePipeline$Head` where HotSpot answers `$2`, `$3` and `$4`, because a
> derived stage is minted as a fresh head rather than as a child. Closing the 11
> means giving P56 streams the flag slot AND a check at P56's own operations, in
> a second crate; it is a cross-crate change and it is not attempted here.

> **UPDATED 2026-08-12 — two of the six residuals are closed, and the widest one
> is re-costed.** `close()` (§5.3) and `onClose()` (§5.4) are implemented, both in
> `native-collections/src/lib.rs`, each behind the source census the residual was
> waiting on. Both are now covered by a **scheduled** fixture rather than by
> `probes/`, which `run.sh` never runs: `RJdkCollections.streamReuse()`,
> `JDKONLY_CLASSES`, 61 → **69** checks, two reds and six controls. §5.1
> (primitive streams) and §5.6 (short-layout streams) are unchanged in verdict and
> changed in *cost* — the numbers in this record were the wrong ones and §5.1.1 /
> §5.6.1 carry the corrections. Nothing here has been built or run.

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
| *(§5.6.1, 2026-08-12: this table undercounts — `service_loader.rs` has **four** `java/util/stream/Stream` mints, at `:3051`, `:3303`, `:3371`, `:3415`)* | | |
| twelve `try_alloc_concurrent_synthetic(ctx, "java/util/stream/Stream", 1)` sites in `native-builtins` (`Files.list`/`Files.walk` shims, `reflect_invoke`, `phases_late`) | 1 | never |
| in-tree fixtures in `vm/src/vm/tests.rs` (six `alloc_receiver(…, 1)`) | 1 | never |

That first row is the significant one: the `StreamSupport.stream(spliterator,
false)` path — Hibernate's `getResultStream()`, Spring's iteration shims,
`ServiceLoader` — carries three slots and is entirely outside the change.

---

## 5. What was deliberately left, and why

> **§5.1 MEASURED against HotSpot for the first time, 2026-09-08 — and the
> residual turns out to have a working reference implementation in the same
> binary.**
>
> §5.1's *Cost* line was a prediction. `apps/probes/StreamReuseProbe.java` run
> through the three-arm harness (HotSpot control, `--real-jdk`, `--jdk-only`)
> on Windows / JDK 25 makes it a measurement. **26 rows** diverge, in exactly
> the predicted shape — a consumed stream answers a second terminal operation
> instead of throwing:
>
> ```text
>   61 IntStream.range count second   HotSpot: THREW IllegalStateException   CratonVM: 3
>   65 mapToInt count second          HotSpot: THREW IllegalStateException   CratonVM: 3
>   73 LongStream.range count second  HotSpot: THREW IllegalStateException   CratonVM: 3
>   96 singleton set  98 emptySet  106 ArrayDeque  108 array stream
>  110 parallelStream                                   ... 26 rows in total
> ```
>
> **The new fact is the third arm: `--jdk-only` is BYTE-IDENTICAL to HotSpot on
> all 26.** This record claims strict-mode immunity for residual §5.2 — the
> deferred intermediate op, where `cratonvm/stream/LazyOp` is a stub strict
> refuses — and says nothing either way about §5.1. Measured, strict is clean
> here too.
>
> That changes what a taker has to hand. The oracle for this residual need not
> be HotSpot on another host: **the same binary, the same probe, `--jdk-only`,
> is a 0-diff reference for all 26 rows.** A `--real-jdk` vs `--jdk-only` diff
> on `StreamReuseProbe` is therefore a one-command regression check for the
> conversion §5.1.1(b) describes, and it needs no second VM.
>
> **Not taken here, for this record's own stated reason.** §5.6.1 says *"do not
> take it from a lane that cannot run those arms"*, and the arms are the
> constraint rather than the edit: `apps/spring-boot`, `apps/tomcat` and
> `apps/h2database` are **absent on this host** (they live on azure-host-2).
> The 25-site pin audit in §5.1.1(b) stands exactly as written.

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

### 5.1.1 — 2026-08-12: what the swallow eats today (nothing), and what the
### conversion actually costs (not 25 `?`s)

Verdict unchanged: **still open, still on purpose.** Two corrections to the
reasoning, because both change what a taker should plan for. Re-derived from
source and written into the doc comment on `int_stream_elements` itself, so the
next reader can check it rather than trust it.

**(a) The swallow is currently eating nothing, and "the chain branch is
unreachable" was only one of three reasons.** `stream_elements` has exactly three
error paths and a primitive receiver reaches none of them:

| `stream_elements` error path | why no primitive receiver reaches it |
|---|---|
| `stream_link_or_consume` | gated on `class_name == "java/util/stream/Stream"` |
| `materialize_lazy_stream(..)?` | needs slot 2. Only `StreamSupport.stream(Spliterator,Z)` writes it, and it mints `java/util/stream/Stream`. `StreamSupport.intStream`/`longStream`/`doubleStream` live in `phases_late/streams.rs`, `register_synthetic_overrides`-only — absent from both shipping modes |
| the real-pipeline `toArray()[Ljava/lang/Object;` branch | needs a receiver whose class is not one of the four interface names. Every `native_{int,long,double}_stream_*` native is registered on an INTERFACE, and every native-shadow hierarchy walk in the tree is a *superclass* walk (W7-9 §2), so a real `IntPipeline$Head` resolves its own `Code`. `prim_stream_values` is the function for real primitive pipelines, and it uses `()[I`/`()[J`/`()[D` |

So this residual is a *latent* trap, not a live loss — which is worth stating
precisely, because "25 sites swallow errors" reads as an active defect and the
census that produced it did not say which errors.

**(b) The conversion is not "add `?` at 25 sites", and the difference is the
pin stack.** At least `native_int_stream_peek` holds a live
`ctx.pin_native_root` handle across its `int_stream_elements` call; a bare `?`
there returns without `unpin_native_roots` and leaks a pin. The pin stack is
strictly LIFO per scope and this file has already paid for corrupting it twice
(`stream_apply_chain_full`'s `cceres3` note: a callback pinning into its callee's
scope made `read_value_slice` degrade to stale values, surfacing as
`to_array_gen` canary firings). `stream_match`, two hundred lines away, already
shows the correct form — `match … { Err(e) => { unpin; return Err(e) } }`. So the
taker's unit of work is **check the live pins at each of the 25**, not append a
character, and the compiler cannot help: a leaked pin compiles.

Only once that lands is extending the mark worth doing, and then it is one line:
`if class_name == "java/util/stream/Stream"` becomes
`if is_synthetic_stream(&class_name)`. Two properties make that safe and are
worth recording now so the next pass does not re-derive them: all three
`make_{int,long,double}_stream` mints use `STREAM_NUM_FIELDS` (5), so the width
guard admits them; and `native_int_stream_flat_map` reads its receiver through
`prim_stream_values` (a direct slot-0 read), not through the funnel, so the
mapper's sub-streams cannot double-mark.

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

3. **`close()` (JDK site 7).** ~~The JDK sets unconditionally there. Ours does
   not~~ — **CLOSED 2026-08-12.** The blocker was *"I could not rule out, without
   a build, a VM-internal path that closes a synthetic stream earlier than HotSpot
   would"*. That census is a grep, not a build, and it comes back clean: the only
   `invoke_virtual(_, "close", "()V", _)` calls in `native-collections/src/lib.rs`
   are the two `flatMap` inner-stream closes — the lazy chain's, inside
   `stream_process_chain`, and the eager body's, in `native_stream_flat_map` — and
   **both run after that inner stream's elements have been read**, which is
   precisely what the JDK's own `try (Stream<R> result = mapper.apply(u))` does.
   Neither inner stream is touched again. `native_stream_close` now calls
   `stream_mark_linked` (the unconditional setter, *not*
   `stream_link_or_consume`), **before** running the handlers, matching
   `AbstractPipeline.close()` whose first statement is `linkedOrConsumed = true`
   and whose `closeAction.run()` is last. That ordering is observable twice: a
   handler operating on the stream must see it spent, and a handler that throws
   must still leave it marked.
   *Cost:* none. `reuse.closeThenCount` now throws; `closeTwice` and
   `consumeThenClose` still do not, because `close()` sets without checking.
   *Gate:* `RJdkCollections.streamReuse()` rows 1, 3, 4.

4. **`onClose()` (the check-only site).** ~~Adding a check there can only ever
   produce throws~~ — **CLOSED 2026-08-12**, and the asymmetry that made it a
   residual is the reason it is now safe. `native_stream_on_close` raises
   `IllegalStateException(STREAM_LINKED_MSG)` when `stream_is_linked`, and marks
   nothing. Placement is the whole fix: the check sits **after** the
   `is_synthetic_stream` gate, because slot 4 on a real `ReferencePipeline` is one
   of its own fields and could hold `Int(1)` for reasons of its own — and
   `stream_is_linked`'s own width guard exempts every short-layout stream. Nothing
   else in the file writes slot 4, so the flag can only be there because
   `stream_link_or_consume` put it there.
   *Cost:* none. `reuse.onCloseAfterConsume` now throws.
   *Gate:* `RJdkCollections.streamReuse()` row 2, with row 5 as the
   handler-still-runs control.

5. **The stage-replacement constructor and `linkOrConsume` (JDK sites 2/3).**
   Both exist for `GathererOp`, which CratonVM does not model. Re-checked
   2026-08-12: still true, nothing to do, and nothing to measure — there is no
   `Gatherer` surface in the tree to attach a residual to.

6. **Short-layout streams** — the four rows in §4.1.

### 5.6.1 — 2026-08-12: the short-layout residual is one out-of-lane edit, and
### the record understated which streams it covers

Verdict unchanged: **not taken.** But it is worth being exact, because §7's
"what cannot throw" list reads like a design decision and it is really a
consequence of two `.max()` calls in a file this lane does not own.

`native-builtins/src/service_loader.rs` mints `java/util/stream/Stream` at
**four** `ctx.class_num_total_fields(cid).max(N)` sites, not the two §4.1 lists —
re-counted 2026-08-12, and the miscount matters because raising only the two named
ones leaves half the family exempt and the residual would read as closed:

| site | `N` | arm |
|---|---|---|
| `:3051` | 1 | — |
| `:3303` | 3 | `native_stream_support_stream_from_spliterator`, the REAL-spliterator arm (stashes the source in slot 2) |
| `:3371` | 1 | the same function's synthetic-spliterator arm (snapshot already materialised) |
| `:3415` | 1 | `alloc_synthetic_stream` |

Raising all four to `.max(5)` is the entire mechanical change:
`Heap::try_alloc_object` zeroes, only the exact `Int(1)` reads as linked, so the
new slots need no explicit init. (The two `.max(3)` sites at `:3197` and `:3230`
are `java/util/Spliterator`, a different class — do not touch them.)

Why it is still declined, in the record's own terms rather than as a shrug: this
is the **most pervasive stream path in the SB / Tomcat / Hibernate arms**
(`getResultStream`, `ServiceLoader`, the `Files.list`/`Files.walk` shims), and the
declining argument at the top of this record — *"a flag set once too often turns a
working stream into a throw on the most pervasive path"* — is a description of
exactly this row. Two lines is not the cost; the arm run is. Do not take it from
a lane that cannot run those arms.

One thing the earlier text got wrong and a taker would trip over: §7 lists
*"every real JDK `ReferencePipeline` reaching `stream_elements` through the
`toArray()` branch"* as unable to throw. That is right, but not for the reason the
line implies — those receivers never enter `stream_link_or_consume` at all,
because it is gated on `class_name == "java/util/stream/Stream"` and a real
pipeline's class name is `java/util/stream/ReferencePipeline$…`. The JDK's own
flag is irrelevant to it.

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

Added 2026-08-12 (§5.3 / §5.4, unbuilt):

| file | change |
|------|--------|
| `native-collections/src/lib.rs` | `stream_mark_linked` in `native_stream_close`, ahead of the handlers; the `stream_is_linked` check in `native_stream_on_close`, after the `is_synthetic_stream` gate; the `int_stream_elements` doc comment replaced with the three-path derivation and the pin hazard (§5.1.1) |
| `regression-suite/src/RJdkCollections.java` | `streamReuse()`, 8 checks, 61 → **69**. Two reds (rows 1, 2) and six controls; `JDKONLY_CLASSES`, so it runs on `CRATONVM_ARGS=--jdk-only` and on `SUITE=all` |

**The swallow question, answered for this pass.** Neither new raise is in the
primitive-stream family, so neither can be eaten by `int_stream_elements`'
`.unwrap_or_default()`: `native_stream_close` returns `MethodCallResult` to the
interpreter and marks rather than raising, and `native_stream_on_close`'s
`IllegalStateException` is returned from the native's own `MethodCallResult` — the
25 swallowing sites all call `stream_elements`, which is a different funnel that
neither of these two functions enters.

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

---

## 10. Re-verified 2026-08-12 — every claim holds, and this record's coverage story is the counter-example to the rest of the campaign

Source read only; **no build, no binary, no `cargo`**. Checked because this
record's residual set is unusually specific and specific claims go stale fastest.
Every one of them reproduces:

| claim | where it says | status |
|---|---|---|
| slot 4 + the three helpers | §4, §8 | `STREAM_LINKED_MSG` (`native-collections/src/lib.rs:17646`), `stream_is_linked` (`:17655`), `stream_mark_linked` (`:17663`), `stream_link_or_consume` (`:17689`) |
| `STREAM_NUM_FIELDS` 2 → 5 | §4.1 | `const STREAM_NUM_FIELDS: usize = 5;` (`:17626`) |
| `STREAM_NUM_FIELDS_LAZY` 4 → 5 | §4.1 | `= STREAM_NUM_FIELDS` (`:18056`) — derived rather than duplicated, which is stronger than the record claims and is why the two cannot drift |
| the §5.1.1 extension hook | §5.1.1 | `is_synthetic_stream` (`:19562`) exists, so the one-line widening is available whenever the 25 pin sites are settled |
| **§5.6.1's four `service_loader.rs` mints, and its correction of §4.1's two** | §5.6.1 | **exact.** `java/util/stream/Stream` is minted at `:3051` (`.max(1)`), `:3303` (`.max(3)`), `:3371` (`.max(1)`), `:3415` (`.max(1)`); the two `.max(3)` `java/util/Spliterator` sites §5.6.1 warns not to touch are at `:3197` and `:3230`, exactly as written. Four line numbers and two decoys, all still correct a day later |

**§5.3 and §5.4 are covered by a fixture that actually runs, and that is the
finding worth carrying out of this record.** `RJdkCollections` is in
`JDKONLY_CLASSES` (`regression-suite/run.sh:119`), `streamReuse()` is declared
at `RJdkCollections.java:221`, called from `main` at `:320`, and prints its
`CK RJdkCollections streamReuse=` line at `:278`. So the two residuals this
record closed are **scheduled**, not probe-only.

That is worth stating plainly against the rest of the sweep, because it is the
exception. The campaign's standing note is that `regression-suite/run.sh` names
no path under `probes/` at any `SUITE=` value, so a probe is evidence a human
can run and not evidence the tree defends. Of the four records this lane holds:

| record | its evidence | scheduled? |
|---|---|:-:|
| **W7-65** (this one) | `RJdkCollections.streamReuse()` + two Rust unit tests | **yes** |
| W7-72 | `probes/SscSocketKeysProbe.java`, `probes/FileChannelIsOpenProbe.java` | no — and §7.1 says why: no fixture calls `ServerSocketChannel.socket()` and none can reach a literal `java/nio/channels/FileChannel` |
| W7-88 | `probes/SscSocketOwnerProbe.java`, NO-CHANGE by construction, plus a source ratchet | no — §9 says a Java-visible predicate cannot move for a row that was never in the registry |
| W7-8 | none for the shipping-path rows; §8.6 states the gap | no |

The difference is not diligence. It is that §5.3/§5.4 changed a *Java-visible
predicate on a class an ordinary program touches* — `stream.close()` then
`stream.count()` — while the other three moved behaviour reachable only through
an object the VM has to mint for itself. A record whose fix has no scheduled
witness should say which of those two it is, and this record is the one that can
say "the first".

**Nothing here upgrades §5.1 or §5.6.** Both remain declined for the reasons
given, and §5.6.1's declining argument is the one to re-read before anyone
raises those four `.max()` calls: two lines of edit, an arm run's worth of risk,
on the most pervasive stream path in the Spring Boot / Tomcat / Hibernate arms.
Do not take it from a lane that cannot run those arms.
