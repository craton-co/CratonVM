# Embedded KRaft `KafkaAutoConfigurationIntegrationTests` — two `ClassCastException`s, both fixed

**Status: FIXED 2026-08-04.** Filed 2026-08-04 as a `ClassCastException:
String cannot be cast to Number` inside `KafkaConfig.listeners`, root cause
unknown. Triage found two independent defects, both now closed:

| # | Defect | Symptom in this test | Fix |
|---|---|---|---|
| 1 | `Map.remove(k,v)` / `replace(k,v)` / `replace(k,old,new)` were the only observable `Map` operations with no registered native | `ClassCastException: java.util.LinkedHashMap$Node cannot be cast to java.util.LinkedHashMap$Entry` → `IllegalStateException: Failed to shut down embedded Kafka cluster` (`containersFailed=1`) | `7696328fc` |
| 2 | `checkcast`/`instanceof` target class names were owned by the `CompiledMethod`, while the JIT type-check memos key on their raw address | the originally reported `String cannot be cast to Number` | `69e7124bc` |

Both are general VM defects that happen to be reachable through this test;
neither is Kafka-specific.

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

## Known residual risk (not reachable today)

`lhm_alloc_node` binds LinkedHashMap nodes to
`java/util/LinkedHashMap$Node`, a class the real JDK does not have — its real
nested node type is `java/util/LinkedHashMap$Entry`. The field layout is
already the real one (`hash@0, key@1, value@2, next@3, before@4, after@5`),
so only the class *identity* differs.

That mismatch is what turned defect 1 from silent state divergence into a
loud `ClassCastException`, and it will do so again for any future real-JDK
bytecode path that reaches a LinkedHashMap node. After this fix the known
entry points are all intercepted (`put`/`get`/`remove`/`putAll`/`compute*`/
`merge`/`putIfAbsent`/`getOrDefault`/`forEach`/`replaceAll`/`writeObject`/
`readObject`/the three added here, plus natively-snapshotted views), so there
is no live reproducer.

Switching the allocation to the real `java/util/LinkedHashMap$Entry` is the
honest model and would degrade any future gap from a hard cast failure to
(probably) correct field access. It changes the runtime class of every
LinkedHashMap node in the process, so it wants its own validated pass across
the collection suites rather than riding along here.

## Affected classes

- `module/spring-boot-kafka` —
  `org.springframework.boot.kafka.autoconfigure.KafkaAutoConfigurationIntegrationTests`
