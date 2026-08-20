# hibernate-reactive 3-GC run (2026-08-20): one fixed defect, one open family

**Status:** ONE real defect FIXED and MEASURED (`nio_selector.rs`'s `SelectorImpl`
field corruption). A SECOND, more consequential family — a persistence-context
correctness cascade — is characterized with a concrete mechanism and evidence
trail but **not fixed**: root-causing it further needs step-through debugging
of `CompletableFuture`/Vert.x continuation completion timing that this session
could not do from log archaeology alone.

**Baseline:** the previous known-good state is
[residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
(2026-08-17) — the hibernate-reactive Windows FAIL bucket at 7 classes, 2 of
which are the host's own timezone/locale (not CratonVM), 5 of which are one
open lambda/`CompletableFuture`-dispatch performance characteristic.

**Regression surfaced by:**
[RESULTS-20260820-3gc-postgres-local.md](../../../apps/hibernate-reactive-suite-runner/RESULTS-20260820-3gc-postgres-local.md)
— the first full run of all 249 classes against a live Postgres container on
3 collectors (ZGC/G1/Generational), at `dev@26e4b5db4`. 10 classes FAIL on
every collector (GC-independent); of those, 2 are the already-known host
issues (`ORMReactivePersistenceTest`, `DatabaseHibernateReactiveTest`) and
`techempower.TechEmpowerTest` is the already-known lambda-dispatch timeout.
**The other 7 are new** and are this record's subject:
`CriteriaMutationQueryTest`, `FilterWithPaginationTest`,
`MutationDelegateIdentityTest`, `OneToManyTest`,
`ReactiveStatelessProxyUpdateTest`, `ReactiveStatelessWithBatchTest`,
`RowIdUpdateAndDeleteTest`. All 7 were verified this session to **PASS
cleanly under real HotSpot** with the identical classpath/`common.args`
(`--hotspot`, wall-clock 16s for all 6 batched, vs. CratonVM's 2m18s for the
same 6) — confirmed CratonVM regressions, not upstream/environment gaps.

Not covered here (separate, lower-confidence, not reproduced in isolation):
the ZGC-only `@BeforeEach` timeouts (`NoLiveTransactionValidationErrorTest`,
`OneToOneIdClassParentIdClassTest`, `ReactiveMultitenantTest`) and the single
`IdentityGeneratorWithColumnTransformerTest` CRASH — re-run standalone with
the fixed binary, `IdentityGeneratorWithColumnTransformerTest` PASSED in
16s. Both patterns look like Testcontainers/Docker resource contention under
6-way concurrent shard load (matching `UUIDAsBinaryTypeTest`'s explicit
`NO-DB: Could not find a valid Docker environment` in the same zgc-only
bucket), not a collector-specific CratonVM defect. Re-run isolated + repeated
before spending more time on them.

---

## 1. FIXED — `Selector.open()` corrupted a real `SelectorImpl`'s `selectedKeys` field

**File:** `native-io/src/nio_selector.rs`. **Fix:** delete the two dead field
writes in `selector_open_native` and `selector_close_native`.

`selector_open_native` allocates a REAL `sun/nio/ch/SelectorImpl`
(`ctx.new_object("sun/nio/ch/SelectorImpl")`), then — despite the function's
own comment already explaining the id/open-flag lookup was moved to an
identity-hash side table (`sel_obj_ids()`) **specifically because writing
those slots as `Int` silently coerces to `null`** — still performed the
writes anyway, "kept as a best-effort legacy path but not relied upon":

```rust
ctx.set_field(obj, SI_ID, Value::Int(id));          // SI_ID = 0
ctx.set_field(obj, SI_OPEN_FLAG, Value::Int(1));     // SI_OPEN_FLAG = 4
```

`SI_ID`/`SI_OPEN_FLAG` (0/4) are reference-typed on this class
(`AbstractSelector.selectorOpen`, `SelectorImpl.selectedKeys` — confirmed via
`javap -p` field-index flattening and the developer's own comment). Per
[G30-1](../jdk-only/G30-1-the-silent-reference-slot-coercion-20260817.md)'s
`primitive-into-reference` coercion, an `Int` store there is silently
descriptor-coerced to `null`. `SelectorImpl.selectedKeys()` is `public final`
real JDK bytecode returning that field directly, so **every** `Selector.open()`
nulled its own `selectedKeys`, before any caller had a chance to touch it.
Confirmed at runtime with `CRATONVM_DBG_LAYOUT=1 CRATONVM_DBG_COERCION=1`
(backtrace resolves through `gc::heap::coerce_field_value_for_slot` →
`vm_exec::set_field` → `nio_selector::selector_open_native:2333`) —
class `sun/nio/ch/SelectorImpl`, `refs=8`, index 4 = `selectedKeys` (flattened
ref order: `AbstractSelector.{provider,cancelledKeys,interruptor}` then
`SelectorImpl.{keys,selectedKeys,publicKeys,publicSelectedKeys,cancelledKeys}`).

Across the full 08-20 ZGC run, this fired **~900+ times** under the
`store descriptor=L value=Int(1) class_id=<varies> index=4` signature — the
wide range of `class_id`s is the SAME bug, not many different classes: this
harness forks one JVM per test class, and `SelectorImpl`'s sequential class id
simply differs per process depending on how many other classes loaded first.

**Why most tests still passed anyway:** Netty's `NioEventLoop` reflectively
replaces `SelectorImpl.selectedKeys`/`publicSelectedKeys` with its own
`SelectedSelectionKeySet` immediately after opening a selector (a well-known
perf optimisation), which silently overwrites the `null` this bug left behind
— on the code path where that reflective swap runs cleanly. Whenever it
doesn't run in time or at all, direct (non-reflection) use of
`Selector.selectedKeys()`/`AbstractSelector.close()` sees the corrupted
`null`/wrong-slot state.

**Measured effect of the fix** (same binary flavor, `dev@26e4b5db4` +this
change, built as `target/release/cratonvm-hibreactregr.exe`):

| class | before | after |
|---|---|---|
| `FilterWithPaginationTest` | FAIL, 141.9s wall | still FAILs (different bug, §2) but **17.8s wall** — down 8x, now within noise of HotSpot's 14s |
| `MutationDelegateIdentityTest` | FAIL (`failed=2`) | **PASS** (5/5) |
| `IdentityGeneratorWithColumnTransformerTest` | CRASH (non-reproducing in isolation both before and after) | PASS, 16s |

No `native-io` test regressed: `cargo test -p cratonvm-native-io --release`
is 509 passed / 3 failed both before and after this change (the 3 failures are
pre-existing `io_tests::fis_*` `FileInputStream` cases, untouched by this
diff — confirmed via `git diff --stat` showing only `nio_selector.rs` changed).
`nio_selector::tests::*` (25 tests) all pass.

---

## 2. OPEN — a persistence-context correctness cascade, one failure per class then a cascading duplicate key

### 2.1 The shape, seen in every one of the 7 classes

Every failing class shows **the same two-step cascade**, not 7 independent
bugs:

1. **One test method's `@AfterEach` cleanup throws**, either
   `java.lang.IllegalArgumentException: Unmanaged instance passed to remove()`
   (`DefaultReactiveDeleteEventListener.fetchAndDelete`,
   `!source.contains(entityName, entity)`) or
   `org.hibernate.reactive.event.impl.UnexpectedAccessToTheDatabase`
   (`DefaultReactiveLoadEventListener.onLoad`, see §2.2). Either way the
   `@AfterEach`'s own `DELETE` statements never reach the DB (visible in the
   `-Dhibernate.show_sql=true` trace: the `SELECT` that `deleteEntities()`
   issues is there, the 5 `DELETE`s that normally follow it are not).
2. **The next `@Test` method's `@BeforeEach` re-inserts the SAME
   hardcoded-id fixture rows** (every affected class uses `new Foo(1L, ...)`-
   style fixed ids in a `@BeforeEach`/instance-field pattern), and now
   collides with the still-present rows from step 1:
   `ConstraintViolationException: … duplicate key value violates unique
   constraint "..._pkey"`.

So each class effectively has **one** root defect; the second reported
failure is a downstream consequence of the harness's `deleteEntities()`
convention, not a second bug. `results.tsv`'s `failed=2` columns for
`FilterWithPaginationTest`/`RowIdUpdateAndDeleteTest`/`OneToManyTest`/
`ReactiveStatelessWithBatchTest` are this shape exactly.
`CriteriaMutationQueryTest` shows the same shape.
`ReactiveStatelessProxyUpdateTest` is the one outlier — a plain 120s
`TimeoutException` in `testLazyInitializationExceptionWithMutiny`, `failed=1`
— not yet connected to the other six; may be a related hang or may be
independent. Not investigated further this session.

### 2.2 A concrete, evidenced mechanism for `UnexpectedAccessToTheDatabase`

`DefaultReactiveLoadEventListener.onLoad` (called from
`SessionImpl.internalLoad`, a **synchronous** Hibernate ORM core entry point
that hibernate-reactive bridges) says so in its own doc comment:

> Since this method is not reactive, we're not expecting to hit the database
> here (if we do, it's a bug) and so we can assume the returned
> `CompletionStage` is already completed.

It checks `checkId.toCompletableFuture().isDone()` immediately after issuing
the operation with **no await** — if the future is not synchronously
complete (e.g. because it truly needed to reach the DB), it throws
`UnexpectedAccessToTheDatabase` by design. This is a deliberate contract that
certain operations (an identity/cache lookup that should be a pure in-memory
hit) **complete inline, within the same stack frame**, never deferred to a
later event-loop tick.

**Working hypothesis, not yet confirmed by a step-through:** CratonVM's
`CompletableFuture`/Vert.x `Future` composition does not always complete
"already-resolved" chains inline the way HotSpot's does — this is the same
`CompletableFuture` composition surface the residual-seven doc already
measured at 100-300x HotSpot's *interpreter* cost
(§2.2-2.4 there), a PERFORMANCE finding. This record's contribution is that
the SAME divergence, if it also changes *when* (not just how fast) a chain
transitions to `isDone()==true`, would explain a CORRECTNESS symptom, not
just a speed one: an operation that finishes synchronously on HotSpot (so
`onLoad`'s assumption holds) could resolve one tick later on CratonVM (so the
assumption breaks and the guard fires). This would also explain
`Unmanaged instance passed to remove()`: `EventSource.contains(...)` walks
the same persistence-context/identity-map machinery, so a query result whose
persistence-context registration lands one continuation-hop later than
HotSpot would explain why the freshly-`getResultList()`-loaded entity isn't
found by `contains()` yet.

**What supports this over alternatives already ruled out:**
* Not the `nio_selector.rs` defect in §1 — reproduced identically on the
  binary carrying that fix.
* Not the `primitive-into-reference` coercion guard
  ([G30-1](../jdk-only/G30-1-the-silent-reference-slot-coercion-20260817.md)):
  a targeted `CRATONVM_DBG_COERCION=1`/`CRATONVM_DBG_LAYOUT=1` run of
  `FilterWithPaginationTest`, cross-referenced against
  `-Dhibernate.show_sql=true` timestamps around the failing query, found no
  coercion event class-resolvable to a hibernate-reactive entity or session
  type near the failure — the coercion traffic present (`class_id=158`,
  `132`, `73`, `488`, `10223`, tens of thousands of occurrences) is boot-time
  background noise unrelated to this window.
* Not upstream: all 7 classes PASS cleanly under real HotSpot with the same
  classpath (§0).
* `FilterWithPaginationTest`'s failing method
  (`testOffsetWithStageWithBasicQuery`) is the only one in its class using a
  **bare `OFFSET`, no `LIMIT`** query (`setFirstResult(3)` with no
  `setMaxResults`) — confirmed via SQL trace
  (`select … from FamousPerson fp1_0 offset $1 rows`, no
  `fetch first … rows only`). Two *other* methods in the same class also
  compile to bare-OFFSET SQL (`testOffsetWithFilterAndOrderByAndStage`-style,
  `… where fp1_0.status = 'LIVING' order by fp1_0.id offset $1 rows`) and did
  **not** fail in this run — so "bare OFFSET" correlates but is not
  sufficient on its own; a timing-sensitive race (only sometimes landing
  outside the synchronous window) fits the data better than a deterministic
  per-query-shape bug.

### 2.3 What the next session should do

1. **Confirm or refute the timing theory directly**: instrument
   `CompletableFuture`/Vert.x `Future` composition (or add a temporary
   `System.err` print at `DefaultReactiveLoadEventListener.onLoad`'s
   `isDone()` checks in a patched hibernate-reactive jar) to see whether the
   *same* logical operation is synchronously complete on HotSpot and not on
   CratonVM for one of these classes, reproduced in isolation.
2. If confirmed, the fix surface is almost certainly in how CratonVM resolves
   an already-satisfied `CompletableFuture`/`Future` continuation — check
   whether completion always schedules through the JIT/lambda dispatch path
   documented in
   [residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
   §2.2/§7 (`CompletableFuture.uniComposeStage`/`uniWhenComplete`) versus
   HotSpot's synchronous inline-complete fast path.
3. `ReactiveStatelessProxyUpdateTest`'s lone 120s timeout
   (`testLazyInitializationExceptionWithMutiny`) has not been triaged; check
   whether it is the same family (a stateless-session lazy-init check that
   never resolves) or independent before assuming it shares this cause.

Any candidate fix must be verified by re-running these 7 classes (not just
the probe it was found on) — this record's own investigation found the
coercion-guard angle looked promising from raw counts alone and did not pan
out under a targeted cross-reference; the same caution applies to whatever
comes out of step 1 above.
