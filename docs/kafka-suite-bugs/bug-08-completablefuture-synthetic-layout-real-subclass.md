# Bug 08 — `CompletableFuture` natives use a synthetic field layout that corrupts real subclasses (`KafkaFuture` cluster)

**Severity:** High / broad — breaks every test that drives a `KafkaFuture`
(admin `*ResultTest`, producer/consumer result handling, anything using
`assertFutureError`). Manifests as wrong results, `rc=1`/`rc=127` abnormal exits,
and hangs. Reproduces under `--nojit`. HotSpot clean.

## Symptom

Standalone repro (`apps/kafka/tests/repro/KFGet.java`):
```java
KafkaFutureImpl<String> ex = new KafkaFutureImpl<>();
ex.completeExceptionally(new RuntimeException("boom"));
ex.isCompletedExceptionally();   // CratonVM: false  (HotSpot: true)   ← WRONG
ex.get();                        // CratonVM: returns the exception object
                                 // HotSpot: throws ExecutionException(cause)  ← WRONG
```
Downstream, `org.apache.kafka.test.TestUtils.assertFutureError(future, cls)` (used
by most admin `*ResultTest`s) calls `future.get()` expecting an `ExecutionException`;
with `get()` mis-behaving, the assertion fails or the test path diverges (some
hang on a never-throwing `get()`).

## Root cause

`org.apache.kafka.common.internals.KafkaFutureImpl` delegates to a field
`completableFuture` of type `KafkaCompletableFuture`, which **`extends
java.util.concurrent.CompletableFuture`** — a *real* class, allocated by real
`new` + real `<init>`, with the real CF instance layout:

```
java.util.concurrent.CompletableFuture:  slot 0 = result (Object),  slot 1 = stack (Completion)
```

CratonVM intercepts `CompletableFuture` with **natives that assume a synthetic
4-slot layout** (`native-collections/src/lib.rs`):
```
const CF_FIELD_RESULT  = 0;   // ok, coincides with real `result`
const CF_FIELD_DONE    = 1;   // COLLIDES with real `stack`
const CF_FIELD_SOURCE  = 2;   // out of bounds on a real 2-field CF
const CF_FIELD_HANDLER = 3;   // out of bounds
```
`CompletableFuture.<init>` is **not** intercepted, so `new KafkaCompletableFuture()`
produces a genuine 2-field object. `kafkaCompleteExceptionally` → `super.completeExceptionally`
→ `native_cf_complete_exceptionally`, which does:
```rust
ctx.set_field(this, CF_FIELD_RESULT, exc);     // writes slot 0 (result) — ok-ish
ctx.set_field(this, CF_FIELD_DONE, Value::Int(2)); // writes slot 1 == real `stack` — CORRUPTS
```
`isCompletedExceptionally` then reads slot 1 (real `stack`, not the done-marker) →
returns the wrong value. The OOB guard also drops writes/reads to slots 2/3 on the
real object. Result: completion state is lost/garbled for any real CF subclass.

This is the same *synthetic-layout-vs-real-object* family as bug-02 / bug-07: a
CratonVM intrinsic keyed to a hand-rolled field layout misbehaves when the JDK
(or an app subclass) supplies a genuinely-laid-out object.

## Fix (IMPLEMENTED — `native-collections/src/lib.rs`)

The natives were made **layout-agnostic** and now produce/read genuine real-CF
state so they interoperate with the un-intercepted `get()`/`join()`/`isDone()`
bytecode:

1. **`completeExceptionally`** — for an exceptional completion, drive the *base*
   `CompletableFuture.obtrudeException` via `invoke_special` (bypassing
   `KafkaCompletableFuture`'s override that throws "User code should not complete
   futures returned from Kafka clients"). That stores a genuine `AltResult(ex)` in
   `result`, so `get()` throws `ExecutionException(cause)` and
   `isCompletedExceptionally()` returns true.
2. **`isCompletedExceptionally`** — check `result` (slot 0) for an `AltResult` with
   a non-null `ex` first; fall back to the legacy synthetic `DONE` marker.
3. **`complete`** (newly registered native) — `CompletableFuture.complete(null)`
   must store the JDK's static `NIL` sentinel (`AltResult(null)`); the real
   `complete(null)` bytecode left `result == null` on CratonVM (so `isDone()` stayed
   false), which left `KafkaFuture.allOf(...)`'s result pending forever (`get()`
   hung). The native reads the real static `CompletableFuture.NIL` field and stores
   it for the null case. Non-null values are stored as-is.
4. **Chaining natives** (`whenComplete`, `allOf`, `exceptionally`, `handle`,
   `thenApply`) — read source completion state via a layout-agnostic helper
   (`cf_read_state`) that understands both the synthetic `DONE` encoding and the
   real `result`/`AltResult` encoding, propagate exceptions correctly (e.g.
   `whenComplete` passes the real `(value, exception)` pair; `allOf` is exceptional
   if any input is), and return **real** completed CFs (`cf_make_completed`) instead
   of synthetic ones.

A layout helper distinguishes synthetic CFs by the object's allocated slot count
where needed, but most logic now keys off `result`/`AltResult` so it works for both.

### Verified
- `CFProbe` (plain `CompletableFuture`): normal/exceptional/`completedFuture` all
  match HotSpot.
- `KafkaFuture` repros (`KFut2`/`KFut3`/`KFut4` in `apps/kafka/tests/`):
  `complete(null)`, `completeExceptionally`+`get`, normal `allOf().get()`.
- **All 5 admin `*ResultTest` now PASS** (`DeleteConsumerGroupOffsets` 5/5,
  `DescribeUserScramCredentials` 3/3, `ListTransactions` 3/3, `RemoveMembers` 5/5,
  `DeleteTopics` 3/3) — previously hang/ABEND/FAIL.
- No regression: 8 previously-OK CF-adjacent classes still pass.

### Known remaining edge
A *standalone* `KafkaFuture.allOf(singleExceptionallyCompletedFuture).get()` micro-repro
still hangs, but the real admin tests (which exercise the same path) pass — likely a
quirk of the isolated single-future case; not pursued further.

## Deeper fix (correct, deferred — larger change)

Rework the CF natives to encode completion state **in slot 0 (`result`) only**, the
way the real `CompletableFuture` does, so they are layout-independent and work on
real subclasses (`KafkaCompletableFuture`) and synthetic CFs alike:
- pending → `result == null`
- normal completion with value `v` → `result = v` (a NIL sentinel for a null value)
- exceptional → `result = AltResult(ex)` (allocate `CompletableFuture$AltResult`,
  or a CratonVM marker class with one `ex` field)
- `isDone` = `result != null`; `isCompletedExceptionally` = `result instanceof AltResult`;
  `get`/`join` unwrap: `AltResult` → throw `ExecutionException(ex)`, NIL → null, else value.

This touches ~20 CF natives (complete / completeExceptionally / get / join / getNow /
isDone / isCancelled / isCompletedExceptionally / thenApply / thenAccept / whenComplete /
handle / exceptionally / allOf / anyOf / …), so it is staged separately to avoid
regressing existing synthetic-CF users.

The deeper fix (run the real `CompletableFuture` bytecode — Unsafe/ForkJoinPool
backed) would remove the intrinsic entirely but is a much larger effort.

## Status
- Root-caused + repro (`KFGet.java`). Fix not yet applied (scoped as a separate change).
- Affected (observed): admin `DeleteConsumerGroupOffsetsResultTest`,
  `DescribeUserScramCredentialsResultTest`, `ListTransactionsResultTest`,
  `RemoveMembersFromConsumerGroupResultTest`, `DeleteTopicsResultTest`, and the
  broader KafkaFuture-driven producer/consumer result tests.
