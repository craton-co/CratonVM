# hibernate-reactive 3-GC run (2026-08-20): two defects, both now fixed

**Status: BOTH FIXED (2026-08-22).**

1. `nio_selector.rs`'s `SelectorImpl` field corruption — fixed and measured
   2026-08-20, section 1.
2. The persistence-context correctness cascade — `reactiveRemove(entity)`
   invoked TWICE for one `ArrayLoop` array slot dispatched ONCE — **root-caused
   and fixed 2026-08-22, section 8.** It is not a `CompletableFuture`/Vert.x
   continuation-timing question and it is not in the composition chain: the JIT
   lambda direct-call arm (`vm/src/jit/helpers.rs::try_lambda_site_direct_call`)
   DROPPED the reconstructed frame of a deoptimized lambda body and let its
   caller re-run that body from entry, re-executing every side effect the
   compiled body had already committed. `CRATONVM_JIT_DENY` bisected it to
   exactly one class (`CompletionStages$ArrayLoop`), `CRATONVM_JIT_LAMBDA_SITE=0`
   was the one feature switch of twelve that cleared it, and the fix routes the
   frame through the same `resume_deopted_body` the interpreter's one-shot door
   already used. **Six of the seven new regressions this page opened with are
   now measured PASS on the fixed binary** (`FilterWithPaginationTest`,
   `CriteriaMutationQueryTest`, `OneToManyTest`, `ReactiveStatelessWithBatchTest`,
   `RowIdUpdateAndDeleteTest`, and — a regression the runner's `passed.txt` had
   not caught — `QuerySpecificationTest`), section 8.6.

Sections 2-7 below are preserved as the trail that got there. Note that
**section 6.4's conclusion is invalidated by section 8.2** — its instrument
lived inside the very method whose compiled body is the defect, and adding it
suppresses the failure.

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

---

## 7. 2026-08-22 — `--jit off` swept across all 8 other classes from section 5's still-FAIL list: 6/8 confirmed the same defect, 2/8 are the already-known separate lambda-dispatch-timeout family

Section 6 established the JIT-dispatch mechanism on `FilterWithPaginationTest`
alone. This section runs the same `--jit on` vs `--jit off` A/B across the
other 8 classes section 5 listed as still genuinely failing (not the
`nio_selector.rs`-fixed six, not the two host-locale classes). Same idle
Azure host (load was NOT quiet this time — up to load average 31 from other
concurrent sessions' work partway through, see the build-time note below —
but this A/B is a coarse PASS/FAIL comparison, not fine-grained timing, so
host contention is not expected to change the verdict, only the wall-clock
numbers), fresh binary (`cratonvm-nojitsweep`, current `dev` tip + this
branch, no code changes), `--shards 8` (one class per shard), `--timeout
120` (the flat default — no per-class overrides applied, so the two
already-known slow classes are expected to hit the cap regardless of JIT
state; see below).

### 7.1 Six classes: clean PASS under `--jit off`, the exact same cascade shape under `--jit on`

| class | `--jit off` | `--jit on` | first exception (jit on) |
|---|---|---|---|
| `RowIdUpdateAndDeleteTest` | PASS 6/6, 31.4s | FAIL 4/6, 26.9s | `Unmanaged instance passed to remove()` |
| `OneToManyTest` | PASS 8/8, 29.2s | FAIL 6/8, 29.6s | `UnexpectedAccessToTheDatabase` |
| `QuerySpecificationTest` | PASS 52/52, 39.1s | FAIL 50/52, 37.1s | `IllegalStateException: Illegal pop() with non-matching JdbcValuesSourceProcessingState` |
| `CriteriaMutationQueryTest` | PASS 9/9, 28.0s | FAIL 7/9, 27.1s | `UnexpectedAccessToTheDatabase` |
| `ReactiveStatelessWithBatchTest` | PASS 24/24, 35.1s | FAIL 22/24, 31.1s | `UnexpectedAccessToTheDatabase` |

(`FilterWithPaginationTest`, section 6, is the sixth — `Unmanaged instance
passed to remove()`.)

Every `--jit on` failure is the exact two-step cascade section 2.1 first
described: one `@AfterEach`/mid-test failure (`Unmanaged instance passed to
remove()` or `UnexpectedAccessToTheDatabase` — both already-established
symptoms of the same underlying `contains()`-sees-a-status-it-shouldn't
mechanism section 6 characterized), then exactly one downstream
`ConstraintViolationException: duplicate key` from the next test method's
`@BeforeEach` re-inserting rows the failed cleanup never deleted. `failed=2`
on every one of the five FAIL rows above, matching the pattern exactly.
**`QuerySpecificationTest`'s first exception is a new shape**
(`IllegalStateException` about a JDBC-values-processing-state stack
mismatch, not `Unmanaged instance`/`UnexpectedAccessToTheDatabase`
directly) — plausibly a different symptom of the same root double-fire (a
second, unexpected re-entry into query-result processing rather than into
delete processing), not investigated further this session, but it fits the
"one call site fires twice" shape the section 6 finding already establishes
and is not evidence of a fourth, unrelated defect.

**This is the same defect, now confirmed on 6 of 6 classes checked, with the
same clean `--jit off`/`--jit on` A/B section 6 already established for
`FilterWithPaginationTest`.**

### 7.2 One class hung under `--jit on` that passed under `--jit off`: `ReactiveStatelessProxyUpdateTest`

`--jit off`: PASS 4/4, 28.4s. `--jit on`: HANG at the flat 120s cap (no
`class-overrides.tsv` entry raises it here). This is **not** a new finding —
section 2.1 already named this class as "the one outlier — a plain 120s
`TimeoutException` in `testLazyInitializationExceptionWithMutiny`,
`failed=1` — not yet connected to the other six" and explicitly left it
untriaged. This run doesn't resolve that; it only adds that the hang, like
the cascade, does not reproduce under `--jit off`, which is at least
consistent with (but does not prove) the same JIT root cause. Worth a
dedicated `--jit off` vs `on` timing/repeat check on its own before folding
it into section 6's finding as a seventh confirmed instance.

### 7.3 Two classes are unaffected either way — the already-known lambda-dispatch-timeout family, at the WRONG timeout for this check

`techempower.TechEmpowerTest` and `MultithreadedInsertionWithLazyConnectionTest`
are the two classes `class-overrides.tsv` deliberately does NOT give a
raised timeout to (its own comment: "The other three
(`MultithreadedInsertionWithLazyConnectionTest`, `it.LocalContextTest`,
`techempower.TechEmpowerTest`) are deliberately NOT here" — because their
own fixture code has a hardcoded Vert.x deadline no runner flag can reach,
per `residual-seven-after-the-afc-fix-20260817.md` section 2.1). At the flat
120s cap used here (no override), both HANG under `--jit off`;
`MultithreadedInsertionWithLazyConnectionTest` also HANGs under `--jit on`;
`techempower.TechEmpowerTest` returns a FAIL under `--jit on` at 17.7s, but
its failure (`AssertionError: Expected status code 200 or 204, but was
500`) is an HTTP-layer failure unrelated to the persistence-context cascade
— plausibly just this class not getting far enough into its own workload
before whatever caused the 500 (a fixture/setup issue at this short a
timeout, most likely), not a JIT correctness finding either way. **This
result is inconclusive for both classes at this timeout** — the already-
documented volume/lambda-dispatch-cost explanation for both stands
unchanged; re-run with the per-class overrides these two classes actually
need (900s+, per the residual-seven doc's own measured wall-clocks) before
drawing any `--jit on`/`off` conclusion for this pair.

### 7.4 A harness quirk found in passing: `docker\.sock` in the NO-DB signature pattern matches a SUCCESS log line

This worktree's copy of `run-hibernate-reactive-suite.sh` has a DIFFERENT
(and separately buggy) NO-DB-tagging implementation than the one section 5
described — this one checks `"Could not find a valid Docker environment|
Connection refused|No such host|docker\.sock"` (note the added `docker\.sock`
alternative) against `"$tmp" "$RAW"` and, if it matches, prepends `NO-DB: `
to the signature (or substitutes the literal fallback text
`"connection-refused"` if the primary exception-signature grep found
nothing). **`docker\.sock` matches Testcontainers' own routine, successful
startup log line** — `DockerClientProviderStrategy - Found Docker
environment with local Unix socket (unix:///var/run/docker.sock)` — which
every one of this section's runs prints on a normal, working Docker
connection. Every FAIL row in this section's sweep was mislabeled
`NO-DB: connection-refused` in `results.tsv` even though `found`/`ok` were
never 0 and the real exception (visible in `raw.log`, section 7.1's table)
is the persistence-context cascade, not a connectivity failure. Section 5's
own warning applies here too: **don't trust the `sig` column at face value**
— check `found`/`ok` against 0 and read the actual `@@TESTFAIL` block before
concluding "no DB." This script is gitignored, so (as with section 5) no
fix is committed here; the fix is to drop `docker\.sock` from the pattern
(or match only "Could not connect to Docker" / a `ConnectException` thrown
from inside the harness's own containers setup, not a raw log line that
also appears on success).

**FIXED 2026-08-22.** Applied directly to both Azure copies of the script
(`/data/cratonvm/apps/hibernate-reactive-suite-runner/` and
`/data/cvm-hibreactive-idle-20260820/apps/hibernate-reactive-suite-runner/`
— the two found with this bug; the older Windows-box copy carries only
section 5's original `"$tmp" "$RAW"` scope bug, in a differently-shaped
`sig`-generation block, not touched here): dropped the `docker\.sock`
alternative, and — since the surrounding code was already being edited —
also fixed section 5's still-live `"$tmp" "$RAW"` scope issue in this same
block (it checked both; now only `"$tmp"`, this class's own output).
Verified both corrections against real data rather than by inspection
alone: the new pattern no longer matches this section's own saved
`raw.log`s that previously false-positived (re-checked directly, `grep`
against the old pattern still matches, against the new pattern no longer
does), and a synthetic genuine `ConnectException: Connection refused` log
still matches the new pattern, so real no-DB detection is unaffected.
`bash -n` passes on both files. Still gitignored, so this is a local fix on
the two hosts/worktrees touched, not a commit — the next session working
from a *different* worktree's copy of this script should apply the same
change (or copy the fixed file) if it hits this again.

**Windows-box copy also fixed, same day.** That copy
(`C:\craton\cratonvm\apps\hibernate-reactive-suite-runner\`) never had the
`docker\.sock` alternative — only section 5's original `"$tmp" "$RAW"`
scope bug, in its own differently-shaped `sig`-generation block (an
if/else on a single combined `grep`, rather than the Azure copies' "compute
sig first, then prepend NO-DB" shape). Applied the equivalent fix: both
`grep` calls in that block now check `"$tmp"` only. Verified the same way —
a synthetic repro (current class's own output holding a real cascade
failure, `$RAW` holding an *earlier* class's genuine `ConnectException:
Connection refused`) false-positives under the old two-argument `grep` and
correctly does not under the fixed one-argument form; a synthetic genuine
`ConnectException: Connection refused` in `$tmp` alone still matches, so
real no-DB detection is unaffected there either. `bash -n` passes. All
three known copies of this script (`/data/cratonvm`,
`/data/cvm-hibreactive-idle-20260820`, and this Windows box) now carry the
`$tmp`-only fix; only the Azure copies also needed the `docker\.sock`
removal, since the Windows copy's pattern list never included it.

### 7.5 Updated status

Of the 9 classes now checked with `--jit on` vs `--jit off` (`FilterWithPaginationTest`
plus these 8): **6 confirmed** to be the section 6 JIT-dispatch defect
(clean `--jit off` PASS, exact cascade shape under `--jit on`), **1 plausible
but unconfirmed** (`ReactiveStatelessProxyUpdateTest` — hangs one way,
passes the other, but was already a separate untriaged outlier), and **2
inconclusive** at this timeout (the already-known, separately-documented
lambda-dispatch-timeout family, needs its own real per-class-override
timeout before a `--jit` verdict means anything for those two). No class
checked this session contradicts the section 6 finding.

---

## 8. 2026-08-22 — FIXED. The defect is the JIT lambda direct-call arm dropping a deoptimized frame and letting its caller re-run the body

Section 6 left the mechanism as "a chained lambda/method-reference composition
executing its terminal side-effecting call twice for one outer invocation", with
the composition layer unidentified. It is identified, and it is neither the
composition chain nor `CompletableFuture`: it is
`vm/src/jit/helpers.rs::try_lambda_site_direct_call`, the JIT-side half of the
lambda tier-up.

### 8.1 Reproduced locally, then bisected with `CRATONVM_JIT_DENY`

`FilterWithPaginationTest` reproduces on this Windows box against a
Testcontainers Postgres exactly as it does on Azure: **33/35, the same 2
failures, on every one of 5 consecutive runs of the same binary**, and
`--nojit` is **35/35**. That makes it a same-binary A/B that runs in ~30 s, so
`CRATONVM_JIT_DENY` (a substring match on `Class.method`, and therefore on
nested classes too) can bisect it:

| `CRATONVM_JIT_DENY=` | result |
|---|---|
| `org/hibernate/reactive/util/impl/CompletionStages` | **35/35 PASS** |
| `org/hibernate/reactive/` | **35/35 PASS** |
| `CompletionStages$ArrayLoop` | **35/35 PASS** |
| `CompletionStages.loop` | 33/35 FAIL |
| `CompletionStages.applyToAll` | 33/35 FAIL |
| `CompletionStages.alwaysContinue` | 33/35 FAIL |
| `CompletionStages.voidFuture` | 33/35 FAIL |
| `java/util/concurrent/CompletableFuture` | 33/35 FAIL |

**Exactly one class matters: `CompletionStages$ArrayLoop`.** Everything else in
`CompletionStages`, and `CompletableFuture` itself, is exonerated. Section 2's
`CompletableFuture` theory is now refuted by measurement rather than argument.

### 8.2 Instrumenting `ArrayLoop` HIDES the defect — which invalidates section 6.4

Two source-level instruments were compiled into a patch directory placed ahead
of `hibernate-reactive-core`'s jar on the classpath (verified live: the patched
`ArrayLoop.nextIndex(I)I` shows up in `CRATONVM_DBG=jit-disasm` output):

* a per-`ArrayLoop` `BitSet` of dispatched indices inside `next()` — **35/35 PASS**
* a wrapper around the `index -> consumer.apply(index).thenCompose(...)` lambda
  in `loop(int,int,IntPredicate,IntFunction)` — **35/35 PASS**

Both instruments make the failure disappear, and neither ever fired. Two edits
that do NOT add code did **not** hide it — renaming the private `next(int)`
overload to `nextIndex(int)` (33/35 FAIL) and replacing the `current++`
`dup_x1` idiom with `index = current; current = index + 1` (33/35 FAIL) — so
the overload-resolution and `dup_x1` hypotheses are both refuted, and the
hiding is specific to changing how much code is in the method.

**This means section 6.4's central claim cannot be relied on.** That claim —
"`ArrayLoop.next()` dispatched index 2 exactly once, yet `reactiveRemove` ran
twice, so the duplication is BELOW the dispatch" — was measured with
instrumentation inside `ArrayLoop.next()`, i.e. inside the one method whose
compiled body IS the defect. An instrument that suppresses the thing it
measures reports its absence.

### 8.3 The one kill switch that clears it

Twelve JIT feature switches were run against the same repro. Eleven changed
nothing (`CRATONVM_JIT_OSR=0`, `LOCAL_HANDLERS=0`, `SELF_TAILCALL=0`,
`SP_TAILCALL=0`, `DIRECT_CALLEE_CALLS=0`, `GUARDED_VIRTUAL_INLINE=0`,
`SP_INLINE_PIC=0`, `SP_INLINE_MIC=0`, `METHOD_SITE_CACHE=0`,
`BYTECODE_LOOP_XFORM=0`, and — as section 6.7 already recorded —
`C2_SUPERSEDE=0`). One cleared it:

```
CRATONVM_JIT_LAMBDA_SITE=0   ->  35/35 PASS, 3/3 runs
```

(`CRATONVM_TIER_ENABLED=0` also passes, but that is the broad "compile nothing"
control, not a mechanism.)

### 8.4 The defect

`CRATONVM_JIT_LAMBDA_SITE` gates `try_lambda_site_direct_call` — a compiled
caller's SAM call served straight from the call site's cached target. Its deopt
handling was:

```rust
// A deopt. The body did not complete, so its signals describe an
// attempt that is being abandoned and are dropped with it ...
// the generic path below re-runs the body ...
drop(stashed);
site.disable_direct();
return None;
```

**"The body did not complete" is not "the body did nothing."** A deopt sentinel
means the compiled body ran up to `rframe.bci` and stopped there. `stashed` is
the reconstructed frame that makes resuming from that point possible; dropping
it and returning `None` sends the call to the generic path, which re-enters the
impl **from entry** and re-executes everything the compiled body had already
committed.

For this workload the impl is `CompletionStages.lambda$loop$4`, whose body is
`consumer.apply(index).thenCompose(CompletionStages::alwaysContinue)`, and
`consumer.apply(index)` reaches `ReactiveSessionImpl.reactiveRemove(entity)`.
So one `ArrayLoop.next()` dispatch produces two `reactiveRemove` calls — the
first from the compiled body before it trapped, the second from the interpreted
re-run — with no second dispatch anywhere, which is precisely the signature
section 6.4 recorded. The second delete then finds the entity already
`DELETED`, which is section 6.3's status flip, and the failure surfaces as
`IllegalArgumentException: Unmanaged instance passed to remove()` or, one test
later, as `duplicate key value violates unique constraint "famousperson_pkey"`.

The interpreter's own one-shot door got this right and says so in its own
comment ("an `Ok(None)` decline re-runs the whole body from entry in the
interpreter, which double-executes every side effect the compiled body already
committed before it trapped"). **The two doors into the same compiled lambda
impl disagreed, and only the compiled-caller one was wrong.** The
`direct_disabled` latch was doing real work — it stops the SECOND and every
later call from taking the broken arm — but the call that actually deopts was
already lost.

### 8.5 The fix

`jit_bridge::resume_deopted_body` — the deopt-resume block factored out of
`execute_jit_call_oneshot`, unchanged, and now shared.
`try_lambda_site_direct_call` spends the reconstructed frame through it, keyed
on the site's own impl identity (`LambdaJitSite::cached_impl()` — the identity
`try_resume_trapped_callee` could not supply, because it matches on the CALL
SITE's name, which for a SAM call is `apply`), runs the resumed frame to
completion, and returns its value in the raw JIT ABI. An escaping exception
goes into `jit_pending_exception`, the same contract the MIC hit path uses.

Two ungated counters make the residual visible, reported in the
`CRATONVM_DBG=lambda-jit` census line as `site_resumed=` / `site_unresumable=`:
`site_unresumable` counts deopted bodies that still could not be resumed and
were therefore re-run from entry. It is deliberately NOT gated on the debug
switch, because a correctness residual that only exists when someone set an env
var cannot answer "did any call re-execute" after the fact.

Those counters are also the engagement evidence. On the fixed binary, one
`FilterWithPaginationTest` run:

```
site_calls=8365 site_direct=832 site_no_code=4545 site_deopted=2988
site_resumed=1 site_unresumable=0
```

**Exactly one call in the whole run resumes a reconstructed frame**, and zero
are left unresumable. One deopting call, one body that would otherwise have been
re-run, one duplicated `reactiveRemove`, two failing tests — the arithmetic
closes.

### 8.5.1 Why there is no synthetic unit fixture for this

Four shapes were built and measured against `CRATONVM_DBG=lambda-jit`, and none
of them reaches the arm, so none of them could carry a regression test that is
not a vacuous green:

| fixture shape | census |
|---|---|
| `IntUnaryOperator`, cold branch RETURNS | `site_calls=2 site_adapters=2 site_deopted=0` — an inline-cache thunk installs after two calls and Rust is never entered again |
| `IntUnaryOperator`, cold branch THROWS | same; and with `CRATONVM_JIT_LAMBDA_ADAPTER=0`, `site_calls=398931 site_deopted=0` — a cold `throw` is served by the body's own exception path, not a deopt |
| `IntFunction<Integer>`, receiver class swapped after warm-up | `site_calls=399093 site_deopted=0` — an interface call compiles to a PIC, which absorbs a new receiver class rather than trapping |
| the same with a loop in the lambda body | `site_calls=399521 site_deopted=0` |

The real trap in this workload is one specific speculation failing once in
~8000 SAM calls. A fixture that asserts the invariant without reaching the arm
would pass on the BROKEN binary too, which is worse than no test — so the
verification here is the end-to-end table in 8.6 plus the ungated
`site_unresumable` counter, and the shapes above are recorded so the next
attempt starts past them rather than repeating them.

### 8.6 Verification

Same host, same Testcontainers Postgres, same `common.args`:

| binary / arm | `FilterWithPaginationTest` |
|---|---|
| `cratonvm-hibfix-base` (dev `652956429`) | 33/35 FAIL, 5/5 runs |
| `cratonvm-hibfix-base` `--nojit` | 35/35 PASS |
| `cratonvm-hibfix-base` `CRATONVM_JIT_LAMBDA_SITE=0` | 35/35 PASS, 3/3 runs |
| `cratonvm-hibfix-lambdaresume` (this fix, JIT on, no switches) | **35/35 PASS, 3/3 runs** |

And the sibling classes, base binary versus fixed binary, JIT on, no switches,
one run each:

| class | base | fixed |
|---|---|---|
| `CriteriaMutationQueryTest` | 7/9 FAIL | **9/9 PASS** |
| `OneToManyTest` | 6/8 FAIL | **8/8 PASS** |
| `ReactiveStatelessWithBatchTest` | 22/24 FAIL | **24/24 PASS** |
| `RowIdUpdateAndDeleteTest` | 4/6 FAIL | **6/6 PASS** |
| `QuerySpecificationTest` | 50/52 FAIL | **52/52 PASS** |
| `MutationDelegateIdentityTest` | 5/5 PASS | 5/5 PASS |

**Five classes go FAIL to PASS on this fix alone**, and every one of them fails
with exactly two tests, the same shape as `FilterWithPaginationTest`.
`QuerySpecificationTest` is worth noting separately: it sits in the runner's
`passed.txt`, so its base-binary FAIL here is a regression the list had not
caught, and it is repaired by the same change.

`MutationDelegateIdentityTest` passes on BOTH binaries on this box, so its Azure
FAIL is either a different defect or environment-dependent; this session has no
evidence either way and is not claiming it.

### 8.6.1 Full suite, three collectors, MySQL — 2026-08-22

The whole 249-class `testlist.txt` on the fixed binary against **MySQL** in
Docker (Testcontainers, one container per class), all three collectors, with a
real-HotSpot control on the same database, list, argfile and shard count. Full
write-up:
[`RESULTS-20260822-3gc-mysql-local.md`](../../../apps/hibernate-reactive-suite-runner/RESULTS-20260822-3gc-mysql-local.md).

204 runnable classes per arm (249 minus the 45 no-`@@RESULT` classes):

| GC | PASS | failures HotSpot does NOT share |
|---|---:|---:|
| ZGC (default) | 202 | **1** |
| G1 | 201 | **2** |
| Generational | 199 | **4** |

Every class in this section's family PASSes on all three collectors:
`FilterWithPaginationTest`, `CriteriaMutationQueryTest`, `OneToManyTest`,
`RowIdUpdateAndDeleteTest`, `ReactiveStatelessWithBatchTest`,
`QuerySpecificationTest`, `MutationDelegateIdentityTest`.

What remains is four classes, none of them this defect:
`MultithreadedInsertionWithLazyConnectionTest` (all three arms, PASSes on
HotSpot — now diagnosed in its own page,
[`hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`](hib-reactive-multithreaded-insertion-lazy-connection-20260822.md):
a ~6x throughput gap on `CompletableFuture` composition crossing the test's own
10-minute Vert.x budget, plus a separate intermittent duplicated INSERT that is
NOT this section's defect — `site_unresumable` reads 0 on it),
`techempower.TechEmpowerTest` (G1 + Generational, PASSes on ZGC),
`MultithreadedIdentityGenerationTest` and `SoftDeleteCollectionTest`
(Generational only). The last three are single observations and have not been
repeated.

**One methodology warning from that run, because it nearly produced the wrong
answer.** The first pass ran the three arms concurrently at 3 shards each — nine
simultaneous MySQL container boots — and reported 25/22/20 FAIL, *including
`FilterWithPaginationTest` at `found=35 ok=0 failed=35`*, i.e. this section's own
witness class failing every test in it. The cause was
`IllegalStateException: Could not find a valid Docker environment`: the daemon
was saturated. Re-running the 28-class non-PASS union one arm at a time at 2
shards — matching the HotSpot control's concurrency — collapsed it to 2/3/5.
**When a suite's setup depends on a shared external resource, fork count is an
experimental variable, and an arm run at 9-way against a control run at 2-way is
not an A/B.**

### 8.7 What this does NOT settle

Section 7's sweep found 6 of 8 other still-FAIL classes clear under `--jit off`;
`--jit off` clearing a class means only "the JIT is involved". Five of those are
now confirmed by re-running on the fixed binary (above). Not covered here, and
unchanged by this fix as far as anything measured says:

* `MutationDelegateIdentityTest` — does not reproduce on this box at all.
* `ReactiveStatelessProxyUpdateTest` — section 7 already had it as "changed but
  unconfirmed" (hangs one way, passes the other); left out of the A/B above
  because a hang is not a comparable outcome, and it needs its own timeout
  override first.
* `techempower.TechEmpowerTest` and the two host-timezone/locale classes
  (`ORMReactivePersistenceTest`, `DatabaseHibernateReactiveTest`) — separate,
  already-documented families.

---

## 9. 2026-08-22 — section 8.7's two open classes, settled on Azure: `ReactiveStatelessProxyUpdateTest` is a SEVENTH class the fix repairs, and its "needs a timeout override" premise was wrong

Section 8 fixed the defect and verified six classes on the Windows box.
Section 8.7 left two classes explicitly unsettled, both for reasons that
needed the Azure host rather than more analysis. This section settles both.
It does not re-derive section 8's mechanism and does not dispute any of it —
section 8 is the cause of everything measured here.

Binary: `cratonvm-hibfixverify`, built in `/data/cvm-hibreactive-idle-20260820`
at `5ee7897cf` (current `dev`), `git merge-base --is-ancestor b8fa0585e HEAD`
confirms section 8's fix is in the tree; `md5sum` matched against the
`target/release/cratonvm` it was copied from. Azure host, live Postgres via
Testcontainers, default (ZGC) collector.

**Host caveat, stated up front:** this host was NOT quiet — 1-minute load
average ranged 23–36 across these runs (8 vCPU), with other sessions building
and running suites throughout. That is recorded beside each result below. It
does not weaken a PASS (a class that passes under contention would pass idle),
and every result here is a PASS, so no conclusion in this section rests on a
timeout that might have been contention. A HANG under this load would have
been reported as provisional; none occurred.

### 9.1 `ReactiveStatelessProxyUpdateTest` — PASSES with the JIT on, at the SAME cap where it used to hang

Section 7.2 recorded it as `--jit off` PASS 4/4 in 28.4 s, `--jit on` HANG at
the flat 120 s cap, and section 8.7 set it aside as needing "its own timeout
override first" before it could be compared at all.

**The timeout-override premise turns out to be wrong, and the class is simply
fixed.** On the fixed binary:

| arm | cap | result | wall |
|---|---|---|---|
| `--jit on` | 600 s | **PASS 4/4** | 28.8 s |
| `--jit off` | 600 s | PASS 4/4 | 15.9 s |
| `--jit on` | **120 s** (the original cap) | **PASS 4/4** | 24.4 s |
| `--jit on` | **120 s** | **PASS 4/4** | 14.9 s |
| `--jit on` | **120 s** | **PASS 4/4** | 18.5 s |

Four `--jit on` runs, all PASS, and the three at the ORIGINAL 120 s cap are
the ones that matter: the class now finishes in 14.9–24.4 s against a cap it
previously could not reach at all. It never needed a raised budget — the
"hang" was the section 8 defect, and a raised timeout would only ever have
converted a hang into a longer hang. Raising the cap to 600 s changed nothing
(28.8 s, the same neighbourhood).

So `ReactiveStatelessProxyUpdateTest` is a **seventh class repaired by
`b8fa0585e`**, alongside section 8.6's six, and section 7.2's "plausible but
unconfirmed" reading of it is now confirmed. It should NOT be given a
`class-overrides.tsv` entry; nothing about it is slow.

### 9.2 `MutationDelegateIdentityTest` — does not fail on Azure on the fixed binary either

Section 8.6 found it passing on both binaries on the Windows box and
explicitly declined to claim anything about its Azure FAIL. On Azure, fixed
binary, `--jit on`, three consecutive runs: **PASS 5/5 every time** (17.1 s,
14.4 s, 22.9 s; load 23).

Read this narrowly. It confirms the class is healthy on Azure today, but it
is **not** new evidence that section 8's fix repaired it, because section 5
already recorded this class among the nine that went PASS on Azure back on
2026-08-21 — attributed there to the `nio_selector.rs` fix in section 1, on a
binary that predates `b8fa0585e` entirely. The most defensible statement is
the one section 8.6 was reaching for: **the original 2026-08-20 3-GC-run FAIL
for this class does not reproduce on Azure and has not for some time**, and
nothing in section 8's change altered that. It is not an open item.

### 9.3 Updated tally

Nine classes have now been checked against section 8's fix across the two
sessions:

| class | status |
|---|---|
| `FilterWithPaginationTest` | fixed by `b8fa0585e` (8.6) |
| `CriteriaMutationQueryTest` | fixed (8.6) |
| `OneToManyTest` | fixed (8.6) |
| `ReactiveStatelessWithBatchTest` | fixed (8.6) |
| `RowIdUpdateAndDeleteTest` | fixed (8.6) |
| `QuerySpecificationTest` | fixed (8.6) |
| `ReactiveStatelessProxyUpdateTest` | **fixed (9.1, this section)** |
| `MutationDelegateIdentityTest` | not failing; unrelated to this fix (9.2) |
| `techempower.TechEmpowerTest` | still open — separate family, see below |
| `MultithreadedInsertionWithLazyConnectionTest` | NOT fixed — perf family, measured in 9.5 |

**Seven classes repaired by one change.** The only member of section 7's
still-FAIL list not accounted for is `techempower.TechEmpowerTest`, which
sections 7.3 and 8.7 both place in the already-documented
lambda-dispatch-timeout family (its fixture carries a hardcoded Vert.x
deadline no runner flag can reach, per
`residual-seven-after-the-afc-fix-20260817.md` section 2.1). It was NOT
re-run here: giving it a meaningful verdict needs a quiet host, and this one
was at load 23–36 throughout — exactly the condition under which a
timeout-bound class's result would be uninterpretable. Left open deliberately
rather than measured badly.

### 9.4 A host note worth recording

The first build attempt for this section was **OOM-killed** — `rustc` at
4.4 GB RSS, `oom-kill … task=rustc` in `dmesg` at 18:32:53, on a 31 GB host
with no swap that had reached 426 logged-in sessions and a 1-minute load
average of 150. `nohup` survives `SIGHUP`, not the OOM killer, and the symptom
is misleading: the build log simply stops mid-crate and the process is gone,
which reads exactly like "still compiling a big crate" for as long as nobody
checks. A `pgrep -f 'cargo build …' | wc -l` returning a nonzero count was
*not* evidence it lived — the pattern matched the checking shell's own
command line, the self-match trap
[[feedback_shared_host_blanket_process_kill]] and its sibling note already
warn about. What settled it was sampling the PID's own CPU time twice, at
which point `ps -p <pid>` returned nothing at all.

Rebuilding with `nice -n 10` and `-j 2` (fewer concurrent `rustc`, lower peak
RSS) completed in 17m46s on the same contended host. Anyone building on this
host while it is busy should do the same, and should watch for process
*disappearance* rather than only for a completion marker — a watcher that
waits for `BUILD_DONE` alone waits forever on a killed build.

### 9.5 `MultithreadedInsertionWithLazyConnectionTest` — measured, still the perf family, and the runner's `--timeout` is the WRONG knob for it

Section 7.3 left this class "inconclusive at this run's flat timeout". Measured
here on the same fixed binary, and the answer is that section 8's fix does not
change it: it is still exactly what
[residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
section 2.1 described.

**The first attempt measured the wrong thing, and the way it was wrong is worth
recording** because it will catch the next reader too. Running with
`--timeout 900` produced `FAIL 0/2` in 251 s, with both methods throwing
JUnit's own
`TimeoutException: … timed out after 120 seconds`. That is **not** the fixture
deadline and not the runner's cap — it is
`-Djunit.jupiter.execution.timeout.default=120s`, set in `common.args`. The
runner's `--timeout` only bounds the forked process's wall clock; it does
nothing to JUnit's per-test timer, and 2 x 120 s accounts for the 251 s exactly.
This is the same argfile-ordering hazard residual-seven section 4 documents from
the other direction: per-class `-D` flags must land AFTER `@common.args` to win,
which the runner now does.

Re-run with the JUnit timer actually raised — via `HR_CLASS_OVERRIDES` pointing
at a temporary table (`timeout=900`,
`-Djunit.jupiter.execution.timeout.default=900s`), so the tracked
`class-overrides.tsv` was not modified:

| | result |
|---|---|
| `found` / `ok` / `failed` | 2 / **1** / 1 |
| wall | 749 s (12m29s) |
| failing method | `testIdentityGeneratorWithTransaction` |
| its exception | Vert.x `TimeoutException: The test execution timed out. Make sure your asynchronous code includes calls to either VertxTestContext#completeNow()…` |

So **one of the two methods passes and the other exceeds the fixture's own
hardcoded `@Timeout(value = 10, timeUnit = MINUTES)`** — the deadline
residual-seven section 2.1 already established `io.vertx.junit5` exposes no
system property for, and which therefore **no runner flag, override row, or
system property can reach**. Patching an upstream stress test's own budget to
make the VM look better remains a trade this project has declined to make.

Two caveats, stated rather than buried: the host was at 1-minute load average
23–27 (8 vCPU) throughout, so this does not prove the second method could not
finish inside 600 s on a quiet box; and the 749 s here is not comparable to
residual-seven's 942.9 s, which was a different host under different conditions.
What it does establish is the **shape** — 1 of 2, second method on the fixture
deadline — is unchanged by `b8fa0585e`, which is the expected result: section 8
fixed a double-execution correctness defect, not the functional-interface
dispatch cost that makes this class slow.

`techempower.TechEmpowerTest` is still not measured (5-minute fixture deadline,
same family, and section 9.3's reasoning about host load applies to it more
strongly than to this class).

### 9.6 …and then it WAS measured, and it is not a perf class at all

Measured the same day on a quiet host (load 6.8–14), and the "same family"
assumption in 9.5 and 9.3 — inherited from
[residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
section 2.1 — **does not hold**:

| arm | result | wall |
|---|---|---|
| real HotSpot | PASS 1/1 | 13.3 s |
| CratonVM `--jit off` | **PASS 3/3** | 74.9 / 72.2 / 72.6 s |
| CratonVM `--jit on` | FAIL 4/4 | 31.3 / 22.5 / 21.7 / 305.8 s |

`techempower.TechEmpowerTest` **passes on CratonVM in ~72 s**, well inside its
own 5-minute fixture deadline, with the JIT off. It never needed a raised
budget and it is not bound by dispatch cost. With the JIT on it fails — 3 of 4
times with a fast WRONG ANSWER (a server-side `NullPointerException` because
`session.find(World.class, id)` returned `null` for an id the benchmark
guarantees exists, surfacing as HTTP 500 in ~25 s), and 1 of 4 times on the
fixture deadline. That bimodality is why it was misfiled: the earlier records
only ever caught the timeout mode.

It is **not** the section 8 defect — the 500 was already present pre-`b8fa0585e`
(section 7.3) and reproduces on a binary containing it.

Full write-up, evidence, reproducer, and the one query that would split the
remaining search space:
`techempower-wrong-answer-was-the-indy-trap-FIXED-20260824.md` (retired to
docs/internal 2026-08-24: the wrong answer was the pre-bridge `invokedynamic`
trap, and `CRATONVM_JIT_INDY_BRIDGE=0` puts it back on any current binary).

---

## 10. 2026-08-24 — the closing sweep: of the 61 non-passed classes, 14 of the 17 genuine failures are gone, and ZERO CratonVM correctness defects remain

Every section above measures one class or one defect. This is the measurement
none of them is: **the whole non-passed set, re-run on current `dev`, after all
three fixes landed.** Until now the suite's recorded state predated the
`nio_selector` fix (§1), the lambda-deopt fix (§8), and the `invokedynamic`
trap fix that retired `TechEmpowerTest`, so "what is actually left" was an
inference rather than a number.

Local Windows box, live Postgres via Testcontainers, binary built from `dev`
`35bc2d5a7`. Deliberately the **same** invocation the 2026-08-20 baseline used —
`--category others --shards 6 --timeout 180`, JIT on, default (ZGC) collector —
so the two are comparable line for line.

| | 2026-08-20 (ZGC arm) | **2026-08-24** |
|---|---|---|
| PASS | — | **14** |
| FAIL | 15 | **2** |
| HANG | 1 | 1 |
| CRASH | 1 | **0** |
| NOTESTS | 44 | 44 |
| wall | — | 4m02s |

The 44 `NOTESTS` are structural — base/abstract classes in `testlist.txt` that
declare no tests — and are identical in both runs. So the real population is
**17 genuine failures, of which 14 now pass.**

### 10.1 The three that remain, attributed per class

**None of the three is an open CratonVM correctness defect.**

| class | status | cause |
|---|---|---|
| `ORMReactivePersistenceTest` | FAIL | **this box** — `ServiceException` … `invalid value for parameter "TimeZone": "America/Buenos_Aires"` |
| `it.quarkus.qe.database.DatabaseHibernateReactiveTest` | FAIL | **this box** — `nameIsNull` `AssertionError`, the `ru_RU` Bean Validation message |
| `MultithreadedInsertionWithLazyConnectionTest` | HANG | known perf residual, and an artifact of the flat cap (below) |

The first two are the pair
[residual-seven-after-the-afc-fix-20260817.md](residual-seven-after-the-afc-fix-20260817.md)
§1 established fail **identically under real HotSpot** on this host, and which
pass on the Azure box because it is UTC/`en`. They are the host's timezone and
display language, not the VM.

The third is not news either: it is `HANG` here only because this run used the
flat `--timeout 180` to stay comparable with the baseline. Given its real budget
it is `1 of 2` methods, with `testIdentityGeneratorWithTransaction` over the
fixture's own hardcoded 10-minute deadline — see
[hib-reactive-multithreaded-insertion-lazy-connection-20260822.md](hib-reactive-multithreaded-insertion-lazy-connection-20260822.md),
whose §8 also records that the `invokedynamic` fix does **not** retire it.

### 10.2 A trap avoided while reading this result

The first attempt to attribute the two FAILs grepped
`shard-*/raw.log` for the timezone signature and found it for **both** classes —
which would have mis-recorded `DatabaseHibernateReactiveTest` as a timezone
failure. `raw.log` is **cumulative per shard**: it holds every class that shard
ran, so a signature found in it belongs to *some* class in that shard, not
necessarily the one being asked about. That is the same `$RAW`-scope hazard
§7.4 documents in the runner's own `NO-DB` tagging, met from the reading side
rather than the writing side.

Attributing per class — locating each one's `@@TESTFAIL` line and reading only
the lines that follow it — gives the correct and *different* answer above:
`ORMReactivePersistenceTest` is the timezone, `DatabaseHibernateReactiveTest`
is the locale. Both were already documented that way; the sloppy grep would have
"confirmed" a wrong story that happened to agree with the tidier half of it.

### 10.3 Status of this page

The regressions this page was opened for are **closed**. Of the 17 genuine
failures in the 2026-08-20 non-passed set, 14 pass on today's `dev`; the other
three are two host-environment classes that fail under HotSpot too and one
documented performance residual with its own page. No class in the
hibernate-reactive suite is currently failing for a CratonVM correctness reason.
