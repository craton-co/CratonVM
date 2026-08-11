# W7-2 — `IntStream.summaryStatistics()` killed the run because the whole file
# that implements it is compiled out of the default binary

Status: **fix written (unbuilt, unmeasured, and PARTLY UNWIRED)**. Wave 7, lane
W7-2. Takes the fourth family of
`W7-1-treemap-views-and-iterator-remove-contract.md` (status OPEN); the other
three families — TreeMap views, `Iterator.remove`, `String.format` floats /
`StringBuilder.delete` — belong to other lanes and are untouched here.

Nothing in this lane has been built or run. Every "HotSpot 25 prints …" below is
measured against `Eclipse Adoptium jdk-25.0.3.9` on this host; every "CratonVM
does …" is read off the source, not observed.

---

## 1. What actually throws

The parent filing measured:

| observable | HotSpot 25 | CratonVM `--real-jdk` |
|---|---|---|
| `IntStream.rangeClosed(1,5).summaryStatistics()` | prints the stats | kills the run — `SECTION-DIED.streamsSurface` |

It is the family this campaign has now seen five times: **`AbstractMethodError:
… has no Code attribute` on a JDK interface, meaning a missing registration.**
What is new is *why* it is missing, and it is not an oversight in a list.

Three facts compose:

1. `IntStream.rangeClosed(int,int)` is intercepted. `native_int_stream_range_
   closed` (`native-collections/src/lib.rs`, reached from
   `register_collections_natives`, which runs in **both** modes) answers with
   `make_int_stream`, which allocates through
   `try_alloc_synthetic(ctx, "java/util/stream/IntStream", …)`. The receiver's
   runtime class is therefore **the interface itself** — `is_synthetic_stream`
   in that same file exists to recognise exactly this. (`getClass()` does not
   admit it: `native-builtins/src/lib.rs` maps the name to
   `java/util/stream/IntPipeline$Head` for display.)

2. Nothing live registers `summaryStatistics` on that interface. Extracted from
   the three registrars mechanically — `register_int_stream_natives` (32
   triples), `register_long_stream_natives` (28), `register_double_stream_
   natives` (22) — the name does not appear once.

3. The registration that *does* exist —
   `native-builtins/src/phases_late/streams.rs`, `register_phase56_stream_extras`
   — **is not in the shipping binary at all.** Its only non-test caller is
   `lib.rs::register_synthetic_overrides`, which is `#[cfg(feature =
   "synthetic-jdk")]`; `synthetic-jdk` is deliberately not in `vm/Cargo.toml`'s
   default feature set, and `vm/src/native/builtins.rs` supplies a **no-op shim**
   under `#[cfg(not(feature = "synthetic-jdk"))]`. So under `--real-jdk` the
   whole of phase 56 registers nothing.

So the invokeinterface resolves to `IntStream.summaryStatistics()`'s own
declaration, which is abstract, and the section dies.

This is not a new diagnosis, it is a **recurring** one: native-collections'
`LongStream.mapToObj` registration already carries the comment *"the
synthetic-jdk-only `register_phase56_stream_extras` registration is compiled out
of the real-JDK CLI"*, and `native_int_stream_find_first` carries *"synthetic
IntStreams … lacked findFirst/findAny returning OptionalInt -> abstract-method
(no Code) -> AbstractMethodError"*. Each was fixed for the one member a
workload happened to reach. The file that holds the *complete* implementations
has been unreachable the entire time, and each of those fixes copied one method
out of it by hand.

## 2. How the surface was enumerated

Not from the probe, and not from the source. The oracle is the JDK 25 image on
this host:

```sh
javap java.util.stream.IntStream java.util.stream.LongStream \
      java.util.stream.DoubleStream java.util.stream.BaseStream
javap -p java.util.IntSummaryStatistics java.util.LongSummaryStatistics \
         java.util.DoubleSummaryStatistics
```

and the CratonVM side by extracting every `r.register(c, "name", "desc")` triple
out of the three native-collections registrars, so the comparison is
declaration-set against registration-set rather than against whatever a probe
touched. Behavioural details (exception messages, `toString` rendering, NaN and
compensated-sum semantics) were measured by running the real methods on HotSpot
25 — see §5.

## 3. The gap, per interface

Registered elsewhere and live in real-JDK mode, so not gaps:
`sequential`/`parallel`/`unordered`/`onClose`/`isParallel` come from
`native-builtins/src/streams.rs::register_basestream_mode_overrides`, which
`register_annotation_overrides` pulls into the essentials path for exactly this
reason. The static factories (`empty`, `builder`, `iterate`, `generate`,
`concat`, the varargs `of`) are static interface methods **with** Code, so they
run real JDK bytecode.

Everything below is an abstract declaration with no live registration — i.e. an
`AbstractMethodError` waiting for a caller.

| member | IntStream | LongStream | DoubleStream | this lane |
|---|---|---|---|---|
| `summaryStatistics()` | missing | missing | missing | **fixed (unwired)** |
| `forEachOrdered(…)` | missing | missing | missing | **fixed (unwired)** |
| `findFirst()` / `findAny()` | present | missing | missing | not fixed |
| `anyMatch(…)` | present | present | **missing** | not fixed |
| `reduce(seed,op)` / `reduce(op)` | present | present | **missing** | not fixed |
| `sorted()` | present | missing | missing | not fixed |
| `distinct()` | missing | missing | missing | not fixed |
| `spliterator()` (`Spliterator$OfInt/OfLong/OfDouble`) | missing | missing | missing | not fixed |
| `iterator()` (`PrimitiveIterator$OfX`) | present | present | present | — |
| `iterator()` (`java.util.Iterator` bridge) | answers EMPTY | answers EMPTY | answers EMPTY | not fixed |

Two consequences worth stating because they do not look like members of the
table:

* **`takeWhile` / `dropWhile` / `mapMulti` are default methods with Code**, but
  the JDK's implementations drive `this.spliterator()`. With `spliterator()`
  abstract-and-unregistered they die the same death as `summaryStatistics` did,
  one frame deeper. Same for the static `IntStream.concat(a,b)`, whose real
  body asks both arguments for their spliterators.
* The `iterator()Ljava/util/Iterator;` bridge is registered
  (`register_basestream_mode_overrides`) but bound to
  `native_stream_empty_iterator`, and its own comment says the empty answer was
  chosen for a receiver with "no elements buffered". On a synthetic *primitive*
  stream that does hold elements — `IntStream.range(0,5)` — it silently answers
  an empty iterator. That is the parent filing's "an empty view reads as a pass
  everywhere a caller only iterates", in a second place.

## 4. The `*SummaryStatistics` family

Same shape one level down. `javap -p` (JDK 25) against what
`register_phase56_summary_stats` registered:

| member | declared | was registered |
|---|---|---|
| `<init>()` | yes | yes |
| `<init>(long,int,int,long)` / `(long,long,long,long)` / `(long,double,double,double)` | yes | **no** |
| `accept(int)` / `accept(long)` / `accept(double)` | yes | Long's `accept(int)` **no** |
| `combine(…)` | yes | **no** |
| `getCount`/`getSum`/`getMin`/`getMax`/`getAverage` | yes | yes |
| `toString()` | yes | yes, but wrong (§5) |

`combine` is not an exotic corner: `IntPipeline.summaryStatistics()` is
`collect(IntSummaryStatistics::new, IntSummaryStatistics::accept,
IntSummaryStatistics::combine)`, and every `Collectors.summarizingInt` merge
goes through it. Five of the eight declared members were covered; the three that
were not are the ones no probe had reached — the FFM carrier finding
(fixed-bugs/ffm-memorysegment-set-carriers-FIXED-20260810.md, four of nine
carriers) and W6-7 (`ForkJoinTask.quietly*`, zero of six) are the same finding.

### 4.1 A layout hazard the moment `summaryStatistics` becomes live

`javap -p` on the three classes:

```
java.util.IntSummaryStatistics     count, sum, min, max                   (4)
java.util.LongSummaryStatistics    count, sum, min, max                   (4)
java.util.DoubleSummaryStatistics  count, sum, sumCompensation,
                                   simpleSum, min, max                    (6)
```

CratonVM's shared synthetic layout is `count=0, sum=1, min=2, max=3`, and the
terminals wrote it unconditionally. `try_alloc_concurrent_synthetic` resolves
the class by NAME and clamps the slot count UP to the real class's field count
while keeping the REAL class id — so under `--real-jdk` those writes land on a
real six-field object, putting `min` into `sumCompensation` and `max` into
`simpleSum`. The real `getMin()`/`getMax()` read slots 4/5 and would have
answered `0.0`; the real `getSum()` is `sum - sumCompensation` and would have
answered `sum - min`. Three wrong numbers, no error anywhere. That defect was
latent only because the terminal was unreachable — fixing the reachability
without fixing the layout would have converted a loud death into a quiet lie.
This is the ctor-signature-is-not-field-layout rule again, and the discriminator
has to be the allocated object's own field count, because which class was loaded
is a run-time fact.

## 5. Behaviour measured on HotSpot 25, and what it corrected

All from `java SS.java` against `jdk-25.0.3.9` on this host.

* **`toString` uses `%f`.** The JDK format strings are
  `"%s{count=%d, sum=%d, min=%d, average=%f, max=%d}"` (int/long) and
  `"%s{count=%d, sum=%f, min=%f, average=%f, max=%f}"` (double). Measured:
  `IntSummaryStatistics{count=5, sum=15, min=1, average=3.000000, max=5}` and
  `DoubleSummaryStatistics{count=0, sum=0.000000, min=Infinity, average=0.000000, max=-Infinity}`.
  CratonVM's three `toString`s interpolated the double with Rust's `{}` —
  `average=3` and `min=inf`. Both fixed.
  KNOWN GAP, deliberately left: the JDK's `%f` is locale-sensitive (the same
  calls print `3,000000` under this host's default locale) and rounds HALF_UP
  where Rust's `{:.6}` rounds half-to-even. CratonVM renders the C/en form
  unconditionally.
* **`Math.min`/`Math.max`, not `f64::min`/`max`.** Measured:
  `DoubleStream.of(1.0, Double.NaN, 3.0).summaryStatistics()` →
  `count=3, sum=NaN, min=NaN, average=NaN, max=NaN`. Rust's `f64::min` is IEEE
  `minNum` and returns the *non*-NaN operand, so the old fold would have
  answered `min=1.0, max=3.0` beside a NaN sum — three fields that cannot have
  come from the same data. Java also orders `-0.0` below `+0.0`
  (`DoubleStream.of(-0.0, 0.0)` → `min=-0.000000`), which `<`/`>` cannot see.
* **The double sum is compensated.** Measured:
  `DoubleStream.of(1e16, 1.0, -1e16).summaryStatistics().getSum()` → `0.0`.
  A naive `sum += d` gives `2.0`. The JDK keeps a Kahan compensation term plus a
  `simpleSum` shadow that `getSum()` falls back to when the compensated total is
  a spurious NaN from same-signed infinities.
* **The 4-arg constructors validate.** Measured:
  `new IntSummaryStatistics(-1L, …)` → `IllegalArgumentException: Negative count
  value`; `new IntSummaryStatistics(3L, 9, 1, 12L)` → `Minimum greater than
  maximum`; `new DoubleSummaryStatistics(2L, Double.NaN, 3.0, 4.0)` → `Some, not
  all, of the minimum, maximum, or sum is NaN`. With `count == 0` the JDK skips
  the body entirely, so `new IntSummaryStatistics(0L, 9, 1, 0L)` constructs
  cleanly and reports the identity defaults `min=2147483647, max=-2147483648`.
* **Empty statistics report identities**, which CratonVM already did.

## 6. What landed, in `native-builtins/src/phases_late/streams.rs`

Everything here is source-only. **Nothing was built and nothing was run.**

1. `register_phase56_primitive_stream_terminals` — new registrar holding
   `summaryStatistics` and `forEachOrdered` for all three primitive streams
   (`forEachOrdered` existed for `IntStream` only; the other two are new
   natives). Called from `register_phase56_stream_extras` so synthetic mode is
   unchanged, and separable so the real-JDK path can call it — see §7.
   **Terminals only, deliberately:** the intermediate ops in that file build
   results through `p56_build_stream`, which allocates a REFERENCE array, while
   native-collections' `make_int_stream` allocates a primitive one and comments
   that a reference array coerces `Value::Int` to null. Wiring the intermediate
   ops into the real-JDK path would hand the live readers a stream of nulls.
2. `p56_double_stats_store` — the one place that decides which of the two
   `DoubleSummaryStatistics` layouts a receiver has, off the allocated object's
   own field count (§4.1). Writes `sumCompensation = 0` and `simpleSum`
   explicitly on the real shape.
3. `p56_java_math_min` / `p56_java_math_max` — Java `Math` semantics; used by
   the double terminal and by `DoubleSummaryStatistics.accept`, which had the
   Rust semantics.
4. `p56_format_java_f` — Java `%f` rendering, including `Infinity`/`-Infinity`;
   all three `toString`s now use it.
5. Compensated summation in `p56_double_stream_summary_stats`, including
   `getSum()`'s `simpleSum` fallback.
6. All three terminals now take `count` from the values they actually folded
   rather than from `elems.len()`. The old spelling counted every element but
   summed only the correctly-typed ones, so a layout mismatch produced
   `count=5, sum=0` — a wrong answer that reads as a right one instead of as the
   error it is.
7. `combine` for all three classes; the 4-arg constructor for all three, with
   the three measured `IllegalArgumentException` messages and the `count == 0`
   skip; `LongSummaryStatistics.accept(int)` (the class implements `IntConsumer`
   as well as `LongConsumer`).

## 7. Out-of-file patch (not applied)

### 7.1 One line makes the fix live — REQUIRED

Without this, §6 changes nothing under `--real-jdk`: the file is still only
reachable from `register_synthetic_overrides`. The precedent is two lines above
the insertion point, added for the identical reason.

File: `native-builtins/src/reflect_annotations.rs`, in
`register_annotation_overrides` (which `register_essential_natives_with_shims`
calls, i.e. the real-JDK path).

old code:

```rust
    // Predicate's compositional defaults are invokedynamic captures in the
    // real JDK. Register the GC-visible bridge implementations in real-JDK
    // mode as well so field-filter composition does not retain a stale capture
    // receiver through JUnit cleanup.
    crate::phases_late::register_phase56_function_extras(registry);
```

new code:

```rust
    // Predicate's compositional defaults are invokedynamic captures in the
    // real JDK. Register the GC-visible bridge implementations in real-JDK
    // mode as well so field-filter composition does not retain a stale capture
    // receiver through JUnit cleanup.
    crate::phases_late::register_phase56_function_extras(registry);
    // `summaryStatistics` / `forEachOrdered` on the three primitive streams.
    // Same reasoning as `register_stream_overrides` above: the rest of phase 56
    // is synthetic-jdk-only, and `IntStream.rangeClosed(1,5)
    // .summaryStatistics()` died with `AbstractMethodError: no Code attribute`
    // on the interface because of it (W7-2). TERMINALS ONLY — the intermediate
    // ops in that file mint reference-array streams, which the primitive
    // readers in native-collections read back as nulls.
    crate::phases_late::register_phase56_primitive_stream_terminals(registry);
```

rationale: `register_phase56_primitive_stream_terminals` registers only triples
`register_{int,long,double}_stream_natives` does **not** register (verified by
extracting all 82 of their triples, §2), so running before
`register_collections_natives` — which is last-write-wins over the essentials —
cannot make it inert and cannot overwrite anything.

### 7.2 The rest of §3, in `native-collections/src/lib.rs` — NOT written

Owned by another agent this session, so not attempted, and not sketched as a
patch either: each of these needs that file's own constructors
(`make_optional_int/long/double`, `make_int_stream`, the `PrimitiveIterator$Of*`
builders), and writing them from memory is how a layout gets restated in a
second place. The work is:

* `DoubleStream`: `anyMatch`, both `reduce` overloads, `findFirst`, `findAny` —
  the largest hole, and `anyMatch` is conspicuous because `allMatch` and
  `noneMatch` beside it are both registered.
* `LongStream`: `findFirst`, `findAny`, `sorted`.
* All three: `distinct`, `spliterator()` returning the primitive spliterator.
  `spliterator` is the load-bearing one — `takeWhile`, `dropWhile`, `mapMulti`
  and the static `concat` are default/static methods whose real JDK bodies drive
  it, so one registration closes four more members.
* The `iterator()Ljava/util/Iterator;` bridge should hand back the receiver's
  elements, not `native_stream_empty_iterator`'s empty one, when the receiver is
  a synthetic stream that has any.

## 8. What is unverified

Everything. No build, no test, no VM run — the lane's constraint. Specifically:

* The Rust in §6 has not been compiled. The layout branch in
  `p56_double_stats_store` in particular is reasoned from
  `try_alloc_concurrent_synthetic`'s clamp-up behaviour and `javap -p`, not
  observed.
* §6 is **inert in the default binary** until §7.1 lands. A build that includes
  §6 and not §7.1 will show `IntStream.rangeClosed(1,5).summaryStatistics()`
  dying exactly as before, and that is not evidence against the fix.
* The `--synthetic-jdk` path is the only one §6 changes on its own, and no
  synthetic-mode run was made either.
* Nothing here re-ran `probes/ShadowDifferentialProbe.java`. The section that
  named this defect is `streamsSurface`; a run that reports it as passing is the
  first real evidence, and it needs §7.1.
