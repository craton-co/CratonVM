# `BasicErrorControllerIntegrationTests` aborts with `checkcast: not an object reference`

**Status: ✅ RESOLVED (2026-07-31).** Two independent GC bugs, both fixed:
the moving young collector never rooted collection-overlay side-table
references, and several TreeSet/TreeMap backing-array allocations published a
pointer through a pre-allocation (possibly relocated) owner. Filed as OPEN
earlier the same day while retiring the Spring/javac JIT bans; the ban removal
was correctly exonerated then and is unrelated to the fix.

## Symptom

A hard VM abort — the process dies mid-run, after the Spring Boot banner:

```
[cratonvm] main-vm run() returned Err: Error in thread "main"
internal error: checkcast: not an object reference
```

Intermittent but frequent: **5 hard aborts and 3 partial failures in 12 runs**
of `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
(`module/spring-boot-webmvc`, Spring Boot 4.1.0-SNAPSHOT, real JDK 25). Some
runs died with SIGSEGV instead, and a second face — a flaky
`BeanDefinitionStoreException: Error processing condition on
HttpMessageConvertersAutoConfiguration` — turned out to be the same corruption
landing elsewhere.

## Root cause

The abort site is precise once the error message names it (this session
widened it from the bare `checkcast: not an object reference`):

```
checkcast: not an object reference (got Int(0))
  at java/lang/String$CaseInsensitiveComparator.compare(Ljava/lang/Object;Ljava/lang/Object;)I pc=6
```

Spring's `JdkClientHttpRequest.lambda$buildRequest$0` calls
`DISALLOWED_HEADERS.contains(name)`. `DISALLOWED_HEADERS` is a
`TreeSet<>(String.CASE_INSENSITIVE_ORDER)`, and CratonVM implements
`TreeSet.contains` natively: `native_ts_contains` → `ts_binary_search` →
`tree_compare` → `comparator_compare` → the real
`CaseInsensitiveComparator.compare`. The element handed to the comparator was
`Int(0)`, not a String.

Instrumenting the backing store showed why:

```
[DBG_TSSLOT] non-ref at mid=2 size=5 arr_len=0 ... data_cid=ClassId(0)
             data_cls="java/lang/Object" owner_cls="java/util/TreeSet"
```

`size=5` but the backing array has `array_length == 0` and `ClassId(0)` — the
documented signature of a **freed, zeroed** object. The array had been
collected out from under a live TreeSet, and reading past its (now zero)
length yields `Int(0)`.

CratonVM keeps TreeSet/TreeMap/LinkedList/LinkedHashMap backing stores in
process-global Rust **side tables**, not in Java heap fields, so no root slot
and no card can describe the collection→array edge. Two independent holes let
that edge be missed:

### 1. The moving young collector had no overlay rooting at all

`native_roots::scan_collection_overlays` deliberately skips the unconditional
overlay root scan on the Generational collector while JIT quiescence is
engaged, an unregistered JIT frame is on the stack, or a major GC is pending —
relying instead on the marker walking each owner and pulling in
`external_roots_for_owner`. `sweep_young_non_moving` and `old_gen_gc`
implement that owner walk. The **moving Cheney young path never did**: an
audit of every `external_roots::` use in `gc/src/gen_heap.rs` finds them in
`sweep_young_non_moving`, `scan_young_object` and `old_gen_gc`, and nowhere in
the moving path. When both conditions met — overlay scan skipped, moving young
chosen — every overlay-held young array was silently reclaimed.

That combination became common exactly when moving-young became the default
(`codex/moving-young-default-20260730`), which matches the observed window:
0 aborts in 12 runs on dev `9ac1feffe`, 5 in 12 on `376114f635`.

**Fix** (`gc/src/gen_heap.rs`, new "Phase 1a"): seed the Cheney evacuation from
`external_roots_for_matching_owners(&|_| true)` — every current owner's refs,
exactly as the non-moving young path does. Only forwarding is needed; the side
tables are repointed afterwards by the existing `remap_external_roots` pass.

### 2. Backing arrays were published through a stale owner

`alloc_ref_array` is a Java-heap allocation and can trigger a moving young
collection that relocates the collection object. Ten TreeSet/TreeMap sites
allocated a backing array and then stored it through the **pre-allocation**
`this`, registering the overlay under a stale owner address in
`overlay_owner_keys` — the reverse index the collector consults for
"which side-table refs does this collection own?". The collection still read
its own state fine (the side-table key is the relocation-invariant identity
hash), but the GC could not associate the live object with its overlay.

`native_tm_put` already had this fix, commented "gcstress face-1"; every other
site had been missed.

**Fix** (`native-collections/src/lib.rs`): new `ts_install_backing_array` /
`tm_install_backing_array` helpers that pin BOTH the owner and the new array
across the allocation and the store and return the refreshed pair; all ten
sites routed through them, plus the unpinned `new_arr` window in
`ts_ensure_capacity`.

## Results

`BasicErrorControllerIntegrationTests`, real JDK 25:

| build | runs | aborts | partial failures | clean |
|---|---|---|---|---|
| dev `2f138f04e3` (before) | 12 | 5 | 3 | 4 |
| + side-table pin fix only | 14 | 1 | 2 | 11 |
| + moving-young overlay rooting | 20 | **0** | **0** | **20** |
| final (diagnostics removed) | 20 | **0** | **0** | **20** |

The `TreeSet` backing-store probe fired 0 times across the last 40 runs
(previously once per failing run), and the second face — the flaky
`Error processing condition on HttpMessageConvertersAutoConfiguration` — is
gone as well.

Unit tests: `cargo test --release` — `cratonvm-gc --lib` 873/0,
`cratonvm-native-collections` all suites pass including the overlay GC harness
(14 tests, one new: `every_overlay_value_is_reachable_through_an_always_true_owner_predicate`,
which pins the seed source the moving path now depends on),
`cratonvm-vm --lib` 2301/0, `cratonvm-jit --lib` 1060/0.
`JavacConsolidationProbe 200` still OK 200/200.

## Why this was worth chasing past the obvious

The abort's shape is identical to `SPRINGBOOT-HTTP-HEADER-COMPARATOR.1`, one
of the JIT bans removed earlier the same day (its doc comment: "a call to
`CaseInsensitiveComparator.apply(Object)`, followed by a fatal invalid-
reference checkcast"). The `NoSuchMethodError:
CaseInsensitiveComparator.apply(Object)Object` warning even appears in the log
immediately before the abort. Both are red herrings: the `apply` call is
`comparator_compare`'s documented key-extractor fallback, which runs only
*after* the real `compare` has already failed, and a pristine-dev control with
every ban still in place reproduced the abort at the same rate. The ban
removal was not involved.
