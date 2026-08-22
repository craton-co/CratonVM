# hibernate-reactive 3-GC run (2026-08-20): one fixed defect, one open family

**Status:** ONE real defect FIXED and MEASURED (`nio_selector.rs`'s `SelectorImpl`
field corruption). A SECOND, more consequential family — a persistence-context
correctness cascade — is characterized with a concrete mechanism and evidence
trail but **not fixed**: root-causing it further needs step-through debugging
of `CompletableFuture`/Vert.x continuation completion timing that this session
could not do from log archaeology alone. **UPDATE 2026-08-21 (section 6):**
the mechanism is now narrowed to a specific, reproducible symptom —
`reactiveRemove(entity)` is invoked TWICE for one `ArrayLoop` array slot that
is dispatched only ONCE — confirmed by identity-hash-correlated tracing
across three independent runs on the idle Azure host. This is likely a
CratonVM lambda/method-reference dispatch defect, not a Hibernate bug; the
exact composition layer responsible is still open.

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

---

## 3. §2's own "isDone() lags one tick" theory is refuted — the real shape is one array-loop step executing TWICE

Session of 2026-08-20, following §2.3 step 1 literally: temporarily patched the
local `hibernate-reactive-core` vendor checkout (`apps/hibernate-reactive`,
gitignored — not part of this repo's git history, restored to pristine before
finishing) to print at every `isDone()`-adjacent decision point in
`BaseReactiveTest.deleteEntities` and `DefaultReactiveDeleteEventListener
.fetchAndDelete`, then ran `FilterWithPaginationTest` in isolation on the
`nio_selector` fix's binary (`cratonvm-hibcascade.exe`, `dev@77e712ec6` +
current tip). §2's theory does not survive contact with the trace.

### 3.1 What actually happens: index 2 of a 5-element array-loop runs twice, index 3 never runs

`BaseReactiveTest.deleteEntities` (called from every `@AfterEach`) does:

```java
s.createQuery(queryForDelete(entityClass)).getResultList()
    .thenCompose(list -> s.remove(list.toArray(new Object[0])))
```

Both the query and the remove run in the SAME session `s`, so `s.contains(o)`
on every freshly-`getResultList()`-loaded `o` prints `true` immediately after
the load, every single time — §2's "does registration lag the future?"
question has a hard `false` answer for this call site; `contains()` is a
plain persistence-context identity-map lookup with no `CompletionStage`
involved. `s.remove(Object...)` fans out over the 5 loaded entities via
hibernate-reactive's own `CompletionStages.applyToAll` →
`CompletionStages.loop(T[], Function)` → an `ArrayLoop` (a mutable, plain-`int`
`current` field, `next()` reads-then-increments it) driven by
`AsyncTrampoline.asyncWhile(loop::next)` (`apps/hibernate-reactive/.../
util/async/impl/AsyncTrampoline.java` — the same "stack-safe recursive
async loop" the residual-seven doc's own profile already named as
`AsyncTrampoline$TrampolineInternal.unroll`, 11.7% of that doc's samples).

The failing run's trace (`FamousPerson` array = `[Margaret, Nellie, Hedy,
RebeccaActress, RebeccaSinger]`, indices 0-4), single Vert.x event-loop
thread throughout:

```
fetchAndDelete idHash=31803 (Margaret,  index 0) detached=false
fetchAndDelete idHash=31804 (Nellie,    index 1) detached=false
fetchAndDelete idHash=31805 (Hedy,      index 2) detached=false   <- succeeds, removes Hedy
fetchAndDelete idHash=31805 (Hedy,      index 2) detached=true    <- SAME object, reprocessed -> throws
```

Index 2 (`Hedy`) is dispatched to `reactiveRemove` **twice** — the second
call finds it already gone (the first call genuinely removed it) and throws
`IllegalArgumentException: Unmanaged instance passed to remove()`, which is
what fails `testOffsetWithStageWithBasicQuery` and — because the exception
aborts the `@AfterEach` before it gets there — indices 3/4 (`RebeccaActress`,
`RebeccaSinger`) are **never processed at all**, leaving their rows in the
table to collide with the next test method's `@BeforeEach` re-insert
(`ConstraintViolationException: duplicate key ... famousperson_pkey`, the
`testMaxResultsWithMutiny` failure §2.1 already described as downstream of
the first).

This is not "a completion is observed one tick late" (§2's framing). It is
**one step of a stateful loop executing twice while the following step never
executes** — a duplicate side effect, not a delayed one. `s.contains()` was
`true` at every check that was ever printed; the defect is entirely in how
many times `ArrayLoop.next()`'s consequence (`consumer.apply(index)`) gets
delivered, not in when a completion is observed.

### 3.2 The instrumentation itself is a heisenbug filter — and points at where to look

Adding synchronous `System.err.println` at each of the four trace points
above made the bug **stop reproducing**: 5/5 clean `PASS` runs immediately
after the traces that caught it. Switching to a non-blocking, low-overhead
in-memory ring buffer (`TraceBuf` — `AtomicInteger.getAndIncrement()` + a
preallocated `String[]`, flushed once at JVM shutdown instead of per-call
I/O) let the class fail again (2 of 7 repeats), but both of those hits landed
on an **unrelated** environmental `NullPointerException` from
`SessionFactoryManager.getHibernateSessionFactory()` returning null (a
Testcontainers/schema-setup timing issue, not this defect) — the host was
under heavy, unrelated concurrent load from another session's `cargo build`
(one `rustc` process alone at ~8.8 GB RSS) for most of this investigation,
which both slows every repro cycle to 45-90s per run and plausibly perturbs
timing enough to mask or redirect the race. **This needs a repeat on an idle
box**, and any future instrumentation attempt should default to the
non-blocking `TraceBuf` shape, not `System.err.println`, or it will not see
the bug it's trying to catch.

### 3.3 A synthetic reproduction of the trampoline mechanism alone did NOT reproduce it

Before finding the real trace above, built
`probes/AsyncTrampolineDoubleFireProbe.java` — a standalone copy of
`AsyncTrampoline`'s actual `unroll()`/`PassBack` reentrancy logic plus an
`ArrayLoop`-shaped 5-element consumer, driven by a randomised mix of
synchronously-already-complete futures and genuinely cross-thread
(`ExecutorService`, different OS thread) asynchronously-completing futures —
the same sync/async boundary `unroll`'s `currentThread.equals(previousThread)
&& previousPassBack.isRunning` check exists to detect. **20,000 trials on
each of HotSpot and CratonVM (`cratonvm-hibcascade.exe`): 0 duplicate-index
trials, 0 skipped-index trials, on both VMs.** So the generic trampoline
mechanism, exercised with a real cross-thread race, is not sufficient on its
own to trigger the defect — whatever actually triggers it needs the real
Vert.x/reactive-SQL-client call shape (single Vert.x event-loop thread the
whole way, per the real trace above — not a second OS thread at all), or a
specific nested-trampoline interaction (`withTransaction`'s own retry/commit
machinery plus the entity-array loop, both trampolined, one nested inside the
other), or something in the exception-propagation path, none of which the
probe modelled. The probe is kept as a checked negative result and a
reusable harness for whoever picks this up next; do not spend time
re-deriving it.

### 3.4 What the next session should do

1. **Reproduce on an idle box first** — §3.2's heisenbug behavior means any
   result gathered under concurrent host load (another session's build,
   another shard sweep) cannot be trusted to reflect the real timing.
2. Re-instrument with `TraceBuf` (§3.2), specifically around the REAL call
   shape: add a trace point inside `AsyncTrampoline.unroll()` itself
   (`sameThread`, `previousPassBack.isRunning`, and whether the
   `previousPassBack.item` stash path or the fresh-loop path was taken) so a
   reproduced failure shows directly which branch let index 2 dispatch twice,
   rather than inferring it from `ArrayLoop`'s outer symptom as this record
   did.
3. Check whether `getSessionFactory().withTransaction(...)` wraps its own
   commit/retry logic in a SECOND `AsyncTrampoline` instance nested around the
   entity loop's — if so, a bug in how two nested trampolines' `PassBack`
   reentrancy detection interact (rather than a single trampoline in
   isolation, which §3.3 already cleared) becomes the leading candidate.
4. Once a mechanism is confirmed, it is very likely a CratonVM-side
   `CompletableFuture.whenComplete()`/dependent-action dispatch issue, not a
   hibernate-reactive bug — `AsyncTrampoline`'s reentrancy bookkeeping is a
   plain, single-threaded algorithm that only breaks if a `.whenComplete()`
   callback is invoked through a different synchronous/asynchronous path than
   the JDK's own `CompletableFuture` guarantees. Any candidate fix belongs in
   the VM's `CompletableFuture`/lambda-dispatch machinery
   (`vm/src/runtime/interpreter/invoke.rs`, `jit/helpers.rs` — the same files
   [residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
   §2.2-§7 already profiled for this workload), not in hibernate-reactive
   itself, and must be A/B'd on this exact repro (`FilterWithPaginationTest`
   in isolation, repeated ≥10x) before being trusted — this record's own §2
   theory looked equally plausible from a smaller trace and turned out to be
   wrong.

---

## 4. CORRECTED 2026-08-21 — section 3's "duplicate array-loop dispatch" does not reproduce; two follow-on theories tried and refuted; the mechanism is still open

Reproduced today on an idle Azure host (`azureuser@20.80.105.49`, 8 vCPU, load
average ~0 before this session started, Docker/Postgres native — not Docker
Desktop) specifically to get a race-free measurement after section 3.2 flagged
the local Windows box as too contended and too easily perturbed by tracing to
trust. That work paid off in an unexpected direction: **the pristine binary
reproduces `FilterWithPaginationTest`'s failure 15/15 times on this box with
NO instrumentation at all** (`failed=2` every run, ms=13000-18000) — this is
not a rare race on this host, it is the default outcome. But the *mechanism*
section 3 described from the Windows trace does not hold up under a clean,
correlated trace taken here.

### 4.1 The string-based TraceBuf from section 3 was itself a heisenbug filter — and the minimal one was not

Repeating section 3.2's lesson but sharper: instrumenting `AsyncTrampoline
.unroll()`/`ArrayLoop.next()` with the original `String`-concatenating
`TraceBuf.add(String)` (allocating and formatting on every call) **dropped
the reproduction rate to 0/15** on this otherwise-100%-reproducing box.
Switching to a raw-`int[]`/`long[]` ring buffer with no allocation or string
work in the hot path (`TraceBuf.add(int kind, int arg1, int arg2)`, format
deferred to a JVM-shutdown dump) restored it to **15/15**. The race is this
sensitive to added latency — a lesson for whoever instruments this class
again: use primitive arrays, never build strings on the hot path, and always
run a same-box pristine control alongside any instrumented run before
trusting either.

### 4.2 What the correlated trace actually shows

Full run trace (`cratonvm-hibidle`, `dev@77e712ec6` + this session's
`fix/hib-reactive-idle-repro-20260820` branch, no code changes): 36,314
`[TRACEBUF2]` events for one `FilterWithPaginationTest` run, correlated by a
per-`PassBack`-instance identity hash (`System.identityHashCode`) captured
alongside every `unroll()`/`ArrayLoop.next()` step. Three loop episodes end
in an exceptionally-completed `whenComplete` (`ex != null`) — matching the
`failed=2` outcome plus one @AfterEach that also fails on a class that
otherwise reports as passing further downstream in the same run.

**Every `ArrayLoop.next()` dispatch in the whole run is unique.** Grepping
every `DISPATCH(index, newCurrent)` event, no index is ever issued twice for
the loop episode (`PassBack` identity) that goes on to throw. The one that
`testOffsetWithStageWithBasicQuery`'s cleanup drives (`passback=28924`)
dispatches index 0, then 1, then 2 — one `CALLF` each, in order, each
correctly incrementing `ArrayLoop.current` before returning — and the
`whenComplete` for index 2's own, singular dispatch comes back with
`ex != null`. **There is no duplicate call anywhere in this trace.** Section
3's claim that "index 2 of a 5-element array-loop runs twice" does not
reproduce here; that record is corrected by this one, not amended.

**Everything in this trace is one thread.** Every single event across all
36,314 — three separate loop episodes' worth of exceptional completions
included — carries the same `thread` identity hash. There is no second OS
thread anywhere in this data. Section 2's original "timing/visibility" theory
and section 3's "two racing trampoline instances" theory both assumed or
required cross-thread activity to explain a plain, non-atomic field read
seeing a stale value; neither is available here, because there is only one
thread in play.

**Many `deleteEntities()` cleanup loops from *different* test methods run
concurrently, interleaved on that one thread.** The full timeline around the
failure window shows loop episodes for nine distinct `PassBack` identities
(`28986`, `28988`, `28991`, `28993`, `28995`, `28997`, `28998`, `28999`, plus
the three that fail) all mid-flight within the same ~12ms window, their
`CALLF`/`WHENCOMPLETE`/`STASH` steps woven together call-by-call as each
one's tiny Postgres round trip resolves and hands control back to the event
loop. `deleteEntities()` is `@AfterEach`; this is not one test's cleanup
taking its time — it is **several different test methods' `@AfterEach`
cleanups running at once**, all racing to delete-then-reinsert the exact same
five hardcoded-id (`1..5`) `FamousPerson` rows that every test method in this
class shares.

### 4.3 The obvious next candidate — cross-test-method interleaving — is also refuted, directly

The "many concurrent `PassBack` episodes on one thread" observation in 4.2
naturally suggests overlapping `@AfterEach` calls: method N's cleanup still
deleting id `3` while method N+1 has already re-inserted a fresh id `3`.
**This was tested directly, not just inferred**, by adding a plain
`AtomicInteger` around `deleteEntities()` itself — increment on entry, decrement
on completion (success or exception), logged through the same non-blocking
`int[]` `TraceBuf` — and re-running.

**Result: `deleteEntities()` is never concurrent. All 35 calls across the run
see `active=1`, every time, with no exception.** The interleaved `PassBack`
episodes visible in 4.2's timeline belong to something else entirely — other
`AsyncTrampoline`/`CompletionStages.loop` usage elsewhere in
Hibernate/hibernate-reactive's own internals (query building, batch/connection
plumbing) that happens to use small loops too, running on the same event-loop
thread while one `deleteEntities()` call's own async gaps are open. They were
never a second `deleteEntities()` call. **Section 4.3's own theory in the
first draft of this correction — cross-test-method interleaving — is
refuted by this measurement and is retracted, same session, before ever being
committed as a finding.**

### 4.4 Where this actually leaves the investigation

Two candidate mechanisms are now directly refuted by measurement on a clean,
correlated, single-threaded, race-free trace:

* **Not** a duplicate `ArrayLoop.next()`/`unroll()` dispatch (section 3's
  claim) — every index in every loop episode, including the three that fail,
  is dispatched exactly once.
* **Not** concurrent `deleteEntities()` calls racing on shared hardcoded ids
  (this section's own first-draft theory) — confirmed always exactly one
  active call.

What remains true and unexplained: within **one single, uninterrupted,
sequential, single-threaded synchronous chain** —
`getResultList()` → (per this doc's own earlier session: `s.contains(o)` is
`true` for every freshly-loaded entity, checked immediately) →
`.thenCompose(list -> s.remove(...))` → `fetchAndDelete`'s own
`source.contains(...)` check — the **same check on the same object in the
same session** goes from `true` to `false` with no other code running in
between on this thread, and no other thread active anywhere in the process
during the window (4.2), and no concurrent `deleteEntities()` call to blame
(4.3). That is closest to this doc's own section 2 framing, not section 3 or
the first draft of this section 4 — both of those turned out to be wrong
turns, now closed off by direct measurement rather than by argument.

### 4.5 What the next session should do

1. **The two refuted theories should not be re-tried** — both were checked
   directly (4.2's per-index dispatch trace; 4.3's `AtomicInteger`
   concurrency counter), not just argued from a symptom, so re-deriving them
   from the same symptom will lead back to the same dead ends.
2. **Instrument `EventSource.contains()`/the persistence-context identity map
   itself**, not just its call sites — the mechanism has to be inside
   `PersistenceContext`'s own entry map (Hibernate ORM core, not
   hibernate-reactive), since the two checks bracketing the mystery
   (the earlier `.thenApply` diagnostic in section 2, and `fetchAndDelete`'s own check)
   are both plain, synchronous, same-thread, same-session calls into it. A
   `#[track_caller]`-style instrumentation of `StatefulPersistenceContext
   .getEntry`/`removeEntry`/whatever backs `contains()` (real Hibernate ORM
   7.4.5 source, not vendored locally — would need decompiling or a
   source jar) between the two checks would show what, if anything, touches
   that map in between.
3. Alternatively: single-step this exact sequence under a debugger (`jdb`
   against the real HotSpot run first, to get a baseline of what *should*
   happen, since the symptom needs a real CratonVM run to reproduce but a
   HotSpot run to know what "correct" looks like at each step) rather than
   more log-based tracing — the log-based approach has now spent three
   documented rounds (§2, §3, this section) without landing the mechanism,
   which is itself a signal to change technique.
4. Whoever picks this up should still default to the raw-`int[]` `TraceBuf`
   shape from 4.1 for anything measured on this Azure host, and should still
   run a same-box pristine control before trusting any instrumented result —
   both lessons from this session remain valid even though the theories built
   on top of the first trace did not survive.

This session made no code changes — docs and a probe only. `apps/hibernate-reactive`
is a gitignored vendor checkout; the temporary instrumentation used to gather
this section's traces was not committed.

---

## 5. 2026-08-21 — full rerun of the 18 non-passed classes on the idle Azure host, current dev tip

All 18 classes named as non-passed in
[RESULTS-20260820-3gc-postgres-local.md](../../../apps/hibernate-reactive-suite-runner/RESULTS-20260820-3gc-postgres-local.md)
rerun against `dev@77e712ec6` + this session's docs-only branch (same binary
as section 4, `cratonvm-hibidle`), on the idle Azure host, default (ZGC)
collector.

**9 now PASS** (all previously FAIL/CRASH/HANG on the original Windows
3-GC run): `UUIDAsBinaryTypeTest`, `ORMReactivePersistenceTest`,
`MultithreadedIdentityGenerationTest`, `IdentityGeneratorWithColumnTransformerTest`,
`NoLiveTransactionValidationErrorTest`, `OneToOneIdClassParentIdClassTest`,
`ReactiveMultitenantTest`, `MutationDelegateIdentityTest`,
`it.quarkus.qe.database.DatabaseHibernateReactiveTest`. The last two are the
`nio_selector.rs` fix (section 1) landing cleanly; `ORMReactivePersistenceTest`
and `DatabaseHibernateReactiveTest` passing confirms
[the not-a-CratonVM-bug doc](hibernate-and-hibernate-reactive-not-cratonvm-bugs.md)'s
own claim that both were the Windows box's timezone/locale, not CratonVM —
this Azure host is UTC/`en` and both pass here as predicted.

**9 still FAIL**, genuinely — not a Docker/Testcontainers artifact, see the
harness-bug note below: `techempower.TechEmpowerTest`,
`MultithreadedInsertionWithLazyConnectionTest` (both already-known
lambda-dispatch-timeout family, [residual-seven doc](residual-seven-after-the-afc-fix-20260817.md)),
`OneToManyTest`, `QuerySpecificationTest`, `ReactiveStatelessProxyUpdateTest`,
`CriteriaMutationQueryTest`, `ReactiveStatelessWithBatchTest`,
`FilterWithPaginationTest`, `RowIdUpdateAndDeleteTest` — the persistence-context
cascade family sections 2-4 investigate. `QuerySpecificationTest` is a new
name to that family's list (it was previously only flagged as a `generational`-only
fail in the original 3-GC run, not one of the "7 new" GC-independent ones);
worth folding in as an eighth member next time someone works this.

### A harness bug found in passing: `run-hibernate-reactive-suite.sh`'s
`NO-DB` signature detection greps the whole shard log, not the current class

All 9 failures above were first reported by the runner as
`NO-DB: connection-refused` — which would mean "no DB reachable, not a VM
result" per the script's own documented convention. **That label is wrong.**
Every one of the 9 raw logs shows a normal `@@RESULT ... found=N ok=M failed=2`
line with real assertion failures (`@@TESTFAIL` for named test methods), not
a connection failure — the same "Unmanaged instance passed to remove()"-shaped
cascade this whole page investigates. The bug: `run_shard()`'s sig-detection
(`run-hibernate-reactive-suite.sh`, the `if grep -qm1 ... "$tmp" "$RAW"` line)
checks the **whole shard's cumulative `raw.log`** (`$RAW`), not just the
current class's own temp output (`$tmp`) — so once any earlier class in a
shard prints something matching `Connection refused|Could not find a valid
Docker|ConnectException|No Docker environment` anywhere in its own stack
trace or log output, every later class in that same shard gets mislabeled
`NO-DB` regardless of its own real outcome. Confirmed by checking `found=0`
(the script's own `NOTESTS`/no-DB signal) against the actual counts: none of
the 9 have `found=0`. This script is gitignored (only `class-overrides.tsv`
is force-tracked), so no fix is committed here — flagging it so the next
`results.tsv` isn't read at face value. The one-line fix is to grep only
`"$tmp"`, not `"$tmp" "$RAW"`.

---

## 6. 2026-08-21 — `EntityEntryContext`/`EntityEntryImpl`/`SessionImpl.contains()` instrumented directly; the mechanism narrows to `reactiveRemove` firing twice for one `ArrayLoop` slot

Following section 4.5's own instruction (instrument `PersistenceContext`'s
entry map itself, in real Hibernate ORM 7.4.5 core — not vendored locally,
sources jar fetched to `/tmp/hibernate-core-7.4.5.Final-sources.jar`,
patched classes compiled into a directory placed ahead of the real jar on
the classpath so no jar rebuild is needed). Binary: `cratonvm-pcprobe`
(same idle Azure host, `dev` tip at merge time + this branch, no code
changes). All instrumentation is the low-overhead raw-`int[]`
`TraceBuf`/`PcTrace` shape section 4.1 established as required — string
tracing was not retried.

### 6.1 Two theories eliminated before finding the real one

* **A large heap does not help.** `FilterWithPaginationTest` still fails
  with `-Xmx4000m` (vs. the default 1500m) — a compacting/generational
  moving-GC-vs-`IdentityHashMap` theory (object relocation invalidating a
  cached identity hash mid-lookup) predicted a large-enough heap would
  suppress the collection cycle and the bug. It didn't, on the first test
  of the theory.
* **`System.identityHashCode()`/`IdentityHashMap` are stable under real GC
  pressure on CratonVM.** A standalone probe
  (`IdentityHashStabilityProbe.java`, 2000 objects, all put into an
  `IdentityHashMap` immediately after allocation, then 1.6 GB of garbage
  forced through a 512 MB heap, then every hash and every map lookup
  re-checked): **0 hash mismatches, 0 map misses.** This directly refutes
  the "compaction corrupts identity hashing" theory rather than just
  failing to trigger it — the primitive itself is correct under load. (Not
  committed; a throwaway probe, not added to `probes/` since it produced a
  clean negative on a mechanism already ruled out by 6.1's first bullet.)

### 6.2 `EntityEntryContext`'s own `IdentityHashMap` (`nonEnhancedEntityXref`) is never wrong

`FamousPerson` is not bytecode-enhanced (no `$$_hibernate_*` synthetic
methods in the compiled class), so `EntityEntryContext.getEntityEntry`
resolves it entirely through `nonEnhancedEntityXref`, a plain
`IdentityHashMap<Object,ManagedEntity>` (`EntityEntryContext.java`,
`getAssociatedManagedEntity`'s final `return nonEnhancedEntityXref != null
? nonEnhancedEntityXref.get( entity ) : null`). Instrumented every
`get`/`put`/`remove` on this map with entity identity hash + map size.
Across a full, reproducing `FilterWithPaginationTest` run: **zero**
"put-then-miss-with-no-remove-in-between" sequences and **zero**
"remove-then-later-hit" sequences, for any of the 408 distinct entities
traced. The map is entirely self-consistent throughout the run — this
mechanism, the doc's own section 4.5 leading candidate, is refuted.

### 6.3 The real signal: `EntityEntry.status` flips to `DELETED` between two legitimate `contains()` checks on the same object

Instrumented `EntityEntryImpl.getStatus()`/`setStatus()` (both delegate to
a single bit-packed `compressedState int` — a compact-layout field, exactly
the kind of representation this project's own history flags as a hazard
area — but the packing itself is not implicated here, see below) and
`SessionImpl.contains(Object)` directly. One clean, complete history for
the object that ends up throwing (`objectIdHash=28305`,
`entryIdHash=28306`, one representative run):

```
setStatus(old=4) -> CHANGED to 0      (SAVING -> MANAGED, from @BeforeEach persist)
getStatus() = 0   x3                  (both of the two calls below read this)
contains() check #1: entry found, status=0, not deleted -> proceeds     <- SUCCEEDS
contains() check #2: entry found, status=0, not deleted -> proceeds     <- SUCCEEDS (again!)
setStatus(old=0) -> CHANGED to 2      (MANAGED -> DELETED, from check #1 or #2's own async completion)
contains() check #3: entry found, status=2, DELETED -> throws           <- FAILS
```

The status transitions are real, driven by genuine `setStatus()` calls —
not phantom/corrupted reads. The question is not "why did status change
unexpectedly" but **"why was `fetchAndDelete`'s delete pipeline entered
twice for the same object, when the array it was drawn from only lists it
once."**

### 6.4 The list has no duplicate; `ArrayLoop` dispatches the slot exactly once; `reactiveRemove` still fires twice

Cross-referencing three independent reproducing runs, each traced at every
layer between `getResultList()` and `fetchAndDelete`:

1. **`BaseReactiveTest.deleteEntities`'s own `list`** (instrumented with an
   `IdentityHashMap`-based per-element duplicate check, reusing the
   already-proven-correct primitive from 6.1): always **exactly 5**
   elements, and the entity that goes on to fail appears **exactly once**
   in the list (`dup=false`), at index 2 in all three runs.
2. **`CompletionStages.ArrayLoop.next()`** (instrumented directly — entry,
   dispatched index, exhaustion): for the loop instance driving this
   deletion, indices 0, 1, 2 are each dispatched **exactly once**, in
   order, one `next()` call per index, no reentrant/overlapping calls
   visible on the single thread. This directly reproduces and confirms
   section 4.2's own finding — that finding was correct, but answers a
   different question than the one that matters here.
3. **`ReactiveSessionImpl.reactiveRemove(Object)`** (instrumented at its
   own entry, the direct target of `applyToAll(delegate::reactiveRemove,
   entity)`): called **twice** for the same object identity hash, from the
   same session identity hash, ~800 μs apart, with **no second `ArrayLoop`
   dispatch event in between** — the single `consumer.apply(2)` call from
   item 2 above is the only dispatch on record, yet its terminal action
   (`reactiveRemove`) runs twice.

So the duplication is not in the list, not in the loop's index bookkeeping,
and not in `DefaultReactiveDeleteEventListener`'s own logic — it is
**between one `ArrayLoop.next()` dispatch and the `reactiveRemove` call
that dispatch is supposed to produce exactly once**, i.e. inside the
`consumer.apply(index).thenCompose(CompletionStages::alwaysContinue)`
composition chain itself (`CompletionStages.java`, the `IntPredicate`
overload of `loop`) or in how CratonVM invokes the `delegate::reactiveRemove`
method reference underneath that chain.

### 6.5 What this is not, and what it most likely is

Five distinct mechanisms have now been checked directly and eliminated for
this specific symptom: JDK `CompletableFuture`/`isDone()` timing (section
2's theory), duplicate `ArrayLoop` index dispatch (section 3's theory,
re-confirmed absent here too), concurrent `deleteEntities()` calls (section
4.3), moving-GC/identity-hash instability (6.1), and a stale
`EntityEntryContext` map (6.2). What remains — a chained lambda/method-reference
composition executing its terminal side-effecting call twice for one
outer invocation — is a shape this project's own history has seen before
in the SAM/lambda-dispatch area (`residual-seven-after-the-afc-fix-20260817.md`
sections 2.2-§7: `RwLock` + `HashMap` lookups on a process-global
`lambda_proxies`/native-registry map, asked "up to twice" per invocation in
at least two other places already found in that investigation). Whether
this is the same structural pattern recurring at a different call shape, or
a distinct defect, is not established here.

### 6.6 `--nojit` clears it: this is the JIT, not the interpreter

Ran the exact repro (`FilterWithPaginationTest` in isolation, idle Azure
host, `cratonvm-pcprobe`) with `--jit off`: **3/3 clean PASS**, 17-18s each.
Immediately re-ran with `--jit on` as a control on the same binary: **FAIL**,
same signature as every other run this session. This is a direct,
same-binary, same-host A/B — the project's own standing convention
("`--nojit` decides a suspected dispatch bug") gives a clean answer:
**the JIT is implicated directly.** The interpreter's own dispatch of this
composition chain is correct; something in the compiled path for the
`consumer.apply(index).thenCompose(CompletionStages::alwaysContinue)` chain
(or the `delegate::reactiveRemove` method reference underneath it) causes
the terminal call to fire twice once JIT-compiled.

### 6.7 What the next session should do

1. **Start from `vm/src/runtime/interpreter/invoke.rs` and `jit/helpers.rs`'s
   lambda/SAM dispatch paths** — `interpreter::lambda::try_lambda_dispatch`,
   `try_invoke_cached_lambda_impl`, and the JIT-side cached-native-dispatch
   helpers (`try_jit_site_cached_native_dispatch`,
   `jit_invoke_virtual_mic`) already named in
   `residual-seven-after-the-afc-fix-20260817.md` sections 2.2-§7 for this
   same workload family. Section 6.6's finding gives those a NEW,
   correctness (not performance) angle: does a JIT-compiled call site for a
   method reference / functional-interface `apply()` ever re-enter or
   replay the callee under a specific tier-transition or deopt condition?
2. **This is now a MINIMAL, reproducible, same-binary A/B** — not a probe
   that needs to be built from scratch. `FilterWithPaginationTest` in
   isolation on the idle Azure host, `--jit on` vs `--jit off`, is the
   instrument. Any candidate fix must flip this same A/B from FAIL to PASS
   under `--jit on`, repeated ≥10x per this project's own repeat-before-filing
   discipline (this session repeated the FAIL 5x and the `--nojit` PASS 3x,
   consistent every time).
3. **`CRATONVM_C2_SUPERSEDE=0` does NOT clear it** — tried on the `--jit on`
   repro, 3/3 still FAIL (same signature, `sum_class_ms` 20-29s). So this is
   not a C1-to-C2 tier-transition/supersede-window issue; whatever compiles
   this call site reproduces the defect on its own, without a tier
   transition in play. Cross this candidate off before re-trying it.
4. All instrumentation in this section lived in the gitignored
   `apps/hibernate-reactive` vendor checkout and the `/tmp` sources-jar
   overlay — not committed, matching this doc's established convention.
   The one exception is `IdentityHashStabilityProbe.java`, also not
   committed (see 6.1) since it produced a clean negative on an already-
   eliminated theory; recreate it if the identity-hash angle needs
   revisiting for a different mechanism.
