# `--jdk-only` refuses `cratonvm/stream/LazyOp`, and the stream stack is SPLIT

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED.** The `LazyOp` strict guard landed 2026-08-07 (commit
  `ba50b498b`): the policy is asked before minting
  (`native-collections/src/lib.rs:18084`) and the mint is guarded at `:18137`.
  The three 2026-08-11 iterator fixes are in the tree too —
  `native_ksv_iterator` (`native-collections/src/lib.rs:47037`, registered
  `:47560`, routed `:5528`/`:13513`), the `java/util/LinkedList$Itr` fallback
  (`:32145-32174`), and the `java/util/ArrayDeque$Itr` fallback
  (`:34718-34754`, `:36896`, `:36911`).
* **Residual 1: CLOSED — this record's text is stale.** It says
  `linkedList.listIterator()` is *"still refused under `--jdk-only`,
  deliberately"*. That was reversed on 2026-08-11 by commit `6ae3ca634`
  *fix(jdk-only): mint LinkedListSnapshotListItr through the VM-internal door* —
  both gates moved together: `ensure_vm_internal_class` at
  `native-collections/src/lib.rs:31049` and `:31076`, and the name moved from
  `VM_MINTED_STAND_IN_RECEIVERS` to `VM_SERVICE_RECEIVERS` in
  `native-api/src/no_image_receiver.rs`. Rationale written in place at
  `lib.rs:30871-30905`; follow-up record
  W7-16-arraydeque-and-linkedlist-residuals.md.
* **Residual 2: CLOSED, and this record's DIAGNOSIS was wrong.**
  `ArrayDeque.stream().count()` answering `0` in both modes is fixed in source by
  commit `fddf67650` *fix(collections): give ArrayDeque back the JDK's spare
  ring-buffer slot* (`ad_ensure_capacity`, `native-collections/src/lib.rs:34020`,
  measurement at `:34004-34005`). This record attributed the defect to *"the
  unwritten `tail`"*. That is not what it was — it was the missing spare slot in
  the ring buffer, which HotSpot keeps and we did not. W7-16 carries the
  correction. Do not chase `tail`.
* **Residual 3: STILL OPEN.** Seven names on `NO_IMAGE_JDK_RECEIVERS` are minted
  outside `native-collections` and were never probed: `HashMap$Entry`,
  `IteratorEnumeration`, `ServiceLoader$Itr`, `CompletedFuture`, and three
  `Atomic*FieldUpdater$RustJvmImpl`. All still listed at
  `native-api/src/no_image_receiver.rs:147-157`, unchanged. Re-grepped
  2026-08-12.
* **Residual 4: STILL OPEN, cosmetic.** `cratonvm/internal/StreamChainCollector`
  (`native-collections/src/lib.rs:18840`) is still an unguarded
  `try_alloc_synthetic(..)?` — unreachable under strict, so it cannot bite
  today, but it is the last instance of the shape this record was opened for.

The inventory below is the more durable half of this record.

> ## UPDATED 2026-08-11 — the stream half is green; the split that was still
> ## costing runs is the ITERATOR family, and it wears `java/util/*` names
>
> This record was kept by a records audit on the grounds that *"the stream
> stack is still SPLIT, and the staged path is unwalked."* Both halves of that
> were re-measured, on a built binary, and the audit's premise needs splitting
> in two.
>
> **The stream stack no longer costs a single run.** Every stream row of a
> three-arm probe (HotSpot 25 / `cratonvm --real-jdk` / `cratonvm --jdk-only`,
> one binary, only the mode differing) is byte-identical:
> `stream().filter().map().collect()`, `IntStream.rangeClosed().boxed().limit()`,
> `Collectors.joining`, and `parallelStream().map().collect()`. So is every row
> of `probes/JdkOnlyCollectionViewProbe` — all 37 lines, all three arms, no
> diff — which is the whole `Collections.unmodifiable*` / `List.of` / `Map.of` /
> sub-list / comparator / function surface this record's "adjacent, same file,
> not this lane's" paragraph listed. That family closed the way §1 of
> `docs/architecture/natives-over-real-jdk-classes.md` says it should: the
> factories were retagged `SyntheticStub`, strict DROPS the registration, and
> `java.base`'s own bytecode runs.
>
> **The 13 `cratonvm/internal/*` boot refusals in every strict run are that
> mechanism working, not the failure.** `ensure_bootstrap_compat_class`
> (`vm/src/vm/vm_init.rs`) says so in place: *"these stand-ins exist for the
> synthetic collection shims, which strict mode does not register."* Read the
> WARN's "the natives bound to it are unreachable" as the claim it is — about
> registration — and it is the intended end state for this family, not a
> symptom. Nothing downstream of those 13 lines failed in any probe.
>
> **What WAS still costing runs is three iterator entry points**, and they are
> invisible to a `cratonvm/*` grep because the fabricated names are
> `java/util/*`:
>
> | entry point | fabricated class | image declares it? | before | after |
> |---|---|---|---|---|
> | `ConcurrentHashMap.newKeySet().iterator()`, `chm.keySet().iterator()` | `java/util/HashMap$KeyItr` | no (real: `HashMap$KeyIterator`) | `NoClassDefFoundError` | real `Arrays$ArrayItr` + `SetLike` write-through |
> | `new LinkedList<>(..).iterator()` | `java/util/LinkedList$Itr` | no (real: `LinkedList$ListItr`) | `NoClassDefFoundError` | real `Arrays$ArrayItr` + new `LinkedList` route |
> | `new ArrayDeque<>(..).iterator()` | `java/util/ArrayDeque$Itr` | no (real: `ArrayDeque$DeqIterator`) | `NoClassDefFoundError` | real `Arrays$ArrayItr` + new `ArrayDeque` route |
>
> The first of those is `RChmKeySetView --jdk-only`, which dies at
> `surface():140` on a plain `for (String x : s)` — and dies identically on a
> pre-merge control binary, so it is a standing defect rather than anything
> this wave introduced. All three are fixed in
> `native-collections/src/lib.rs`; `Compatible` executes none of the new arms.
> The generalisation worth carrying out of this: **`--jdk-only` refuses a
> fabrication by NAME, not by package**, so an inventory scoped to
> `cratonvm/*` under-reports the same defect by however many stand-ins were
> given a `java/util/*` name. `native-api/src/no_image_receiver.rs`'s
> `NO_IMAGE_JDK_RECEIVERS` is the list to read alongside this one.
>
> The full re-measured split table, the two arguments that measurement
> refuted, and the residuals are in the section *"2026-08-11 — the re-measured
> split"* at the end of this file.

## The failure

Two suite classes, one error, strict only. Both pass in `--real-jdk` (exit 0)
and on HotSpot 25.

    RJdkCollections --jdk-only
      CK RJdkCollections list=[e0..e9]
      CK RJdkCollections map={a=2, m=3, z=1} set=[a, b, c] sum=4324
      Exception in thread "main" java/lang/NoClassDefFoundError: cratonvm/stream/LazyOp
          at RJdkCollections.streams(RJdkCollections.java:169)

    RJdkJmx --jdk-only
      CK RJdkJmx objectName=cratonvm.test:name=alpha,type=Counter
      Exception in thread "main" java/lang/NoClassDefFoundError: cratonvm/stream/LazyOp
          at RJdkJmx.registerAndInvoke(RJdkJmx.java:133)
          at java/lang/management/ManagementFactory.getPlatformMBeanServer(ManagementFactory.java:472)

`RJdkJmx` is the load-bearing one: nothing in that test writes a stream. Real
JDK `ManagementFactory` bytecode reaches one internally, so this blocked far
more than the one line that names it.

## Why it is new, and why the sweep that caused it was right

`native-collections/src/lib.rs`, `stream_make_lazy_derived`, used to read
`alloc_synthetic(ctx, "cratonvm/stream/LazyOp", 3)` — infallible, so it
fabricated even under strict. The dev-wide sweep that converted the fabrication
helpers to `Result<_, MethodCallFailed>` turned that into `try_alloc_synthetic`,
and strict mode started actually enforcing the ban here. The sweep exposed a
real gap; it did not create one.

## The fix

Ask the policy **before** minting anything, and treat a refusal as "do not
defer" rather than as an error:

```rust
if ctx
    .try_ensure_synthetic_class("cratonvm/stream/LazyOp", 3)
    .is_err()
{
    return Ok(None);
}
```

`Ok(None)` from `stream_make_lazy_derived` is the answer `stream_try_defer`
already gives when `CRATONVM_EAGER_STREAMS=1` is set, and all six deferrable ops
(`filter`, `map`, `flatMap`, `limit`, `skip`, `peek`) already carry a complete
eager body behind it — the pipeline that was the shipped default until the lazy
one was turned on. Nothing new is written; the fallback was already wired.

This is the idiom lane L7 established on 2026-08-05 for
`cratonvm/internal/StreamCollector`, at all three of its mint sites
(`native-collections/src/lib.rs` `drain_spliterator_to_array_capped`,
`native-builtins/src/phases_late/streams.rs` `drain_spliterator`,
`native-builtins/src/service_loader.rs` `drain_real_spliterator`), with the same
stated reason: *a fabrication that happens to run first makes every other site's
refusal order-dependent rather than a policy.*

### What strict mode loses

Element **results** are identical. Short-circuiting is not:

* `peek` runs on every element instead of stopping at the first `findFirst`.
* An unbounded source feeding `limit(n)` stops at the drain's safety cap
  (1,000,000) instead of at `n`. Bounded, but wasteful.

Only `--jdk-only` is affected. `Compatible` is unchanged: the guard makes the
same `try_ensure_synthetic_class(name, 3)` call, with the same field count, that
`try_alloc_synthetic`'s own `refused_class` arm already makes on the first
deferred op of the run.

### What was deliberately not done

**Not** `ensure_generated_class` / `ClassOrigin::VmInternal`, which never
refuses in either mode. A `LazyOp` record is VM-internal in *shape*, but its
reason for existing is the synthetic `java.util.stream` model — the thing strict
mode is meant to retire. Minting it there would make the synthetic pipeline work
under `--jdk-only` **and take the gap off the census in the same change**. The
guard above keeps the `CompatibilityClassRequested` violation recorded on every
refused call, so the census still reports the gap's size.

**Not** a `drop_real_layout_synthetic` arm in `native-api/src/registry.rs`. Two
reasons. That flag is set in *both* real-JDK arms of `vm/src/vm/vm_init.rs`
(lines 1635 and 2138) — it is a correctness gate, not a mode gate — so a stream
drop there changes the currently-passing `--real-jdk` arm too. And the drop
needed is not one class: see the inventory.

## The inventory (deliverable (d)) — the stream stack is SPLIT

### Fabricated `cratonvm/*` classes the stream stack needs

Three, and only three. None has a real-JDK equivalent — no JDK declares any of
these names, so "let the real class load" is not available for any of them.

| class | slots | minted at | real equivalent | strict status |
|---|---|---|---|---|
| `cratonvm/stream/LazyOp` | 3 (kind:Int, lambda:Object?, aux:Long) | `native-collections/src/lib.rs` `stream_make_lazy_derived` | none | **fixed here** — refusal falls back to the eager pipeline |
| `cratonvm/internal/StreamCollector` | 2 (Object[] storage, Int len) | 3 sites: `native-collections` `drain_spliterator_to_array_capped`; `native-builtins/src/phases_late/streams.rs` `drain_spliterator`; `native-builtins/src/service_loader.rs` `drain_real_spliterator` | none — but `java.util.Spliterators.iterator(Spliterator)` is public JDK API and replaces the need for it | already handled (L7): all three fall back to `drain_spliterator_via_real_iterator` |
| `cratonvm/internal/StreamChainCollector` | 0 | `native-collections/src/lib.rs` `drain_spliterator_inline` | none | reachable **only** through a deferred op-chain, which strict no longer builds after this fix — now unreachable under `--jdk-only`. Still an unguarded `try_alloc_synthetic(..)?` if a future path reaches it |

Adjacent, same file, not stream-specific but on the same refusal list (visible
in the boot WARNs of every strict run): `cratonvm/internal/Unmodifiable{Collection,
List,Set,SortedSet,NavigableSet,Map,Itr,ListItr,EntrySet,EntryItr,MapEntry}`,
`java/util/Enumeration$Impl`, `java/util/Comparator$Native`. Not this lane's.

`java/util/function/Function$Identity` is the fourth stand-in the stream/function
natives reach for; it is already gated (lane L18 + L7) and mints nothing under
strict.

### Why `java/util/stream/*` names are NOT on that list

Every name the stream natives allocate under `java/util/stream/` —
`Stream`, `IntStream`, `LongStream`, `DoubleStream`, `BaseStream`, `Collector`,
`Collectors`, `StreamSupport`, `ReferencePipeline`, `AbstractPipeline` — is a
class or interface the real JDK image declares. `try_alloc_synthetic` finds the
real bytes and never reaches the refusal path. That is why `--jdk-only` gets
*through* `IntStream.rangeClosed(1,20).boxed().collect(...)` on line 168 and
dies on line 169.

It is also why the synthetic surface is invisible to the census: the objects it
allocates have the real **interface** as their runtime class, which is a
different violation (instantiating an interface) that nothing currently refuses.

### The split: which SOURCES divert into the synthetic model

Measured independently by the orchestrator on the same build: `src.parallelStream()`
produces a genuine `ReferencePipeline` and runs real `AbstractPipeline`/`Nodes`
bytecode with real line numbers. The source explains that exactly.

**Intercepted → synthetic model:**

| entry point | registration site |
|---|---|
| `Stream.of(Object)` / `of(Object[])` / `empty()` / `concat(..)` | `native-collections/src/lib.rs` (`let c = "java/util/stream/Stream"` block, "Source methods") |
| `IntStream.range` / `rangeClosed` / `of` | `native-collections/src/lib.rs` (`let c = "java/util/stream/IntStream"` block) |
| `LongStream.*` / `DoubleStream.*` factories | `native-collections/src/lib.rs` (`let c = "java/util/stream/LongStream"` / `"…/DoubleStream"` blocks) |
| `StreamSupport.stream(Spliterator,Z)` | `native-builtins/src/service_loader.rs` (registered LAST, wins), also `phases_late/streams.rs` ×2 |
| `StreamSupport.intStream` / `longStream` / `doubleStream` | `native-builtins/src/phases_late/streams.rs` |
| **`Collection.stream()`** and **`List.stream()`** | `native-collections/src/lib.rs`, the `java/util/Collection` and `java/util/List` interface blocks |
| `ArrayList` / `HashSet` / `LinkedList` / `TreeSet` / … `.stream()` | `native-collections/src/lib.rs`, per-class blocks |
| `Arrays.stream(Object[])` | `native-builtins/src/lib.rs` — delegates to `list.stream()`, so synthetic transitively |
| `Arrays.stream(int[]/long[]/double[])` | `native-builtins/src/phases_early.rs` |
| `BitSet.stream()`, `JarFile.stream()`, `ServiceLoader.stream()` | `phases_early.rs`, `phases_late/jar_manifest.rs`, `service_loader.rs` |

**NOT intercepted → real pipeline:**

| entry point | evidence |
|---|---|
| `Collection.parallelStream()` / `List.parallelStream()` | **zero** registrations of the name `parallelStream` anywhere in the tree |

So the discriminator is precise and mechanical: **`stream()` is registered,
`parallelStream()` is not.** That single asymmetry is the whole split.

Two further wrinkles that any migration has to plan around:

* `java/util/stream/ReferencePipeline` and `java/util/stream/AbstractPipeline`
  carry natives of their own in `native-collections/src/lib.rs` (`collect`,
  and others), deliberately, so that a real pipeline arriving from real JDK
  bytecode is pulled back into the synthetic terminals. The real path is
  therefore not fully real today either.
* `Stream.concat` has a bounded-drain safety net (`FMT-STREAMS-CCE`) written
  specifically because a REAL pipeline operand can be infinite. Dropping the
  synthetic `concat` removes that net.

## Recommendation: the staged path to a real `java.util.stream`

Strategy (a) — drop the interception and let real stream bytecode run — is the
right end state and is **partially proven**, not hypothetical: the real pipeline
already executes on this VM for `parallelStream()`. But it is not one lane's
work, and it is not free.

Ordering, on evidence:

1. **First, the real path's own defect.** `parallelStream().filter(..).collect(..)`
   currently dies inside real bytecode with
   `NullPointerException: Cannot invoke "java.util.stream.Node.getChildCount()"
   because "node" is null` at `Nodes.flatten` — a `ForkJoinTask.invoke()` bridge
   answering the void `compute()`'s null instead of `getRawResult()`. Until that
   lands, switching a source from synthetic to real trades a
   `NoClassDefFoundError` for an NPE. That is a different lane.
2. **Then the sources, one family at a time**, `Collection.stream()`/`List.stream()`
   first — it is the highest-traffic one and it has a working oracle sitting
   next to it (`parallelStream()` on the same receiver).
3. **Then the `ReferencePipeline`/`AbstractPipeline` pull-backs**, which only
   exist to rescue real pipelines into the synthetic terminals and become
   actively harmful once the sources are real.
4. **Last, the three `cratonvm/*` classes**, which have no callers left at that
   point and can be deleted rather than gated.

The gate for every step is `NativeKind::SyntheticStub` (refused under `JdkOnly`
by `allowed_in`, `native-api/src/registry.rs`), **not** `drop_real_layout_synthetic`
— the latter fires in `--real-jdk` too, and the STANDING RULE binds: the
`synthetic-jdk` build has no class library, so the synthetic stream stack must
keep working there. Gate by mode; never delete what `Compatible` still needs.

## Falsifying observation

If `RJdkCollections --jdk-only` still fails at line 169, but now naming
`cratonvm/internal/StreamChainCollector` or `cratonvm/internal/StreamCollector`
instead of `cratonvm/stream/LazyOp`, then a deferred chain is being built by a
path that does not route through `stream_make_lazy_derived`, and the guard is in
the wrong function.

## Verification

    cargo build --release -p cratonvm-cli
    target/release/cratonvm --jdk-only -cp regression-suite/classes RJdkCollections
    target/release/cratonvm --jdk-only -cp regression-suite/classes RJdkJmx
    target/release/cratonvm            -cp regression-suite/classes RJdkCollections
    target/release/cratonvm            -cp regression-suite/classes RJdkJmx

Expected: all four exit 0. The `--jdk-only` runs should still emit
`CompatibilityClassRequested` violations naming `cratonvm/stream/LazyOp` under
`--jdk-only-report` — silence there would mean the gap was hidden rather than
recorded.

---

## 2026-08-11 — the re-measured split

Everything below was measured on the built `target/release/cratonvm.exe` of
the wave-1 integration merge, against Temurin 25.0.3.9 on windows/x64, one
binary per row with only `--jdk-only` differing. **The fixes described here
were written afterwards and are NOT rebuilt**, so every "after" is a claim
about source, and every "before" is an observation.

Method, and the one step that made the table trustworthy: each row prints
CONTENT, never "ok". `docs/known-issues/jdk-only/W7-1-treemap-views-and-iterator-remove-contract.md`
is the reason — four `TreeMap` navigable views answered `{}` for months
because every caller only iterated. Two rows below are *only* visible because
of that rule (`ArrayDeque.stream().count()`, and the empty-`table` finding for
`ConcurrentHashMap`).

### The split table

"Refused?" is `--jdk-only`'s answer to fabricating the class, which depends on
the NAME and nothing else: no supported image declares it ⇒ refused.

| fabricated class | needed by | refused? | resolution |
|---|---|---|---|
| `cratonvm/stream/LazyOp` | `stream_make_lazy_derived` (deferred `filter`/`map`/`flatMap`/`limit`/`skip`/`peek`) | yes | **already gated** (2026-08-07, this record). Falls back to the eager pipeline; all stream probe rows byte-identical in all three arms |
| `cratonvm/internal/StreamCollector` | 3 drain sites | yes | **already gated** (L7). Falls back to `drain_spliterator_via_real_iterator` |
| `cratonvm/internal/StreamChainCollector` | `drain_spliterator_inline` | yes | unreachable under strict — strict never builds a deferred op-chain. Still a bare `?`; a future path that reaches it fails loudly, which is the correct order of events |
| `cratonvm/internal/Unmodifiable*` (11) | the `Collections.unmodifiable*` / `*.of` / `copyOf` factories | yes — the 13 boot WARNs | **already closed** by retag: the factories are `SyntheticStub`, strict drops them, real `java.util.Collections` bytecode runs. All 37 `JdkOnlyCollectionViewProbe` rows identical across the three arms |
| `cratonvm/internal/ArrayListSubList` | `List.subList`, `Pattern.split` | yes | closed the same way — probe rows identical |
| `cratonvm/util/MapViewBacking` | map key/value/entry views | yes | never reached under strict in any probe; `keySet`/`values`/`entrySet` all identical across arms |
| `cratonvm/internal/SnapshotEnumeration` | `Collections.enumeration`, `Properties.propertyNames` | yes | never reached under strict — both rows identical across arms. The mint site is a bare `?`; left as a loud failure rather than gated blind |
| `java/util/Comparator$Native` | `make_comparator` (`naturalOrder`/`reverseOrder`) | yes — a boot WARN | never reached under strict; both sort rows identical across arms |
| `java/util/Collections$EmptyItr` | `Collections.emptyIterator()` | yes | never reached under strict; row identical |
| `java/util/TreeMap$KeyItr`, `java/util/TreeSet$Itr` | `TreeMap.keySet().iterator()`, `TreeSet.iterator()`/`descendingIterator()` | yes | **already gated** — both land on `real_snapshot_iterator` with the `TreeSet` route; rows identical |
| `java/util/HashMap$KeyItr` via `native_hs_iterator` | `HashSet`/`LinkedHashSet`/`Map.keySet()` iteration | yes | **already gated** (2026-08-05) — lands on `Arrays$ArrayItr` + `SetLike`; rows identical, including `it.remove()` write-through |
| **`java/util/HashMap$KeyItr` via `native_ksv_iterator`** | `ConcurrentHashMap.newKeySet().iterator()`, `chm.keySet().iterator()` | yes | **FIXED HERE** — same fallback, `SetLike` route (which already discriminated key-set views) |
| **`java/util/LinkedList$Itr`** | `linkedList.iterator()` | yes | **FIXED HERE** — `Arrays$ArrayItr` + new `LinkedList` route |
| **`java/util/ArrayDeque$Itr`** | `arrayDeque.iterator()` | yes | **FIXED HERE** — `Arrays$ArrayItr` + new `ArrayDeque` route |
| **`cratonvm/internal/LinkedListSnapshotListItr`** | `linkedList.listIterator()`, `listIterator(int)`, and every real `AbstractList` method that reaches them — `equals`, `hashCode`, `indexOf` on a foreign list | yes | **OPEN** — see the residual below. A real `Arrays$ArrayItr` cannot stand in for a `ListIterator` |

Every `java/util/stream/*` name the stream natives allocate is a real class,
as the original inventory says — that half needed no change and got none.

### The two arguments this measurement refuted

Both were correct when written, and both were outlived by later work in the
same file. Recording the shape, not just the outcome: **a comment that argues
why a fallback cannot exist has to be re-read against the fallback's own code
before it is believed.** This is the habit
`W6-12-stampedlock-split-brain.md` was kept for.

1. **"The KeySetView site must refuse rather than land on the array
   iterator."** The stated reason was that a fixed-size list's iterator
   answers `UnsupportedOperationException: remove` and `RChmKeySetView`
   exercises write-through removal. But `real_snapshot_iterator` had since
   grown a `backing` parameter, `snapshot_itr_backing_table` had since grown
   the pointer the real two-field class has no slot for, and
   `native_snapshot_itr_remove`'s `SetLike` arm **already** reads
   `if is_key_set_view(ctx, backing) { native_ksv_remove(..) }`. The
   write-through was built for this exact receiver and never wired to it.

2. **"Our overlay-based `LinkedList` never writes the real `size` and
   `first`"** / **"the JDK `first` field is always null"** — two registrar
   comments in the same file. Reflectively, under
   `--add-opens java.base/java.util=ALL-UNNAMED`, a two-element CratonVM
   `LinkedList` reads `size = 2` with `first` and `last` holding real
   `LinkedList$Node`s, and a three-element one walks
   `a(prev=null) b(prev=node) c(prev=node)` through the real `item`/`next`/
   `prev` fields — character-for-character what HotSpot prints. `ll_set`
   mirrors the overlay into the real fields (`ll_get`'s overlay-MISS arm
   documents it), and `linkedList.descendingIterator()` — which nothing
   intercepts — already runs real `LinkedList$ListItr` bytecode under
   `--jdk-only` and answers `[c,b,a]`. Both comments are corrected in place.

   **That did not make "let the real bytecode run" the right answer**, which
   is the more useful half. Drive a real `ListItr.remove()` at a native
   `LinkedList` and the two owners diverge: real `unlink` decrements the real
   `size` and re-links the real nodes, `ll_get` consults the OVERLAY first and
   still answers the old size, and the next real iteration walks past the end
   —

       first=c
       removed ok
       Exception: java/lang/NullPointerException: Cannot read field "item"
           at java/util/LinkedList$ListItr.previous(LinkedList.java:921)
           at java/util/LinkedList$DescendingIterator.next(LinkedList.java:1010)

   reproducible in **`Compatible`** mode on an unmodified binary, so it is a
   live defect independent of strict mode and independent of this fix. The
   snapshot fallback was chosen precisely to avoid adding a second writer.

### Why "let the real class serve it" was unavailable for the three fixed sites

Measured, not assumed — and each is the empty-view failure mode:

* **`ConcurrentHashMap`**: `table = null`, `baseCount = 0` on a native map
  holding one entry, against `Node[16]` / `1` on HotSpot. A real
  `KeySetView.iterator()` walks `table` and would iterate **empty**.
* **`ArrayDeque`**: `elements = Object[2]`, `head = 0`, **`tail = 0`** on a
  two-element deque, against `Object[3]` / `0` / `2` on HotSpot. Real
  `DeqIterator` derives its bounds from `head`/`tail` and would see size 0.
* **`LinkedList`**: the fields are right; the *ownership* is not (above).

### Residuals — open, with the evidence

1. **`linkedList.listIterator()` still refuses under `--jdk-only`**, and it
   takes `AbstractList.equals`/`hashCode`/`indexOf` with it: `ll.equals(new
   ArrayList<>(..))` answers `true` on HotSpot and in `Compatible`, and raises
   `NoClassDefFoundError: cratonvm/internal/LinkedListSnapshotListItr` under
   strict. Left refusing deliberately. Two designs were considered and both
   are worse than a loud failure until someone can build and measure them:

   * a real `Arrays$ArrayItr` cannot serve — `hasPrevious`/`previous`/`set`/
     `add`/`nextIndex` are not on it, and the CratonVM stand-in registers
     `set` (the registrar comment names `List.sort`'s default-method path as
     the caller that needs it);
   * a real `java.util.ArrayList$ListItr` over a snapshot `ArrayList` *is*
     constructible — CratonVM's `ArrayList` does populate the real `size` and
     `elementData` — but its `set()` is real bytecode writing the snapshot's
     `elementData`, so the write would silently not reach the `LinkedList`.
     A silently-dropped write is strictly worse than a `NoClassDefFoundError`,
     and this file's history is mostly instances of that.

   The honest fix is the same one the `Unmodifiable*` family got: make the
   natives stop owning `LinkedList` state, then drop the interception. That is
   a collections-reclassification-wave change, not an iterator change.

2. **`ArrayDeque.stream().count()` answers `0` in BOTH modes** (HotSpot: `2`),
   and `ad.size()`/`toString()`/`contains()` are all correct beside it — the
   natives answer those. Root cause is the unwritten `tail` above: nothing
   intercepts `stream()` for `ArrayDeque`, so the real `Collection.stream()`
   default reaches real `ArrayDeque.spliterator()`, which reads `head`/`tail`
   and reports an empty source. This is a `Compatible`-mode defect, so it was
   **not** touched here. Fixing `tail` at the `ad_state` write sites would
   close it and would also make the strict fallback above unnecessary; both
   need a build to verify.

3. **`java/util/HashMap$Entry`, `java/util/IteratorEnumeration`,
   `java/util/ServiceLoader$Itr`, `java/util/concurrent/CompletedFuture`** and
   the three `Atomic*FieldUpdater$RustJvmImpl` names are on
   `NO_IMAGE_JDK_RECEIVERS` but are minted outside `native-collections`. Not
   probed here. The species is the same; the owners are not.

### Falsifying observation (updated)

If a `--jdk-only` run of `RChmKeySetView` now fails at
`ConcurrentHashMap$KeySetView.removeAll`/`retainAll` rather than at
`surface():140`, the `SetLike` route is reaching `native_hs_remove` instead of
`native_ksv_remove` — i.e. `is_key_set_view` is answering `false` for the
receiver recorded as the backing, and the discriminator, not the fallback, is
wrong. If instead any of the three fixed iterations comes back **empty**, the
snapshot was taken from a collection the natives no longer own and the
fallback is reading the real fields after all.

### Verification (updated)

    cargo build --release -p cratonvm-cli
    for M in "--jdk-only" ""; do
      target/release/cratonvm $M --java-home "$JDK" -cp regression-suite/build RChmKeySetView
      target/release/cratonvm $M --java-home "$JDK" -cp probes JdkOnlyCollectionViewProbe
    done

Expected: exit 0 on all four, and the `JdkOnlyCollectionViewProbe` output
identical to `java -cp probes JdkOnlyCollectionViewProbe`. The `--jdk-only`
runs must still emit `CompatibilityClassRequested` violations naming
`java/util/HashMap$KeyItr`, `java/util/LinkedList$Itr` and
`java/util/ArrayDeque$Itr` under `--jdk-only-report`: the fallbacks are
supposed to keep the gap on the census, not take it off.
