# HIB-CV-30 — Multi-level cascade "association is null" is really a JUnit store / `List.of().hashCode()` bug

**Run:** Hibernate ORM suite, retest 2026-06-23 (dev `c792b5f7`, ancestor of reported `f8cdd52b`)
**Binary used for diagnosis:** clean release built into an isolated target dir (`target-cv30`) — the
main worktree was mid-WIP and its `target/` was being thrashed by leftover build loops.
**Severity:** Medium-High — data-correctness divergence; deterministic under `--nojit`; HotSpot PASS.
**Status:** **ROOT-CAUSED.** Original "composite-key cascade/reflection" hypothesis is **WRONG**.

---

## TL;DR

The failing assertion is `assertNotNull(mainEntity)` (line 80), where
`mainEntity = session.find(MainEntity.class, 99427L)` returns **null**. It is **not** a
cascade bug, **not** composite-key `equals/hashCode`, and **not** `@EmbeddedId`/`@IdClass`
reflection. The real chain is:

1. Both tests share one `@TestInstance(PER_CLASS)` instance (via `@Jpa`). The first method
   (`...InitializeCollections`) runs `initialize()` (native-SQL `INSERT`s) and sets the
   instance field `initialized=true`. The second method
   (`...WithoutInitializingCollections`) therefore **skips** `initialize()` and relies on the
   data + schema persisting across methods — exactly as on HotSpot.
2. On CratonVM the **`EntityManagerFactory` is rebuilt between the two methods** (schema
   `drop`+`create` at the start of the 2nd test). Because the 2nd test skips `initialize()`,
   it queries a **freshly-created empty schema** → `find(...)` returns null → `assertNotNull`
   fails. HotSpot keeps the one class-scoped EMF, so the data is still there → PASS.
3. The EMF is rebuilt because the JUnit **`NamespacedHierarchicalStore` lookup that should
   return the cached, class-scoped EMF scope misses.** The store is keyed on
   `Namespace.hashCode()`, and that hash **shifts during the run** (`823745034` →
   `823745035`), so the entry put at class-context becomes unreachable from the 2nd method's
   parameter resolver → a brand-new EMF scope is created and built.
4. `Namespace.hashCode()` is unstable because it is computed over a
   `List.of(extensionClassName, testInstance)` parts list, and **CratonVM's synthetic
   immutable-list `hashCode()` mis-computes the `testInstance` element's contribution** — it
   yields `0`, then `1`, instead of the instance's stable identity hash (`78498`).

So: a VM `hashCode` defect on `java.util.List.of(...)` silently corrupts JUnit's extension
store, which breaks `@TestInstance(PER_CLASS)` EMF caching, which surfaces as "a cascaded
child/association is null".

Both variants (`@EmbeddedId` `MultiLevelCascadeCollectionEmbeddableTest` and `@IdClass`
`MultiLevelCascadeCollectionIdClassTest`) fail for the same reason — they share the
`initialized`-flag + native-SQL-`initialize()` + `@TestInstance(PER_CLASS)` structure; the
composite-id style is incidental.

---

## Evidence chain (all reproduced on the clean binary, `--nojit`)

### A. Each test passes in isolation; only the combined run fails
```
WITHOUT test ALONE  -> ok=1   (fresh instance: initialized=false -> initialize() runs -> find works)
INITIALIZE test ALONE -> ok=1
Both together        -> ok=1 failed=1   (WITHOUT fails: find -> null)
```

### B. Schema is rebuilt between the two methods (SQL + per-test markers)
```
@@START Initialize  -> drop table MAIN_TABLE; create table MAIN_TABLE; INSERT x4; select MAIN ... ; @@FINISH SUCCESSFUL
@@START Without     -> drop table MAIN_TABLE; create table MAIN_TABLE; select MAIN -> (null); @@FINISH FAILED
```
HotSpot: one `create table` for the whole class, both methods PASS.

### C. The JUnit store entry vanishes mid-run (spy extension reading the EMF scope)
A globally-registered `BeforeEach/AfterEach` spy reading
`ctx.getStore(Namespace.create(EntityManagerFactoryExtension.class.getName(), inst)).get(EMF_KEY)`:

| Point              | HotSpot scopeId | CratonVM scopeId |
|--------------------|-----------------|------------------|
| Initialize BEFORE  | 1472759652      | 80144 (found)    |
| Initialize AFTER   | 1472759652      | **0 (vanished)** |
| Without BEFORE     | 1472759652      | 0                |
| Without AFTER      | 1472759652      | 327692 (new obj) |

`System.identityHashCode(inst)` stays stable (`78500`) the whole time — the instance is fine;
the **store key hash** is what drifts.

### D. The drifting key is `Namespace.hashCode()` → `List.of(...).hashCode()`
```
@@SPY ... List.of=823745034  Objects.hash=823823534  Arrays.asList=823745034  nsHash=823745034   (BEFORE)
@@SPY ... List.of=823745035  Objects.hash=823823534  Arrays.asList=823745035  nsHash=823745035   (AFTER)
```
- `Objects.hash(cn, inst)` (Object[]-array path) is **stable + correct** = `823823534`.
- `List.of(cn, inst).hashCode()` (immutable-list path) = `823745034` → `823745035` (**wrong**,
  drifts by +1). Expected = `31*(31*1 + cn.hashCode()) + inst.hashCode()` = `823823534`.
  CratonVM's value corresponds to **`inst` contributing 0, then 1**, instead of `78498`.

### E. The miss is specific to the synthetic immutable-list `hashCode` path
With `inst` correct everywhere it is *read*:
```
@@SPY nativeHash=823745034  manualHash=823823532  elemHashes=[1689140375, 78498]
      arr1==inst=true arr1.hC=78498  iter1==inst=true iter1.hC=78498  instHC=78498
```
- `lo.get(1) == inst`, `lo.toArray()[1] == inst`, `lo.iterator()` yields `inst`, all with
  `hashCode()==78498`.
- A **manual Java `for (e : lo)` loop** computes the correct contract hash (`823823532`),
  reading `elemHashes=[cnHash, 78498]`.
- Only `lo.hashCode()` itself (the native path) is wrong.

### F. The exact internal path
`List.of(...)` is synthesised as `cratonvm/internal/UnmodifiableList` wrapping a synthetic
`java/util/ArrayList` backing (`getClass()` reports `ImmutableCollections$List12`). Its
`hashCode` native is `native_unmod_hash_code` →
`unmod_delegate(ctx, "hashCode", "()I")` → `ctx.invoke_virtual(backing, "hashCode", "()I")`.

Because `native_al_hash_code` is **not** force-listed over real JDK bytecode, that
`invoke_virtual` runs the **real `AbstractList.hashCode` bytecode** re-entrantly from native.
Instrumentation confirms `[ALHASH]`/`element_hash_code` is never reached for this list, while
`[UNMODHASH]` returns the buggy `823745034`. The real `AbstractList.hashCode` iterates and
calls the per-element `Object.hashCode()` — and for the `testInstance` element that nested,
deeply-re-entrant `Object.hashCode` (identity) returns a **counter value (0, then 1)** instead
of the header-cached identity hash (`78498`).

### G. It is GC-state-dependent (why a minimal repro does NOT reproduce)
A standalone `List.of("x", new Foo()).hashCode()` (no prior GC) is **correct and stable**
(both the direct and the deep re-entrant path agree). The divergence only appears for the
**already-GC'd** test instance: after the heavy first test, the re-entrant
`invoke_virtual(inst,"hashCode")` path disagrees with the direct/manual path (which still
reads `78498`). The `0 → 1` drift is one increment **per test method** (i.e. per GC cycle).
This points at the deep re-entrant identity-hash read using a stale/alternate value for a
relocated object rather than the preserved header hash. Big heap (`-Xmx6g`) does not help
(young-gen GC still fires).

---

## Repro

```
# from C:/craton/CratonVM/apps/hibernate-orm/.cratonvm-suite
cratonvm --nojit -Dlog4j2.configurationFile=<sql.props> @common.args \
  CratonRunner <list-with-MultiLevelCascadeCollectionEmbeddableTest> 0
#   -> @@RESULT ... ok=1 failed=1 ;  @@FAIL ... AssertionFailedError: expected: not <null>
```
Minimal in-VM signature (no Hibernate needed once a GC has run): a globally auto-registered
JUnit extension that, per test, evaluates
`List.of("x", thatTestInstance).hashCode()` — the value drifts across two `@TestInstance(PER_CLASS)`
methods. (Diagnosis scripts/spy extension lived in the suite dir as `SpyExt.java`/`OrderRunner.java`.)

---

## Fix

Two layers; the first is the true defect, the second (IMPLEMENTED) is the localized fix.

### Layer 1 (root defect — NOT yet fixed)
**Re-entrant `Object.hashCode` (identity) via `ctx.invoke_virtual` returns an unstable
counter value (0,1,…) instead of the header-cached identity hash for a GC-relocated, no-
`hashCode`-override receiver.** The specific trigger is real `AbstractList.hashCode` bytecode
run re-entrantly from native: it allocates an `Iterator` and holds the element on its operand
stack across that allocation/GC, so the held element ref goes stale before its `hashCode()` is
read. (Root home: `vm/src/vm/vm_exec.rs::invoke_virtual` / interpreter operand-stack roots
across a native→bytecode re-entry GC.) Affects any native that re-enters allocating bytecode
which then hashes a relocated identity-hash object.

### Layer 2 (IMPLEMENTED — `native-collections/src/lib.rs::native_unmod_hash_code`)
For a List-shaped immutable wrapper (`List.of(..)` → `cratonvm/internal/UnmodifiableList` over
a synthetic `ArrayList`/`LinkedList` backing) compute the `List.hashCode()` contract **directly
over the backing's element array via `native_al_hash_code`/`element_hash_code`**, instead of
delegating to `invoke_virtual(backing,"hashCode")` (which re-enters the buggy allocating real
`AbstractList.hashCode`). The native path reads each element fresh from the array and hashes it
immediately — no element is held across an allocation/GC — so it sidesteps the Layer-1 defect.
Guarded to ordered list backings so Set/Map wrappers keep the unordered contract.

This stabilises `Namespace.hashCode()` and unbreaks JUnit-6's `NamespacedHierarchicalStore`
(broad blast radius across the JUnit-6 suites).

**Verification status:** compiles clean; trivially correct (`List.of(x,o).hashCode() ==
Objects.hash(x,o)` and stable under forced GC). **End-to-end Hibernate verification is
PENDING** — at diagnosis time the shared worktree had an unrelated, in-progress module-info
parsing change (`classloading/src/module.rs`, a *different* concurrent task) that broke the
log4j-core module-export check and prevented the Hibernate harness from starting. Re-run the
repro once that settles:
```
cratonvm --nojit -Dlog4j2.configurationFile=<sql.props> @common.args \
  CratonRunner <list-with-MultiLevelCascadeCollectionEmbeddableTest> 0   # expect ok=2
```

Relevant prior art: `vm/src/runtime/interpreter.rs` ~11231/23942 and
`vm/src/runtime/invokedynamic.rs` ~1941 already document the JUnit-6
`Namespace.hashCode()`/`CompositeKey` instability theme and the identity-hash intrinsic
poisoning class — this is the same family, surfacing through the synthetic-collections
re-entry rather than the intrinsic IC.

## Triage

Real, deterministic, JIT-independent. **Not** a composite-key/reflection bug (correct the
original filing). The composite-id tests are just the messenger: any `@TestInstance(PER_CLASS)`
Hibernate test that caches state across methods via the JUnit store and runs enough allocation
to GC between methods is at risk. Mid-high priority — the store corruption can mis-fire other
JUnit-6 suites in subtle ways.
