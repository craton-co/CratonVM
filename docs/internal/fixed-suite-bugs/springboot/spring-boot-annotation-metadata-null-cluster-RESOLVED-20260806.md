# Resolved 2026-08-06: the annotation/merged-annotation "metadata returns null" cluster was the recycled-`JitInvokeInfo` dispatch defect

Branch `fix/springboot-annotation-metadata-null-20260806`. Retires
`docs/known-issues/springboot/spring-boot-annotation-metadata-null-cluster-20260805.md`
and, on the same evidence, its `Related` page
`docs/known-issues/springboot/spring-bean-attribute-type-null-flake-20260803.md`.

The page grouped two classes whose context startup died on an unexpected null
out of annotation-reflection machinery, and reasoned that three annotation
nulls across four classes on one day "looks like a systemic weak spot in
CratonVM's `MergedAnnotations`/ASM annotation-scanning path under GC or
class-loading pressure rather than 4 unrelated bugs". The grouping instinct was
right and the cause was one bug. It is not in the annotation path, and neither
GC nor class loading is involved.

**Cause: `383e7f5cf` — "a recycled `JitInvokeInfo` address let one call site
serve another's dispatch"**, which landed on `dev` at 13:22 on 2026-08-05,
*after* the full-suite run this page was written from. The page's own update
already suspected this and named the right next step; this closes it with the
measurement.

## Why the page could not have been right about the binary it measured

The 08-05 Azure full suite ran at `origin/dev` @ `1078f6f05c`
(`RESULTS-20260805-azure-fullsuite.md`, line 7). Ancestry:

| commit | landed 08-05 | in `1078f6f05c` | in current `dev` |
|---|---|---|---|
| `836631dcc` — site-cache EVERY native, not just leaves (*amplifier*) | 09:10 | no | yes |
| `383e7f5cf` — recycled `JitInvokeInfo` address (**the fix**) | 13:22 | no | yes |

So every class on that page was measured on a binary carrying the defect and
neither its amplifier nor its fix. Read out of the anchor worktree, not assumed,
that tree's flush state was:

| memo | cleared on JIT generation | cleared on class-definition epoch |
|---|---|---|
| `DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE` | yes | yes |
| `VIRTUAL_TARGET_CACHE` | **no** | yes |
| `OBJECT_NATIVE_DISPATCH_CACHE`, `INTEGER_NATIVE_DISPATCH_CACHE`, both counters | **no** | **no** |

Only the JIT-generation trigger moves when a `CompiledMethod` is published or
dropped, which is the event that recycles a `JitInvokeInfo` address. So six of
the eight were unprotected against recycling — `VIRTUAL_TARGET_CACHE` included,
despite having a flush, because the epoch it hangs off does not move on a code
lifetime event.

## The mechanism, named in these exact workloads

`CRATONVM_DBG_SITE_ALIAS=1` counts dispatch-helper entries whose
`JitSiteKey = (vm_identity, JitInvokeInfo pointer)` came to name a *different*
call site than the one that first used it — i.e. an address freed with its
`CompiledMethod` and re-issued. It measures the **precondition** of the defect
rather than its rare visible corruption, so one run answers "does this workload
alias?".

All three classes alias heavily — the printed list caps at 40 events per run and
all three hit the cap, out of ~900-1000 distinct site keys. The pairs it names
are not incidental; they are the exact call sites the two pages blame:

| observed aliased pair (one run, current `dev`) | the page symptom it produces |
|---|---|
| `Method.getDeclaringClass()` → `TypeMappedAnnotations.from(AnnotatedElement, SearchStrategy, Predicate, RepeatableContainers, AnnotationFilter)` | `MergedAnnotations.from(...)` returns **null** — the page's cassandra symptom. `MergedAnnotations.from` is a thin delegate to this method. |
| `AnnotationTypeMappings.get(I)` → `TypeDefinition$Sort.isNonGeneric()Z` | a method returning an `AnnotationTypeMapping` served by one returning a `boolean`: a reference slot receiving 0/1 |
| `TypeMappedAnnotations$Aggregate.getMappings(I)` → `Annotation.annotationType()` | null / wrong-typed annotation metadata |
| `Class.isPrimitive()Z` → `AnnotationsScanner.hasPlainJavaAnnotationsOnly(Object)Z` | **the neo4j symptom.** See the chain below. |
| `AnnotationTypeMapping.getDistance()I` → `Class.isPrimitive()Z` | **the `Related` page's symptom.** `getDistance()` returns a non-zero `int` into `isPrimitive()`'s boolean slot. |
| `Class.isAssignableFrom(Class)Z` → `Class.isPrimitive()Z` | same |
| `Annotation.annotationType()` → `ElementMatcher.matches(Object)Z` | a null-typed annotation escaping into a reflection walk |

### The neo4j chain is closed, not guessed

`No ConfigurationProperties annotation found on 'DataNeo4jProperties'` is
`Assert.state(annotation.isPresent(), ...)` in
`ConfigurationPropertiesBeanRegistrar.registerBeanDefinition`, reached from
`MergedAnnotations.from(type, TYPE_HIERARCHY).get(ConfigurationProperties.class)`.
Disassembling spring-core 7.0.7 (`javap -c`, not from memory):

* `TypeMappedAnnotations.from` returns the `NONE` singleton when
  `AnnotationsScanner.isKnownEmpty(element, strategy, …)` is true.
* `isKnownEmpty` returns true immediately if `hasPlainJavaAnnotationsOnly(source)`
  is true; otherwise, when `isWithoutHierarchy` holds, it reduces to
  `getDeclaredAnnotations(source, false).length == 0`.
* `DataNeo4jProperties` has superclass `Object`, no interfaces and no enclosing
  class, so `isWithoutHierarchy` **is** true for it.

Both surviving branches are single boolean-returning calls. A recycled site key
answering `hasPlainJavaAnnotationsOnly` with `Class.isPrimitive()`'s value —
which is one of the pairs the detector actually printed — yields `NONE`, and
`NONE.get(...)` reports the annotation absent for a class that plainly carries
it. No annotation-scanner defect is required.

## A/B: pre-fix RED, post-fix GREEN

One host (Windows, 32 logical cores), one fixture checkout, one process per
run, 4 lanes concurrent per class. The classes pass serially on **both**
binaries — 6/6 on the anchor — so concurrency is what exposes it, matching the
sibling page's finding. Blocks were run in both orders.

| block | binary | runs | runs with >=1 failed test |
|---|---|---:|---:|
| A | `1078f6f05c` (anchor, pre-fix) | 36 | **3** |
| B | current `dev` | 36 | 0 |
| C | current `dev` | 24 | 0 |
| D | `1078f6f05c` (anchor, pre-fix) | 36 | **4** |

Per class, anchor vs `dev`: neo4j **4/24** vs 0/20, cassandra **2/24** vs 0/20,
couchbase **1/24** vs 0/20. Totals **7/72 pre-fix, 0/60 post-fix**. At the
anchor's observed ~10% per-run rate, 60 clean post-fix runs is p < 0.002.

The page's headline symptom reproduced **byte-for-byte** on the anchor:

```
Factory method 'neo4jConversions' threw exception with message:
Cannot invoke "org.springframework.core.annotation.MergedAnnotations.get(java.lang.Class)"
because the return value of "org.springframework.core.annotation.MergedAnnotations.from(
java.lang.reflect.AnnotatedElement, org.springframework.core.annotation.MergedAnnotations$SearchStrategy,
org.springframework.core.annotation.RepeatableContainers)" is null
```

— note it landed in the **neo4j** class here, while the page recorded it under
**cassandra**. That is the point: which face you get is an accident of what the
recycled entry happened to hold. The seven anchor failures produced four
distinct faces across the same machinery:

1. `MergedAnnotations.from(...)` returned null (the quoted one);
2. `IllegalArgumentException: RepeatableContainers must not be null` — an
   `Assert.notNull` on an argument whose own static factory returned null;
3. `Cannot invoke "MergedAnnotation.isPresent()" because "annotation" is null`;
4. ByteBuddy: `AnnotationValue.filter(...)` on null, `Unknown type: null`.

None of the four appeared in 60 post-fix runs.

## What this refutes

* **"under GC or class-loading pressure"** — no. The corruption is a
  thread-local dispatch memo whose key stopped identifying its call site.
* **"a systemic weak spot in `MergedAnnotations`/ASM annotation scanning"** —
  the *concentration* in annotation code is real and has a mundane explanation:
  a reflection walk is dense in short methods that compile, tier up and drop,
  which is exactly what recycles `JitInvokeInfo` addresses. Annotation code was
  the victim with the most call sites, not the defect.
* **The `Related` page's central puzzle** — "It is one bad lookup, not a broken
  map… the map is intact before and after, so this is not a lost entry, not a
  resize race" — is answered exactly. `primitiveTypeToWrapperMap` was never
  broken. `clazz.isPrimitive()` returned another call site's boolean, so
  `resolvePrimitiveIfNecessary` looked up a non-primitive `Class` in a map of
  primitives and correctly missed, returning null. That page's own "where to
  look next" ranked `Class.isPrimitive()` answering true for a non-primitive
  **first**. It was right about the symptom and one layer off about the
  location: not the mirror's slot, the dispatch that delivered the answer.
  It also explains why ~1030 instrumented hunt runs never caught it — both
  detectors watched the map, and the map was innocent.

## What changed in the code here

Nothing about the fix — `383e7f5cf` is correct and complete; all eight
site-keyed memos are on the flush list, and the `JIT_TYPECHECK_*` family was
checked and is **not** exposed (its name pointers are content-interned and
leaked by `intern_typecheck_class_name`, never freed, so address reuse cannot
rename them — that hazard was closed separately).

What this branch changes is that the flush list can no longer drift from the
declaration list, which is the specific way the defect was introduced: four of
the eight memos existed for weeks with no trigger clearing them, because
declaring a memo and flushing it were two hand-maintained lists.
`site_keyed_memos!` now declares the memos **and** generates
`clear_site_keyed_dispatch_memos` from that one list, so a memo cannot exist
without being flushed. The compiler enforces it instead of a reviewer.

`a_jit_generation_change_clears_every_site_keyed_memo` was also vacuous for
three of the eight: it populated and asserted five, so dropping
`DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE` or `OBJECT_NATIVE_DISPATCH_CACHE`
from the flush would have left it green. It now populates **all eight** —
`OBJECT_NATIVE_DISPATCH_CACHE` needs a `NativeCallback`, which is a plain fn
pointer and so needs no registry, since the test stores it and never calls it —
and sweeps `site_keyed_memo_census()`, the real list, both before and after the
flush. So a memo added later is covered the moment it is declared, and the
pre-flush sweep names it if whoever added it forgot a population line.

## Reproduce

```bash
# the precondition, one run, no failure needed -- prints the aliased pairs
CRATONVM_DBG_SITE_ALIAS=1 <cratonvm> ... SbRunner \
  org.springframework.boot.data.neo4j.autoconfigure.DataNeo4jReactiveRepositoriesAutoConfigurationTests
```

```bash
# the flush guarantee
cargo test --release -p cratonvm-vm a_jit_generation_change_clears_every_site_keyed_memo
```

The A/B needs a binary built at `1078f6f05c` (pre-`383e7f5cf`) and 4 concurrent
lanes per class; serial runs pass on both arms and prove nothing.

## Affected classes — all green on current `dev`

- `module/spring-boot-data-cassandra` — `DataCassandraReactiveRepositoriesAutoConfigurationTests`
- `module/spring-boot-data-neo4j` — `DataNeo4jReactiveRepositoriesAutoConfigurationTests`
- `module/spring-boot-data-couchbase` — `DataCouchbaseReactiveRepositoriesAutoConfigurationTests` (the `Related` page's regression)

## One thing left honest

The `Related` page's **original** 2026-07-31 sighting
(`cratonvm-aotresid-r3.bin`, `Bean.autowireCandidate()`) is *consistent* with
this cause — the site-keyed memo family dates to `fc012ab1b` on 07-31 — but it
was not re-measured here and is not claimed as proven. Its 08-05 regression,
which is what re-opened the page, is.
