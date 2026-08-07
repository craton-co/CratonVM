# `--jdk-only` refuses `cratonvm/stream/LazyOp`, and the stream stack is SPLIT

Status: fixed 2026-08-07 (wave-2 lane W2-1) in `native-collections/src/lib.rs`.
Not built or run — this worktree cannot build. The inventory below is the more
durable half of this record.

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
