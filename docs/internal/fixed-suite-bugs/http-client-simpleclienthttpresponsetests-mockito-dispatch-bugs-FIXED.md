# SimpleClientHttpResponseTests: Mockito/ByteBuddy dispatch residuals [FIXED]

Status: FIXED on 2026-07-08. Split out of the retired `http-client-cluster-redefine-
dispatch-and-jdk21-gaps.md` (full historical fix context there:
`docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md`).

Fix summary:

- `ThreadLocal.get()` now honors overridden `initialValue()` for anonymous
  subclasses such as Mockito's `ThreadSafeMockingProgress$1`, preventing stale
  per-thread mocking progress from leaking into the next JUnit method.
- Method-handle virtual dispatch now keeps adapted object arguments pinned
  through the final target invocation and can recover a known declared owner
  when receiver dispatch collapses to `java/lang/Object`.
- Synthetic/lazy stream pipelines now pin source streams, source elements, and
  deferred lambdas across materialization and terminal pulls, preventing
  StackWalker/Mockito lambdas from degrading to `Object.apply/test`.
- InputStream/OutputStream native fallbacks now refresh receivers across
  transfer/drain loops and avoid applying inherited `InputStream` read helpers
  to Mockito mock streams where Mockito's default-answer machinery should own
  the inherited concrete methods.

Validation:

- Built release binary
  `/data/data/bin/cratonvm-http-simpleclient-residuals-20260708-170843-fix16`.
- Probe `TLInitialValueProbe20260708170843`: PASS.
- Probe `LazyStreamLambdaProbe20260708170843`: PASS.
- `org.springframework.http.client.SimpleClientHttpResponseTests`: PASS 17/17
  after final fixes (`fix14` batch 10/10, `fix15` batch 3/3, `fix16`
  batch 3/3, and one standalone `fix14` run), with no `NoSuchMethodError` and no
  `UnfinishedVerificationException`.

Two distinct, both intermittent bugs in `org.springframework.http.client
.SimpleClientHttpResponseTests`, confirmed across multiple sessions on the
Azure host (`--java-home` real JDK 25 mode):

## Bug 1 — `UnfinishedVerificationException`, order-dependent

`shouldNotCloseConnectionWhenResponseClosed` passes internally, but
`org.mockito.exceptions.misusing.UnfinishedVerificationException` is
thrown from the *next* test method's `mock()` call. Reproduces at a HIGH
rate — 18/21 runs (~86%) in the most recent session, considerably more
often than earlier single-run reports suggested.

Suspected: a narrow residual in how Mockito's `MockingProgress`
thread-local interacts with JUnit 5's per-method reflective test
instantiation (same OS thread, new Java object instances per test
method). Not root-caused; no live capture attempted yet specifically for
this bug (effort in the most recent session went to Bug 2). **Next step:**
instrument CratonVM's `ThreadLocal` get/set to log `MockingProgress`-shaped
keys across a `shouldNotCloseConnectionWhenResponseClosed` → next-test-method
boundary, to see whether the thread-local state genuinely leaks or whether
CratonVM's JUnit5 per-method instantiation itself doesn't reset something
real JVM's would.

## Bug 2 — `NoSuchMethodError` with a substituted `(class, method, descriptor)` triple

Reproduces at a low rate (3/21 runs in the most recent session) from
`shouldNotDrainWhenErrorStreamClosed`, always from inside Mockito's
`InstrumentationMemberAccessor$Dispatcher$ByteBuddy$<hash>` dispatcher
machinery, but naming a **completely unrelated method** each time it
fires — NOT just a wrong class for the same method name:

- `NoSuchMethodError: java/lang/Object.write(I)V` (the originally-reported
  form), from a plain bytecode `MethodHandle.invokeWithArguments
  (Object...)` call.
- `NoSuchMethodError: java/lang/Object.test(Ljava/lang/Object;)Z`, from
  the SAME test method but a completely different call site
  (`Predicate.test`, invoked from `LocationImpl.lambda$getStackFrame$2`
  via `StackWalker.walk` → `Stream.filter`).

Seeing two unrelated `(class, method, descriptor)` triples from the same
test run-to-run rules out a *fixed*, deterministic bug (e.g. a corrupted
constant-pool entry, which would always produce the same wrong method for
a given call site) — the substitution is dynamic/timing-dependent.

### Hypotheses checked and REFUTED (do not re-propose without new evidence)

1. **Receiver `ClassId(0)` collapses to bare `java/lang/Object`** (the
   mechanism fixed for the JIT path in commit `b09fea46`,
   `vm/src/jit/helpers.rs:938-988`) — refuted:
   - The interpreter's uncached dispatch path already has the equivalent
     guard, `vm/src/runtime/interpreter.rs:16077-16105` (tagged `S111r8`).
   - Wrong shape of bug anyway: this mechanism would produce
     `NoSuchMethodError: java/lang/Object.invokeWithArguments(...)` (same
     method, wrong class) — the actual errors name entirely different
     methods.
2. **ClassId reuse** (a freshly-defined ByteBuddy class inheriting a stale
   numeric id) — refuted: `classloading/src/class.rs:756`
   (`ClassStore::next_id`) is strictly monotonic on an append-only `Vec`,
   never recycled.
3. **Hash-collision returning the wrong cache entry** in `InvokeCache`
   (`classloading/src/resolution.rs:1246`) or `ResolutionCache`
   (`classloading/src/resolution.rs:336`) — refuted: both are
   `FxHashMap`s (faster hash, not weaker); Rust's `HashMap` always does a
   full key-equality check on lookup.
4. **Constant-pool corruption at `resolve_method_ref`** (the leading
   hypothesis after refuting #1-3) — refuted with a LIVE capture (not
   just static reading): instrumented `execute_invoke_kind`'s
   `resolve_method_ref(shared, current_class_id, cp_index)` call
   (`vm/src/runtime/interpreter.rs`, ~line 15637 as of the investigating
   session) to dump the resolved value AND an independent, cache-bypassing
   re-read of the class's own constant pool at that index. Captured the
   exact `InstrumentationMemberAccessor$Dispatcher$ByteBuddy$<hash>
   .invokeWithArguments` call site multiple times across 8 instrumented
   runs — **the constant pool resolution was correct every single time.**
   This class's own bytecode/CP is fine; `resolve_method_ref` and
   `current_class_id`/`cp_index` (i.e. a `frame_idx` bug feeding a stale
   frame) are not the source.

   Notably, none of those 8 instrumented runs reproduced Bug 2 (all hit
   Bug 1 instead) — circumstantial evidence the trigger is
   timing-sensitive, since the instrumentation's small added latency was
   enough to suppress it in that batch.

### Current leading hypothesis — NOT yet confirmed

Given the constant pool is proven correct, the bug must be downstream of
`execute_invoke_kind`'s `try_lambda_dispatch` check, in the receiver-class
determination for the `Value::Object(Some(obj_ref))` arm (`vm/src/runtime
/interpreter.rs`, ~line 15850). That code has two existing rescue guards
for "receiver's resolved class collapses to `java/lang/Object`" (`S111r8`
and `S111r12`/"S-trinity #2"), and both were checked by hand against
`is_object_member("test", "(Ljava/lang/Object;)Z")` and
`is_object_member("write", "(I)V")` — both correctly return `false`, so
**both existing rescues should fire and prevent this bug, and yet it still
happens.** Two explanations, both still open:

(a) The actual failing call doesn't go through this code path at all —
`vm/src/vm/vm_exec.rs:5630`'s `invoke_virtual` (the `NativeContext` trait
method, used by `mh_dispatch`'s virtual-arm,
`native-builtins/src/lang_invoke.rs:5878-5890`) has its own, separately
implemented receiver-class-determination logic that was NOT
exhaustively checked with the same rigor.

(b) A stale/dangling receiver pointer whose target address was reclaimed
and reallocated to an unrelated live object between when the pointer was
captured (e.g. an operand-stack slot of a suspended frame) and when it's
dereferenced. This cleanly explains both symptoms: the header is NOT
all-zero (so neither `S111r8`'s all-zero check nor the gen_heap
"sweep-zero" detector fires, because a live *different* object now
occupies that address), and the reported method varies run-to-run
(depends on which unrelated object occupies the stale address). Related
precedent: memory `bug03-cross-thread-jit-root-scan-insufficient`. NOT
directly confirmed for this bug — `CRATONVM_DBG_SWEEP_ZERO=1
CRATONVM_DBG_STALE_RECV=1` (both pre-existing) were run for 5 additional
clean runs and did not fire, but Bug 2 also didn't reproduce in those 5 —
inconclusive given the session's time budget and the ~1-in-7 to 1-in-11
intermittent rate.

## Next steps

1. Instrument `vm/src/vm/vm_exec.rs:5630`'s `invoke_virtual` the same way
   as `execute_invoke_kind` was — dump receiver `class_id`, whether
   `class_manager.get_class()` resolves it, and the resolved dispatch
   class, specifically for calls where `method_name` doesn't match the SAM
   of any known `lambda_proxies` entry. This is the actual code path
   `mh_dispatch` uses and was not re-verified with the same rigor.
2. Run with `CRATONVM_DBG_SWEEP_ZERO=1 CRATONVM_DBG_STALE_RECV=1
   CRATONVM_DBG_HCMH0706=1` together across a much larger batch (20-30+
   runs) on a quiet, uncontended host — the investigating session's host
   swung between load average 5 and 90+, which made large batches
   expensive and likely suppressed the timing-sensitive trigger.
3. For Bug 1, instrument CratonVM's `ThreadLocal` implementation around
   `MockingProgress`-shaped keys across the JUnit5 per-method instantiation
   boundary — not yet attempted at all.

## Reproduction

```bash
CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"
# CRATONVM_DBG_HCMH0706=1 enables the resolve_method_ref live-capture instrumentation
# (left in place, zero cost when unset) at vm/src/runtime/interpreter.rs's execute_invoke_kind.
KRUN_STACK=1 timeout 180 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.SimpleClientHttpResponseTests
# Run 10-20+ times -- both bugs are intermittent; expect roughly 80-90% Bug 1,
# 10-15% Bug 2, occasionally neither (test passes 5/5). This class is also
# pathologically slow (~70s for 5 tests) due to a separate, already-documented,
# unrelated JIT-frame-scan-throughput issue -- not a hang, just slow.
```
