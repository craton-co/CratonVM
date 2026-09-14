# W7-2 — `IntStream.summaryStatistics()` killed the run because the whole file
# that implements it is compiled out of the default binary

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — "PARTLY UNWIRED"
> IS STALE. THE WIRING LANDED.** Section 7.1, the one line marked REQUIRED, is
> in the tree:
> `crate::phases_late::register_phase56_primitive_stream_terminals(registry);`
> at `native-builtins/src/reflect_annotations.rs:548`, commit `4752a00a4` —
> landed by the W7-5 lane, not this one, which is why this record never learned
> of it. The section 6 bodies are present too: `p56_int_stream_summary_stats` at
> `native-builtins/src/phases_late/streams.rs:1764`, `p56_double_stats_store` at
> `:1909`, registrar at `:469`, commit `1fcbd9060`.
>
> * **Headline: CLOSED in source, still unverified against a binary.**
> * **Residual: WRITTEN 2026-08-12 except `spliterator()` and the static
>   `concat`.** Section 7.2's `DoubleStream`/`LongStream` holes — `anyMatch`,
>   both `reduce` overloads, `findFirst`/`findAny`, `sorted`, `distinct` — plus
>   `distinct` on `IntStream` are now registered in
>   `native-collections/src/lib.rs`, and the `iterator()Ljava/util/Iterator;`
>   bridge boxes. `spliterator()` is **REFUSED, not deferred**: §9 says why, and
>   it strikes §3's "one registration closes four more members". Source only —
>   nothing built, nothing run.
> * **Cannot adjudicate without a run:** `cargo build --release -p cratonvm-cli`
>   then `cratonvm --real-jdk ... ShadowDifferentialProbe`; the
>   `IntStream.rangeClosed(1,5).summaryStatistics()` row must stop dying.
>
> ## ADJUDICATED BY A RUN, 2026-08-12 — headline CONFIRMED closed; §5's NaN/`-0.0` claim is FALSE, and its cause is one level below this record
>
> The run this record has asked for since it was written. Binary: a
> default-feature release build dated 2026-08-12 15:27; oracle HotSpot 25.0.3+9;
> host default locale `ru_RU`.
>
> **Headline: CLOSED and VERIFIED.**
> `probes/ShadowDifferentialProbe.java` prints
> `stream.summaryStats=IntSummaryStatistics{count=5, sum=15, min=1, average=3.000000, max=5}`
> where it used to print `SECTION-DIED.streamsSurface`. §7.1's wiring is live.
> `regression-suite/src/RJdkViews.java` — the §9.4 cover, scheduled in
> `CORE_CLASSES` — passes **107/107 in all three arms** (HotSpot, `--real-jdk`,
> `--jdk-only`). So §9.1's landed rows and §9.5's boxing fix are verified too.
>
> **§5's second bullet is contradicted by measurement.** It states that
> `Math.min`/`Math.max` semantics were applied so NaN poisons the fold. Measured,
> both modes, on a directly constructed real `java.util.DoubleSummaryStatistics`:
>
> | observable | HotSpot 25 | CratonVM `--real-jdk` | `--jdk-only` |
> |---|---|---|---|
> | `d.accept(1.0); d.accept(NaN); d.accept(3.0)` → `getMin`/`getMax`/`getSum` | `NaN`/`NaN`/`NaN` | **`1.0`/`3.0`/`NaN`** | same |
> | `z.accept(-0.0); z.accept(0.0)` → `getMin` | `-0.0` | **`0.0`** | same |
> | `DoubleStream.of(1.0,NaN,3.0).summaryStatistics().getMin()` | `NaN` | **`1.0`** | same |
>
> `sum=NaN` beside `min=1.0, max=3.0` is, in this record's own words, "three
> fields that cannot all have come from the same data" — the exact wrong answer
> `p56_java_math_min`'s doc comment was written to prevent, still being produced.
>
> **The cause is NOT in this file, and that is the finding.** `p56_java_math_min`
> is correct and is used correctly at every site in
> `native-builtins/src/phases_late/streams.rs`. But the fold that actually runs
> here is the **real JDK bytecode** of `DoubleSummaryStatistics.accept`, because
> `register_phase56_summary_stats` is reachable only from
> `register_phase56_stream_extras` (`streams.rs:29`) — the synthetic-only
> registrar. §7.1 wired the *terminals* into the real path and deliberately left
> the `*SummaryStatistics` surface behind. So the real `accept` runs, and it
> calls `java.lang.Math.min(double,double)` — **which this VM answers wrongly**:
>
> ```text
>                        HotSpot 25    CratonVM (both modes)
> Math.min(1.0, NaN)     NaN           1.0
> Math.max(1.0, NaN)     NaN           1.0
> Math.min(-0.0, 0.0)    -0.0          0.0
> Math.min(1.0f, NaNf)   NaN           1.0
> StrictMath.min(1.0,NaN) NaN          1.0
> StrictMath.min(-0.0,0.0) -0.0        0.0
> ```
>
> `native_math_min_double`/`_max_double`/`_min_float`/`_max_float`
> (`native-builtins/src/lang_math.rs:1410`–`:1482`) use Rust's `f64::min`/`max`,
> which is IEEE `minNum` — the operand-returning form this record's own
> `p56_java_math_min` doc comment names as the trap. `register_math_natives` is
> called for **both** `java/lang/Math` and `java/lang/StrictMath`
> (`native-builtins/src/lib.rs:14627`–`:14628`), so those four bodies are eight
> wrong triples. **The correct helper already exists in this repo and exactly one
> caller uses it** — see NOMINATION 1.
>
> Counted rather than assumed, because a family this shape is where a count goes
> wrong: **ten wrong triples, reached by three different routes.**
>
> * eight from `lang_math.rs`'s four bodies x `Math`/`StrictMath`;
> * **two more with their own bodies** — `java/lang/Double.min/max(DD)D`,
>   registered separately at `native-builtins/src/phases_early.rs:2701`–`:2710`,
>   also spelled `a.min(b)`/`a.max(b)`. Measured: `Double.min(1.0, NaN)` → `1.0`,
>   `Double.min(-0.0, 0.0)` → `0.0`. These are **not** fixed by NOMINATION 1 and
>   need their own edit;
> * `java/lang/Float.min/max(FF)F` is **not registered anywhere** (grepped) — it
>   is real bytecode delegating to `Math.min`, and measures wrong
>   (`Float.min(1.0f, NaN)` → `1.0`) purely by inheritance. It needs no edit of
>   its own, and it is the cheapest witness that this reaches ordinary callers
>   rather than only the stream fold.
>
> **Why nothing flagged it.** `register_math_natives` opens with
> `registry.set_category(NativeKind::Intrinsic)` (`lang_math.rs:20`–`:21`),
> ambient over the whole function. `Intrinsic` is exempt from the shadow
> retirement *and* from the census's `native-shadows-bytecode` kind: a
> `--jdk-only --jdk-only-report` run of a program that calls `Math.min` four
> times contains **zero** `java/lang/Math` rows of any kind (13
> `compatibility-class-requested`, 2 `native-shadows-bytecode`, 1324
> `synthetic-native-registered`; `grep -c java/lang/Math` → 0). This is
> W7-25 §1's species exactly — a native that gives an answer the bytecode would
> not, wearing a category that makes it invisible — and it is the reason a
> reader of `phases_late/streams.rs` alone concludes the defect is fixed.
>
> **§5's first bullet — the `%f` locale gap — is now MEASURED, not predicted.**
> It is the single surviving divergence in the whole 864-line
> `ShadowDifferentialProbe` transcript (W7-1's closing block): HotSpot renders
> `average=3,000000` under this host's `ru_RU` default, `p56_format_java_f`
> renders `3.000000` unconditionally.
>
> **Provenance matters for exactly this row, so it is stated rather than
> glossed.** The binary measured here (15:27) PREDATES commit `67146db71`
> (17:49), which is where the locale lane's no-`Locale` `String.format` fix and
> its vector `RJdkFormatLocale` landed. On the binary measured,
> `String.format("%f", 3.0)` also answered `3.000000` — i.e. the pre-fix
> behaviour, not evidence against that lane. **What is verified at HEAD by
> reading rather than running** is that `p56_format_java_f`
> (`native-builtins/src/phases_late/streams.rs:1698`) is untouched by that
> commit and still ends in `format!("{:.6}", d)`, a Rust-side renderer that
> never calls `String.format`. So the prediction — not the measurement — is that
> at HEAD the two now DISAGREE inside CratonVM about which locale they follow:
> `String.format("%f", 3.0)` follows `Locale.getDefault(FORMAT)` and
> `IntSummaryStatistics.toString()` still does not. First run of a post-`67146db71`
> binary settles it in one line. NOMINATION 2.
>
> The same caveat applies to nothing else in this block: the `Math.min`/`Math.max`
> bodies were last touched at 03:34 on 2026-08-12 and are byte-for-byte at HEAD
> what the 15:27 binary contains — checked, not assumed.
>
> **Coverage gap in §9.4's own vector, named because it is why 107/107 is not a
> discharge of the two rows above.** `RJdkViews.primitiveStreamSurface` asserts
> `emptyStats=2147483647,-2147483648,0.0` — `getMin`/`getMax`/`getSum` **values**.
> It never asserts a `*SummaryStatistics.toString()`, which is the only thing
> `p56_format_java_f` produces, and it never feeds a NaN to `accept`. Both
> defects above are invisible to a green run of it. NOMINATION 3.
>
> **Unchanged and re-confirmed by the same run:** §9.1's third bullet —
> `DoubleStream.min()`/`max()` are still wrong for NaN. Measured:
> `DoubleStream.of(1.0,NaN,3.0).min()` → HotSpot `OptionalDouble[NaN]`,
> CratonVM `OptionalDouble[1.0]`. §9.3's `spliterator()` refusal stands; nothing
> here re-opens it.

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
(ffm-memorysegment-set-carriers-FIXED-20260810.md, four of nine
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

### 7.2 The rest of §3, in `native-collections/src/lib.rs` — WRITTEN 2026-08-12, except `spliterator`

**This section is superseded by §9.** Its work list was right; its closing
claim — that `spliterator` "is the load-bearing one … so one registration closes
four more members" — is struck. Read §9 before acting on anything below.

The original text, kept because the reasoning about *why* it was deferred is
the reasoning §9 acts on:

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

## 9. §7.2 closed out, 2026-08-12 — what landed, and the one that is refused

All in `native-collections/src/lib.rs` (`register_int_stream_natives`,
`register_long_stream_natives`, `register_double_stream_natives`) and
`native-builtins/src/streams.rs`. **Source only: not built, not run.** Every
triple below was checked against
`native-builtins/src/phases_late/streams.rs::register_phase56_stream_extras`
first — none collides, so nothing here silently re-registers or is re-registered,
and `register_collections_natives` runs last in both modes anyway.

### 9.1 Landed

| member | IntStream | LongStream | DoubleStream |
|---|---|---|---|
| `anyMatch` | was present | was present | **added** |
| `reduce(seed,op)` / `reduce(op)` | was present | was present | **added, both** |
| `findFirst()` / `findAny()` | was present | **added** | **added** |
| `sorted()` | was present | **added** | **added** |
| `distinct()` | **added** | **added** | **added** |

Three things worth stating because they are not obvious from the table:

* **`DoubleStream.sorted()` and `.distinct()` cannot be written with `<` and
  `==`.** `Arrays.sort(double[])` is `Double.compare` order and
  `distinct()` is `Double.equals`, and both differ from `f64`'s in the same two
  places: `-0.0` sorts strictly BELOW `+0.0` and compares UNEQUAL to it, and all
  NaNs are one value that sorts last. `f64::total_cmp` is a third order again —
  it puts a negatively-signed NaN below `-inf`. Hence `java_double_compare` and
  `java_double_to_long_bits` (the canonicalising `doubleToLongBits`, not Rust's
  `to_bits`, which is `doubleToRawLongBits`). The two `-0.0` rows in
  `RJdkViews.primitiveStreamSurface` are there to fail the naive version, which
  passes everything else.
* **`DoubleStream.min()`/`max()` still use `f64::min`/`f64::max` and are still
  wrong for NaN** — §5 measured `min=NaN` on HotSpot for
  `DoubleStream.of(1.0, NaN, 3.0)` and fixed the *summary-statistics* fold for
  it; the two stream terminals in `native-collections` were never fixed and are
  not fixed here. Adjacent, unmeasured, stated rather than deferred silently.
* **The vectors have to use a SYNTHETIC stream or they measure the real JDK.**
  `IntStream.of(int...)` and `Arrays.stream` return a real `IntPipeline$Head`
  that implements all of this in its own bytecode. Only `range`/`rangeClosed`
  and the intermediate ops built on them mint the interface-stamped object this
  record is about. `RJdkViews` builds every one of its streams that way and says
  so in place.

### 9.2 The `iterator()Ljava/util/Iterator;` bridge — §3's row was half stale

§3 records this as "answers EMPTY". **It does not, and has not for some time:**
`native_stream_empty_iterator` (`native-builtins/src/streams.rs`) already reads
the receiver's backing array — that was the Hibernate `JoinedList` fix, whose
rationale is written out in the function's doc comment.

What survived is the other half, and it is worse than an empty answer. A
primitive stream's backing store is a primitive `int[]`/`long[]`/`double[]`
(`make_int_stream` allocates one deliberately, because a reference array coerces
`Value::Int` to null). `ServiceLoader$Itr.next` is declared
`()Ljava/lang/Object;`, so those elements went back to the caller as bare
`Value::Int`s — an **untyped word where a reference is declared**, which is
`W7-84-primitive-in-reference-store`'s species, not a wrong value. It is silent
at the source: it misbehaves at the caller's `checkcast` or `intValue()`, a
frame away.

`box_primitive_iterator_source` now boxes through `Integer`/`Long`/`Double`/
`Float`.`valueOf` — but only after a scan finds a primitive, so a REFERENCE
stream (the overwhelmingly common receiver, and the case the function exists
for) keeps its own array and allocates nothing.

**§9.2 fixed a function that does not run on this path. See §9.5.**

### 9.5 The bridge, MEASURED — §9.2 boxed the shadowed half

First section of this record with a VM run behind it. Binary: the frozen
`cratonvm-wave-full.exe`, `--jdk-only`, `--java-home` = jdk-25.0.3.9 on this
host; HotSpot 25 as the oracle, same session.

`RJdkViews.primitiveStreamSurface` fails at line 483:

```
AssertionError: BaseStream.iterator() must yield boxed Integers (element 1 was not an Integer)
```

`seen++` runs before the check, so **"element 1" is the FIRST element, not the
second.** There is no element-0-worked asymmetry to explain: nothing is boxed.

#### What the run shows

A standalone probe (`Object o = it.next()` over
`((BaseStream<?,?>) IntStream.rangeClosed(1,3)).iterator()`):

| observable | CratonVM `--jdk-only` | HotSpot 25 |
|---|---|---|
| `it.getClass().getName()` | `java.util.Arrays$ArrayItr` | `java.util.Spliterators$2Adapter` |
| element count | 3 | 3 |
| `o == null` | **false** | false |
| `o.getClass()` | **NullPointerException** | `java.lang.Integer` |
| `o instanceof Integer` | **false** | true |

`o != null` and `o.getClass()` NPEs on the same word: that is the signature of
an untyped primitive in a reference slot, not of a null. (It is not even
uniformly null-shaped — `ifnonnull` reads `Value::Int(1)` as non-null, so an
element that happened to be `0` would read as null and one that was not would
not. Two different wrong answers from one store.) Across the four stream kinds:

| receiver | n | typed elements |
|---|---|---|
| `IntStream.rangeClosed(1,3)` | 3 | **0** |
| `LongStream.range(1,3)` | 2 | **0** |
| `DoubleStream.of(1.5,2.5)` | 2 | 2 |
| `Stream.of("a","b")` | 2 | 2 |
| `IntStream.rangeClosed(1,3).map(i->i*10)` | 3 | **0** |
| `IntStream.rangeClosed(1,3).boxed()` | 3 | 3 |

#### Why §9.2's fix was invisible

The iterator handed back is `Arrays$ArrayItr`, which is
`native-collections`' `make_iterator_from_array` landing — **not** the
`java/util/ServiceLoader$Itr` that `native_stream_empty_iterator` builds. The
descriptor `iterator()Ljava/util/Iterator;` is registered TWICE:

* `native-builtins/src/streams.rs` → `native_stream_empty_iterator`, on all
  five of `BaseStream`/`Stream`/`IntStream`/`LongStream`/`DoubleStream`;
* `native-collections/src/lib.rs` → `native_stream_iterator`, on `BaseStream`
  and `Stream` only.

`register_collections_natives` runs after `register_builtins`
(`vm/src/vm/vm_init.rs`) and registration is last-writer-wins, so for
`BaseStream` and `Stream` the native-collections one wins — and a call site
whose static type is `BaseStream` (which is how every `IntStream` reaches this
descriptor, since `BaseStream` is where it is declared) lands exactly there.
§9.2 boxed the copy that only serves the three primitive-interface keys.

The last row of the table is the proof the boxing itself is sound:
`boxed()` already routes through `box_primitive_stream_elements`, on the same
synthetic `IntStream` elements, and yields real `java.lang.Integer`s.

#### The fix

`native_stream_iterator` (`native-collections/src/lib.rs`) now runs
`box_primitive_stream_elements` over `stream_elements`' output before storing
into the `Object[]` the iterator reads. Reference elements pass through
untouched and allocate nothing, so the Hibernate `JoinedList` case this function
exists for is unchanged.

Three supporting changes in the same file:

* `box_primitive_stream_elements` is now fallible and delegates each element to
  `box_primitive_result` instead of open-coding three `valueOf` calls. Its old
  `.ok().flatten().unwrap_or(Value::Object(None))` converted a refusal into a
  silent `null` element — the W7-65 shape, in the boxing helper itself. Its four
  call sites all sit in `-> MethodCallResult` functions and take a `?`.
* **Is the new raise reachable? Not on this path.** `box_primitive_result`
  raises only if `Integer/Long/Double/Float.valueOf` fails to return an object
  AND the `try_alloc_synthetic` fallback is refused (which it is, under
  `--jdk-only`). The `boxed()` row above measures `valueOf` succeeding on this
  exact receiver in this exact mode, so the fallback is not entered. It is a
  real raise reserved for an image with no wrapper classes, and it propagates —
  `native_stream_iterator` returns `MethodCallResult` and its caller is the
  interpreter, not one of W7-65's 25 `unwrap_or_default` swallowers (those are
  on `int_stream_elements`, which this path does not use — `stream_elements` is
  the fallible one and already carries a `?`).
* `box_primitive_result` grew a `Float` arm. There is no `FloatStream`, but a
  `Value::Float` reaching a reference-typed surface is the same untyped word,
  and the arm was simply missing beside the other three.

#### Not done

`DoubleStream` already answered typed elements before this change, and no run
established which mechanism did that. `spliterator()` is still refused (§9.3),
so `takeWhile`/`dropWhile`/`mapMulti`/`concat` are unchanged. The fix is source
only — this lane could run the frozen binary but not rebuild it, so the fix
itself has not been observed passing.

### 9.3 `spliterator()` — REFUSED, and §3's claim about it is struck

§3 says: *"`spliterator` is the load-bearing one — `takeWhile`, `dropWhile`,
`mapMulti` and the static `concat` are default/static methods whose real JDK
bodies drive it, so one registration closes four more members."* The benefit is
real. **The cost is a registration on `java/util/Spliterator$OfInt`,
`$OfLong` and `$OfDouble`, and that is not a contained change.**

  * `tryAdvance(IntConsumer)` and `forEachRemaining(IntConsumer)` are
    **abstract** on those interfaces. So the escape hatch the sibling
    `java/util/Spliterator` natives rely on does not apply: their own comments
    record that *"the dispatcher in `invoke_on_class_shared_inner` now prefers
    default-method bytecode over this native when the receiver is a non-synthetic
    class"* — there is no default-method bytecode to prefer here.
  * Which means every real JDK primitive spliterator in the image —
    `Spliterators$IntArraySpliterator`, the `*Pipeline` sources, every
    `Spliterator.OfInt` an application declares — becomes a candidate receiver
    for a native written for a 3-slot synthetic shape. That is the
    interface-registration hazard the `java/util/Spliterator` block already
    guards against with a "field 0 is not an array → bail" test, and **the bail
    branch has no correct answer for `tryAdvance`**: `false` silently truncates
    the caller's stream, `true` without calling the consumer corrupts it, and
    re-dispatching through `invoke_virtual` re-enters the same native.

A registration whose failure mode is "every primitive spliterator in the process
silently reports empty" is not one to land from a lane that cannot build, cannot
run, and cannot take a census of the receivers. `takeWhile`/`dropWhile`/
`mapMulti`/`concat` therefore remain dead on a synthetic primitive stream, one
frame deeper than they used to be.

**What a future lane needs, and it is a measurement not a patch:** a
`--dump-native-registry` census of who actually implements `Spliterator.OfInt`
in the corpus arms, and a decision on whether the three primitive spliterators
should be a distinct synthetic class name (so the receiver test is exact and the
bail branch is unreachable) rather than the interface name. The rest of this
record's shape — allocate through `try_alloc_synthetic` on the interface — is
what makes the guard impossible, so that is the thing to change first.

### 9.4 Coverage

`regression-suite/src/RJdkViews.java`, new `primitiveStreamSurface()` section,
run by `CORE_CLASSES` on a default invocation. Fifteen checks plus three `CK`
lines; every one of them fails on the old behaviour (an `AbstractMethodError`
that kills the run, or the `-0.0` rows above). `RJdkViews` is deliberately not
also in `JDKONLY_CLASSES` — under `CRATONVM_ARGS=--jdk-only` the CORE list runs
with those args too, so the one file covers both modes.

---

## Adjudicated in `--synthetic-jdk` — 2026-08-12 (lane A31)

The reconciliation block at the head of this record says the §7.2 residual is
"Source only — nothing built, nothing run", and that adjudicating it needs a
build. A `--features synthetic-jdk` binary was built from clean HEAD and
launched with `--synthetic-jdk` — the mode `register_phase56_stream_extras` and
`register_phase56_primitive_stream_terminals` are actually reachable in.

**Headline: the wiring works, and §7.2's named holes are RETIRED.** The bug this
record came from — `IntStream.summaryStatistics()` killing whole probe runs with
`AbstractMethodError: … has no Code attribute` — does not reproduce:

```
--synthetic-jdk (HotSpot 25 identical on every row):
  R intStream.summaryStatistics = count=5 sum=15 min=1 max=5 avg=3.0
  R intStream.range.distinct    = [0, 1]
  R intStream.range.anyMatch    = true
  R intStream.range.findFirst   = 2
  R intStream.range.reduce      = 6
  R intStream.range.sorted      = [0, 1, 2]
  R intStream.range.iterator    = 0,1
  R intStream.concat            = [0, 1, 5]
  R intStream.asDouble.stats    = 6.0
  R longStream.range.reduce     = 6
```

That is every §7.2 member the reconciliation block lists as WRITTEN —
`anyMatch`, both `reduce` overloads, `findFirst`/`findAny`, `sorted`,
`distinct`, and the `iterator()` boxing bridge — answering correctly in the only
mode they are compiled into. **Retired.** (`summaryStatistics` was already green
in `--real-jdk`; this closes the other half.)

### Two residuals survive, and one of them is exactly what §9 refused

1. **`spliterator()` — CONFIRMED ABSENT, with the missing triple named.** §9
   refuses it as "REFUSED, not deferred". The refusal is still in force and now
   has a transcript:

   ```
   --synthetic-jdk: R intStream.range.spliterator ! java.lang.NoSuchMethodError:
       java.util.Spliterators.spliterator([IIII)Ljava/util/Spliterator$OfInt;
   HotSpot / --jdk-only: est=3
   ```

   The gap is not in `IntStream` at all — `IntStream.spliterator()` runs and
   reaches `java.util.Spliterators.spliterator([IIII)Ljava/util/Spliterator$OfInt;`,
   which the synthetic image does not declare. Anyone reopening §9 should target
   that one `Spliterators` triple, not the six `spliterator()` overloads §3
   assumed.

2. **The static `of(...)` factories are ABSENT — not previously recorded here.**

   ```
   --synthetic-jdk:
     R intStream.distinct    ! NoSuchMethodError: java.util.stream.IntStream.of([I)Ljava/util/stream/IntStream;
     R longStream.reduce     ! NoSuchMethodError: java.util.stream.LongStream.of([J)Ljava/util/stream/LongStream;
     R doubleStream.sorted.distinct ! NoSuchMethodError: java.util.stream.DoubleStream.of([D)Ljava/util/stream/DoubleStream;
   HotSpot / --jdk-only: [1, 3] / 6 / [1.0, 2.0, 3.0]
   ```

   §9's "static `concat`" exception is half right: `IntStream.concat` **works**
   (`[0, 1, 5]`), and it is `of(...)` — all three primitive flavours, varargs
   form — that is missing. This matters for the falsifier design: **a probe
   written with `IntStream.of(...)` measures the absence of `of`, not the
   terminal it was aimed at.** Every green row above uses `IntStream.range` /
   `LongStream.range` for exactly that reason. Redo any earlier
   `of`-based measurement before trusting it.

Method note: none of this is visible from `--real-jdk` or `--jdk-only`, where
real JDK bytecode serves all of it and the whole file is out of the picture. The
falsifier the head of this record asks for (`cratonvm --real-jdk …
ShadowDifferentialProbe`) tests a different question from the one §7.2's
registrations answer.
