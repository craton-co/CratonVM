# Embedded KRaft `KafkaAutoConfigurationIntegrationTests` — two `ClassCastException`s, and the invented node class behind one of them

**Status: FIXED 2026-08-04, residual closed 2026-08-04.** Filed 2026-08-04 as
a `ClassCastException: String cannot be cast to Number` inside
`KafkaConfig.listeners`, root cause unknown. Triage found two independent
defects; closing them left one residual, which is now closed too:

| # | Defect | Symptom in this test | Fix |
|---|---|---|---|
| 1 | `Map.remove(k,v)` / `replace(k,v)` / `replace(k,old,new)` were the only observable `Map` operations with no registered native | `ClassCastException: java.util.LinkedHashMap$Node cannot be cast to java.util.LinkedHashMap$Entry` → `IllegalStateException: Failed to shut down embedded Kafka cluster` (`containersFailed=1`) | `7696328fc` |
| 2 | `checkcast`/`instanceof` target class names were owned by the `CompiledMethod`, while the JIT type-check memos key on their raw address | the originally reported `String cannot be cast to Number` | `69e7124bc` |
| 3 | `java/util/LinkedHashMap$Node` is not a JDK class — the real nested node type is `java/util/LinkedHashMap$Entry`. Filed here as a residual "not reachable today"; it was reachable | the right-hand side of defect 1's cast message, and a `removeEldestEntry` override receiving an immutable copy instead of the live entry | `6ca262993` |

All three are general VM defects that happen to be reachable through this
test; none is Kafka-specific.

---

## Defect 1 — `Map`'s three conditional mutators ran real JDK bytecode

`native_map_init`'s own header states the premise the whole
`native-collections` layer rests on:

> Native HashMap path is the authoritative implementation (every observable
> Map method — put, get, size, isEmpty, containsKey, keySet, values, entrySet,
> toString, etc. — is registered as a native), so JDK bytecode for
> `HashMap.<method>` does not run.

Three methods were the exception: `remove(Object,Object)`, `replace(K,V)` and
`replace(K,V,V)`. They are `java.util.Map` **default** methods that `HashMap`
and `Hashtable` override with bodies that walk the bucket array directly.
Only `ConcurrentHashMap` had them registered — under a comment calling them
"ConcurrentHashMap-specific methods", which they are not.

So every call ran the REAL JDK bytecode over a table whose nodes this native
layer allocates. On a `LinkedHashMap` that path is:

```
HashMap.remove(k,v) -> HashMap.removeNode -> LinkedHashMap.afterNodeRemoval
```

and `afterNodeRemoval`'s first statement is `(LinkedHashMap.Entry<K,V>) e`.
CratonVM's nodes are `java/util/LinkedHashMap$Node` (`lhm_alloc_node`), a name
the real JDK does not have — so it threw.

Kafka's `MetadataLoader.removeAndClosePublisher` calls exactly
`publishers.remove(publisher.name(), publisher)` on a `LinkedHashMap` field
(`kafka-metadata-4.2.0`, `MetadataLoader.java:555`, `invokevirtual
java/util/LinkedHashMap.remove:(Ljava/lang/Object;Ljava/lang/Object;)Z`), so
broker shutdown died and the whole class reported `containersFailed=1`.

The cast is only the loudest symptom. On a plain `HashMap` the same bytecode
silently mutates the real bucket array and `size` field behind the native
bookkeeping (`map_state`'s size, the LinkedHashMap overlay's
`size`/`head`/`tail`, the integer fast-path overlay).

### Fix

Three new natives in `native-collections/src/lib.rs`
(`native_map_remove_kv`, `native_map_replace`, `native_map_replace_kv`), each
mirroring its `java.util.Map` default exactly, expressed in terms of the
already-registered and receiver-routing `get` / `containsKey` / `put` /
`remove`. Registered by `register_map_conditional_mutators` for `HashMap`,
`LinkedHashMap`, `Hashtable` and `Properties` — the families whose JDK class
overrides them with a bucket-walking body. `TreeMap` does not override them,
and its inherited `Map` defaults already call the registered natives.
`map_kv_reject_null_for_hashtable` preserves the
`Objects.requireNonNull(value)` that the `Hashtable`/`Properties` overrides
open with.

`force_native_over_real_jdk_bytecode` already lists `"remove"` and
`"replace"` by method name, so no gate change was needed — the missing
registry entry was the whole bug.

---

## Defect 2 — a freed class name aliased a live type-check cache key

This is the defect the original report could not pin down.

`jit_checkcast` / `jit_instanceof` receive their target class as a raw
`(ptr, len)` pair, and `vm/src/jit/helpers.rs` memoizes on exactly that pair
in two **thread-locals**:

```
JIT_TYPECHECK_TARGET_CACHE  (vm, ptr, len)                        -> ClassId
JIT_TYPECHECK_ANSWER_CACHE  (vm, ptr, len, receiver cid, lenient) -> passed
```

Those thread-locals outlive any one compiled method. The names, however, were
`Box<str>`s owned by `CompiledMethod::_jit_strings` — "freed when this
`CompiledMethod` is dropped", i.e. on tier-up, invalidation, or code-cache
eviction. The allocator then hands the same block to the next compilation, so
a `(ptr, len)` key silently starts naming a *different* class of the same
length while both caches keep answering for the old one.

Measured, not assumed: modelling this allocate/drop pattern under `mimalloc`
(this VM's allocator) over 8000 name allocations produced **3568** `(ptr,
len)` keys that later meant a different class name.

`java/lang/String` and `java/lang/Number` are both 16 bytes. A warm
`x instanceof String` site leaves `(vm, P, 16, String, lenient=false) -> true`
behind; once `P` is recycled for a `java/lang/Number` site, the strict
`instanceof` answers `true` for a `String`, while the paired `checkcast` — a
different site pointer, and `lenient = true`, so a different key — resolves
honestly and throws.

That is the reported stack exactly. `scala.runtime.BoxesRunTime.equals2` is
the only `checkcast java/lang/Number` anywhere in the
`Seq.distinct` → `StrictOptimizedSeqOps.distinctBy` → `HashSet.add` →
`BoxesRunTime.equals` chain, and its guard is seven bytecodes earlier:

```
  1: instanceof java/lang/Number
  4: ifeq      16
  8: checkcast java/lang/Number
```

`kafka.utils.CoreUtils$.validate$1` at `CoreUtils.scala:139` — the line the
report names — is `endPoints.map(_.listener).distinct`, a `Seq[String]`.
(The report's guess that the method holds two differently-typed `.distinct`
calls does not hold for Kafka 4.2.0: `javap` shows one, over `String`s. The
`Number` cast never came from a port list; it came from the guard lying.)

### Fix

`cratonvm_jit::intern_typecheck_class_name` — a process-wide, append-only,
content-deduped intern table. The `(ptr, len)` pair is now a permanent,
unique identity for one class name, which is what both caches always assumed.
All three producers switched over: the baseline compiler, the IR tier
(cov-05's `ir_instanceof_strings`/`ir_checkcast_strings`, now gone), and the
interpreter's own typecheck-info build. The table is bounded by the number of
DISTINCT class names at a type-check site, not by the number of compilations.

It also makes both memos strictly more effective: two type checks against the
same class — in one method or in two — now share an entry instead of evicting
each other, which is the exact thrash `JIT_TYPECHECK_TARGET_CACHE`'s own
header describes.

---

## Validation

HotSpot control first, on the same fixture and runner
(`/data/hsrun.sh module/spring-boot-kafka …`): **3 tests, 0 failed, 0
containersFailed** — a real CratonVM defect, not a stale fixture.

Azure Linux, `/data/data/wt-kafkabp-20260804`, one process per class via
`/data/sbrun.sh`, host load 7–20 throughout (well under the level that
invalidates a Spring Boot run).

| Binary | Contents | JIT | `--nojit` |
|---|---|---|---|
| `cratonvm-kafkabp` | `origin/dev` @ `3db59eb9b`, unmodified | 3/3 **FAIL** | 2/2 **FAIL** |
| `cratonvm-kafkabp-r2` | + defect 1 | 3/3 **PASS** | — |
| `cratonvm-kafkabp-r3` | + defect 2 | 3/3 **PASS** | 2/2 **PASS** |
| `cratonvm-kafkabp-r4` | + `origin/dev` merged forward | 9/10 **PASS**, 1 HANG | 2/2 **PASS** |
| `cratonvm-kafkabp-r5` | final merged state, as landed on `dev` | 3/3 **PASS** | 2/2 **PASS** |

Every passing run reported `SBRUNNER_RESULT tests=3 failed=0 aborted=0
skipped=0 containersFailed=0`.

**The one `r4` HANG is pre-existing load sensitivity, not this change.** It
happened in the only run that overlapped a concurrent `regression-suite`
build/run on the same box; its log stops at
`SocketServer listenerType=CONTROLLER … Enabling request processing` with the
controller fencing broker 0 for a timed-out session — the broker never
finished starting, and no test method ever ran. Re-running the identical
binary six times with the box otherwise idle gave **6/6 PASS**, with wall
times ranging 76–177 s, which is the same starvation pressure short of the
ceiling. This class is also on record as a 300 s HANG in the
[08-02 full-suite round](../../../../apps/spring-boot-suite-runner/RESULTS-20260802-azure-fullsuite.md),
i.e. long before either fix. Across all binaries today the failure counts are
0 hangs in 5 baseline runs and 1 in 12 post-fix runs — no signal, and the one
event has a load explanation in its own log.

`probes/MapConditionalMutatorProbe.java`, same host, same JDK: the baseline
binary dies on the very first `remove(k,v)` with

```
ClassCastException: java.util.LinkedHashMap$Node cannot be cast to java.util.LinkedHashMap$Entry
  at java/util/LinkedHashMap.afterNodeRemoval(LinkedHashMap.java:309)
  at java/util/HashMap.removeNode(HashMap.java:854)
  at java/util/HashMap.remove(HashMap.java:1158)
```

while `r3` prints `PROBE PASS` — 38 assertions, matching HotSpot line for
line.

`regression-suite/run.sh` against `r3` (it diffs CratonVM against HotSpot):
**22 passed, 0 failed**, including `RJitArrayTypecheck`, `RCollections`,
`RMapResizeGc` and `RMapGcStress`.

Spring Boot spot-check for the JIT change: a fixed 24-class random sample of
`all-tests.tsv` (`shuf -n 24 --random-source=<(yes)`), run **interleaved per
class** on the unmodified `dev` binary and on `r3` — 48 runs. Verdicts are
identical in all 24 pairs: 23 PASS on both arms, and
`jarmode.tools.ListCommandTests` FAILs on both (already a FAIL in the 08-04
residual list, unrelated). No class changed verdict in either direction.

The originally reported `String cannot be cast to Number` did **not**
reproduce on the unmodified `origin/dev` binary in any of the five baseline
runs (0/5); the `LinkedHashMap$Node` cast reproduced in all five (8
occurrences per run). Defect 2 is therefore closed on the strength of the
mechanism — which is measured and directly matches the reported stack — plus
its own regression test, not on a live reproduction of the 08-04 symptom.

### Regression tests

* `native-collections/tests/map_conditional_mutators.rs` — a registration
  guard for all four map families, plus behavioural coverage of all three
  methods (match / mismatch / absent key / old-value return /
  compare-and-set). **Verified by injecting the violation**: dropping the
  `LinkedHashMap` registration turns 4 of the 6 red; restoring it turns them
  green.
* `jit/src/lib.rs::typecheck_class_names_are_interned_by_content_not_owned_by_a_compilation`
  — two same-length names never share a `(ptr, len)` key across allocator
  churn, the same name is pointer-stable, and the bytes still read back
  correctly afterwards.
* `probes/MapConditionalMutatorProbe.java` — end-to-end witness over
  `LinkedHashMap` / `HashMap` / `Hashtable`, one line per assertion, runnable
  on HotSpot as its own control.

Also green after the change: `cratonvm-jit --lib` (1879), `cratonvm-types
--lib` (488), `cratonvm-native-collections --lib` (94) and all its
integration tests except `gc_relocation_harness`, which does not compile on
`dev` either (pre-existing, unrelated).

---

## Defect 3 — the node class itself was invented (residual, closed 2026-08-04)

Filed here as "known residual risk (not reachable today)". It was reachable.
Fixed by `6ca262993`.

`lhm_alloc_node` bound LinkedHashMap nodes to `java/util/LinkedHashMap$Node`,
a class the real JDK does not have — its nested node type is
`java/util/LinkedHashMap$Entry`:

```
$ javap -p --module java.base 'java.util.LinkedHashMap$Entry'      # JDK 25.0.3+9
class java.util.LinkedHashMap$Entry<K, V> extends java.util.HashMap$Node<K, V> {
  java.util.LinkedHashMap$Entry<K, V> before;
  java.util.LinkedHashMap$Entry<K, V> after;
}
$ javap -p --module java.base 'java.util.HashMap$Node'
class java.util.HashMap$Node<K, V> implements java.util.Map$Entry<K, V> {
  final int hash;  final K key;  V value;  java.util.HashMap$Node<K, V> next;
  public final K getKey();            public final V getValue();
  public final V setValue(V);         public final java.lang.String toString();
  public final int hashCode();        public final boolean equals(Object);
}
```

The field layout was already the real one (`hash@0, key@1, value@2, next@3,
before@4, after@5`), and `compute_field_layout` lays superclass fields out
first in declaration order, so the six `LHM_NODE_*` indices were already
exactly right. Only the class *identity* was invented — but identity is what
carries the six methods above, all declared on `HashMap$Node` and inherited by
`Entry`. On the invented class there were none.

That had two consequences; the residual note only saw the first.

1. **The cast.** It is what turned defect 1 from silent state divergence into
   a loud `ClassCastException`, and it would do so again for any future
   real-JDK path reaching a node.
2. **The missing methods — live, not hypothetical.** `native_lhm_put_evict`
   could not hand the head node to a `removeEldestEntry` override, because
   `eldest.getKey()` would have been a `NoSuchMethodError`. It copied the
   head's key and value into an `AbstractMap$SimpleImmutableEntry` instead.
   HotSpot's `afterNodeInsertion` passes the live `head`, so on the copy
   `eldest.setValue(v)` threw `UnsupportedOperationException` where HotSpot
   mutates the map, and the entry was never `==` the one in the map. That
   reproduces on the unmodified `dev` binary — see the baseline row below.

### Fix

`lhm_alloc_node` allocates `java/util/LinkedHashMap$Entry`. `alloc_synthetic`
resolves it to the real class (already in the tier-4b `view_classes` bootstrap
list), and `alloc_object`'s clamp raises the requested slot count to the
class's declared instance-field count — which is the same 6.

With real nodes the eldest-entry copy is unnecessary, so the hook passes
`head`. The pin discipline is unchanged in substance: the eldest key is still
read and pinned **before** the dispatch, because an override may unlink the
node reentrantly (Hibernate's `BoundedConcurrentHashMap.LRU` calls back into
`this.remove` from its eviction listener), and the node itself is now pinned
across the dispatch too.

Nothing else needed changing. `typecheck.rs` already listed
`java/util/LinkedHashMap$Entry` among the `Map$Entry` implementations (and
never listed `$Node`); no native is registered on either node class name; and
`getKey`/`getValue`/`setValue` resolve to the real inherited `HashMap$Node`
bodies rather than to the `java/util/Map$Entry` **interface** natives, because
native-override lookup keys on the declaring class of the *resolved* method.
That last one was the live risk in this change — those interface natives read
`key@0`/`value@1`, the synthetic `Map$Entry` layout, which on a real node is
`hash`/`key`. Confirmed empirically rather than by reading: `eldest.getKey()`
returns the key, not the boxed `hash` a slot-0 read would have produced.

### The regression this exposed, and its fix

Making the node honest turned on the descriptor-aware write path for its
slots, and that immediately broke something else. The 24-class Spring Boot A/B
below caught it: `JerseyAutoConfigurationDefaultFilterPathTests` went PASS ->
FAIL, and an interleaved re-run was **6/6 PASS on `dev`, 6/6 FAIL on the new
binary** — deterministic, not a flake.

`Resource.Builder` keeps its method builders in a `LinkedHashSet` and asserts
on the removal (`javap`: `getfield methodBuilders:Ljava/util/Set;` ->
`invokeinterface Set.remove` -> `Preconditions.checkState`):

```
IllegalStateException: Resource.Builder.onBuildMethod() invoked from a
resource method builder that is not registered in the resource builder
instance.
```

Reduced to 20 lines: `LinkedHashSet.remove(x)` **deleted the element and
returned `false`** (the set really did end up empty), while
`LinkedHashMap.remove` and `HashSet.remove` were both fine.

The cause is the other half of "the slot has a real type now". A `Set` is a
map whose values are a PRESENT marker, and `native_hs_add` writes a raw
`Value::Int(1)` as that marker. The node's value slot is `V value` ->
`Ljava/lang/Object;`, so the descriptor-aware write coerced the marker to
**null** — and both `native_hs_add` and `native_hs_remove` decide membership
purely from whether the previous value was null. `add` reported a duplicate as
new, and `remove` reported a present element as absent.
`CopyOnWriteArraySet` shares the backing and broke with it.

Fixed by `7bf427af1`: `native_lhm_put_evict` — the single choke point for both
node-value writes — boxes a primitive value on the way in. A real Java caller
can never arrive with one (`Map.put`'s descriptor is `(Object,Object)Object`,
so the interpreter has already boxed), so this only fires for our own
sentinels, and `Integer.valueOf(1)` returns the JDK's cached instance rather
than allocating.

The identical trap is still latent on the plain-`HashMap` view-set paths
(`map_alloc_node(ctx, entry, sentinel, ..)`, four sites). They are not broken
today only because `java/util/HashMap$Node` is in practice a synthetic stub
with no field descriptors, so no coercion runs — luck, not design. Left for
its own pass; **it is a live trip-wire for anyone who makes that class real.**

### Validation

HotSpot control first, same host and JDK: `probes/LinkedHashMapNodeProbe.java`
**PROBE PASS**, 80 assertions.

| Binary | Contents | `LinkedHashMapNodeProbe` | `MapConditionalMutatorProbe` |
|---|---|---|---|
| HotSpot 25.0.3+9 | the control | PASS | PASS |
| `cratonvm-lhment-base` | `origin/dev` @ `87d323bac`, unmodified | **FAIL** (JIT and `--nojit`) | PASS |
| `cratonvm-lhment-r1` | + the class change alone | **FAIL** (6, the Set regression) | PASS |
| `cratonvm-lhment-r2` | + the sentinel fix | PASS (JIT and `--nojit`) | PASS (JIT and `--nojit`) |
| `cratonvm-lhment-r3` | final merged state, as landed on `dev` | PASS (JIT and `--nojit`) | PASS (JIT and `--nojit`) |

The baseline failure is the original divergence, identically on both arms:

```
  FAIL eldest.getClass() expected=java.util.LinkedHashMap$Entry
                         actual=java.util.AbstractMap$SimpleImmutableEntry
Exception in thread "main" java/lang/UnsupportedOperationException
        at java/util/AbstractMap$SimpleImmutableEntry.setValue(AbstractMap.java:800)
        at LinkedHashMapNodeProbe$Observer.removeEldestEntry(...)
```

and `r1`'s is the regression, which the probe now carries a section for:

```
  FAIL   LinkedHashSet add(dup) expected=false actual=true
  FAIL   LinkedHashSet remove(present) expected=true actual=false
  FAIL   LinkedHashSet 40 identity removes report true expected=40 actual=0
  FAIL   CopyOnWriteArraySet ... (same three)
```

**A note on how nearly this was missed.** The first version of that probe
section did not compile (`Set`/`HashSet`/`LinkedHashSet` unimported). `javac`
failed, the runner kept using the previous class file, and every arm — base,
`r1`, `r2` — reported `PROBE PASS`, including the one the section exists to
catch. Always check that the compile succeeded before reading the verdicts.

`KafkaAutoConfigurationIntegrationTests`, run **interleaved across the
binaries** (arm order rotating) so the class's known load sensitivity cannot
land on one arm:

| Arm | JIT | `--nojit` |
|---|---|---|
| `base` | 8/8 PASS | 2/3 PASS |
| `r1` | 8/8 PASS | — |
| `r2` | 7/8 PASS | 3/3 PASS |

Every PASS reports `SBRUNNER_RESULT tests=3 failed=0 aborted=0 skipped=0
containersFailed=0`. **Both reds are this class's documented load sensitivity,
not an arm difference** — one on `r2` under JIT, one on `base` under
`--nojit`. `testEndToEndWithRetryTopics` gates on a 30-second latch, and the
reds are that assertion or a starved broker heartbeat
(`InvalidReplicationFactorException: All brokers are currently fenced`) in
runs taking 170–290 s against the 33–56 s an idle box gives.

An earlier window recorded **0/3 on `r2`**, and it is kept here rather than
dropped, because "0/3" is exactly the shape that reads as a regression when it
is a busy neighbour. Every one of those runs took 150–218 s with host load at
80–95 from concurrent builds by other sessions — past the level at which a
Spring Boot verdict means nothing. Re-run interleaved against `base` and `r1`
on a quiet box, `r2` went 5/5.

`regression-suite/run.sh` (it diffs CratonVM against HotSpot): **24 passed, 0
failed** against `r2` and again against `r3`, including `RCollections`,
`RJdkCollections`, `RMapResizeGc`, `RMapGcStress`, `RSerial` and
`RForNameGcStress`.

Re-verified on the **merged state** (`r3`): both probes PASS under JIT and
`--nojit`, regression suite 24/24, Kafka 2/3 JIT and 2/2 `--nojit`. A further
interleaved `base`-vs-`r3` round landed in a second load storm and stratifies
cleanly by wall time rather than by arm — every run that finished in 28–30 s
passed on both arms, every run that took 219–511 s failed on both:

```
[base] PASS 29s   [r3] PASS  28s      <- load ~10
[base] PASS 30s   [r3] PASS  30s
[base] PASS 30s   [r3] FAIL 219s      <- load climbing past 85
[base] FAIL  58s  [r3] FAIL 405s
[base] FAIL 511s
```

`cratonvm-native-collections`: **94** lib tests plus **86** across every
integration test (`abstract_collection_interception`, `gc_native_pins`,
`gc_relocation_collection_stores`, `gc_side_table_root_audit`,
`map_conditional_mutators`, `mock_arraylist`, `mock_concurrency`,
`mock_hashmap`, `mock_lhm_access_order`, `mock_treemap`, and the new
`lhm_node_class_identity`). `gc_relocation_harness` is excluded — it does not
compile on `dev` either.

Collateral damage — and the reason this pass was worth running: a fixed
24-class random sample of the Spring Boot suite (`shuf -n 24
--random-source=<(yes)` over the 08-02 full-suite PASS set), run **interleaved
per class** with the arm order alternating, `dev` against the new binary.

| Round | New binary | Result |
|---|---|---|
| 1 | `r1` (class change alone) | **23 SAME, 1 DIFF** — `JerseyAutoConfigurationDefaultFilterPathTests` PASS -> FAIL |
| 2 | `r2` (as landed) | **24 SAME, 0 DIFF** |

The round-1 DIFF is the `LinkedHashSet` regression above. It was confirmed
deterministic before being diagnosed — the class re-run interleaved was
**6/6 PASS on `dev` against 6/6 FAIL on `r1`** — and it is PASS/PASS in round
2.

### Regression tests

* `native-collections/tests/lhm_node_class_identity.rs` — the node allocator
  asks for `java/util/LinkedHashMap$Entry` and never for the invented name;
  the hook receives the live head node rather than a copy; the node carries
  the real slot layout (`Int` in slot 0, `before`/`after` at 4/5); and a plain
  `java/util/LinkedHashMap` still never dispatches the hook, so the three
  above cannot pass vacuously. **Verified by injecting each violation
  separately**: restoring the `$Node` name turns 3 of the 4 red, and restoring
  the `SimpleImmutableEntry` copy on its own turns 2 red.
* `probes/LinkedHashMapNodeProbe.java` — the end-to-end witness, runnable on
  HotSpot as its own control: what the hook is handed and every method it can
  call on it, `setValue` write-through, LRU eviction, the Hibernate reentrant
  eviction shape under both verdicts, insertion order across head and tail
  removal, `entrySet` `setValue`, access-order LRU, serialization round trip,
  and 2000 entries across several resizes.

### Not run, and why

The Hibernate `InPredicateTest` witness for the `removeEldestEntry` hook was
**not** re-run. The host's Hibernate fixture is incomplete (156 of 235
classpath entries present, and the test class is not built), and that test is
independently on record as a JIT timeout on `dev`, so it would not have given
a clean signal even rebuilt. The hook change is instead covered end-to-end by
`LinkedHashMapNodeProbe`'s reentrant-eviction section, which reproduces
`BoundedConcurrentHashMap.LRU`'s exact shape — the override removes the eldest
itself from its eviction listener — under both verdicts it can report, with
HotSpot as the control.

### Left alone, deliberately

`map_alloc_node`'s comment still says it allocates with `ClassId::new(0)`
"rather than binding to the real `java/util/HashMap$Node` class"; the line
below it has bound to the real class for some time. Stale comment, correct
code, unrelated to this doc.

## Affected classes

- `module/spring-boot-kafka` —
  `org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`

---

## Follow-up 2026-08-04 — the same trap on the HashMap side

Defect 3 left a note that the identical primitive-marker trap sat on the
plain-`HashMap` view-set paths, "not broken today only because
`java/util/HashMap$Node` is in practice a synthetic stub". **That reason was
wrong**, and the corrected mechanism is worth having, because it is the thing
that decides whether the trap is armed.

`java/util/HashMap$Node` is loaded and REAL — it is in `--dump-class-origins`.
There are simply two node allocators, and they differ in class:

| Allocator | Used by | Class | Descriptors? |
|---|---|---|---|
| `native_map_put_evict_pinned` | every ordinary `put` | `ClassId::new(0)` → `cratonvm/synthetic/AnonymousObject$4` | **no** |
| `map_alloc_node` | the five view/snapshot set builders | real `java/util/HashMap$Node` | **yes** |

Plain `HashSet.remove` works because HashSet's nodes never reach the allocator
that binds to the real class — not because that class is a stub.

So the raw `Value::Int(1)` marker was **already being destroyed today**, on
every view-set snapshot node: ~1000 coerced-to-null writes in a program that
does nothing but iterate an `entrySet`. Nothing read them back (the view
branches of `native_hs_contains`/`native_hs_remove` resolve membership against
the source map's `containsKey`/`get`), so no test failed — and it drowned
`CRATONVM_DBG=overlay` in benign noise, which is exactly what made that
detector's output easy to mis-scope while Defect 3 was being fixed.

Fixed by `a6bc95030` + `e5a9f6f10`: every PRESENT marker now goes through
`present_marker()` and is a reference (the element itself — no allocation, no
Java dispatch). A null element has no reference to mark itself with, so
`native_hs_add`/`native_hs_remove` settle that one case with `containsKey`.

**Measured outcome:** destructive native `set_field` writes over the same
program went **1000 → 0**. `LinkedHashMapNodeProbe` grew a map-view section
and a null-element section (80 → 102 assertions, HotSpot green), plus a
`SetSurface` probe over the whole set surface.

### The part that is NOT fixed, measured rather than assumed

Binding the ordinary-put node to the real class is still **not** safe, and the
marker was only one of the reasons. Built exactly that way on top of the fix:

```
             pre-fix + real node class   post-fix + real node class
node probe   PROBE FAIL (9)              still FAIL
SetSurface   SETSURFACE FAIL 7           still FAIL, incl.
                                         Map$Entry.getKey() == null
                                         keySet().remove leaves map unshrunk
```

Whatever else that node's real descriptors change has not been chased down, so
switching the class is its own validated project, not a tidy-up. The comment
at the allocation says so, and the probe sections go loudly red (9 failures)
the moment the line changes — which is the guard, verified by injection.

**Validation:** HotSpot control green on both probes; the landed binary PASS on
JIT and `--nojit`; `regression-suite/run.sh` **25/25**; `cratonvm-native-
collections` **94 lib + 86 integration**; `KafkaAutoConfigurationIntegrationTests`
**4/4 PASS** at 25–49 s on a quiet box (one earlier red at host load 68 was a
controller-starvation `TimeoutException` — `writeNoOpRecord took 10717 ms` —
with none of this defect's signatures in the log).
