# Bug 05 — JIT-compiled method's catch block sees `this`/params as null/uninitialized

**Severity:** High — **CratonVM-only**, layer-B (instance-method JIT tier-up) regression.
When a JIT-compiled method catches an exception, the interpreter rebuilds the method's
frame to run the catch block but **discards the incoming locals (`this` + parameters)**.
Any handler that reads `this` or a parameter — overwhelmingly common in instance methods —
then operates on `null`/uninitialized. HotSpot (JDK 25) runs the identical code fine.

**Status: FIXED** (main checkout `C:\craton\cratonvm`, branch `dev`,
`vm/src/runtime/interpreter.rs`). Verified by a minimal standalone repro (== HotSpot),
a 6-scenario exception-semantics diff (== HotSpot), and B-off/JIT-off differentials.

## Scope — what this fixes (and what it does NOT)
This is a **real, distinct production JIT bug**: any JIT-compiled instance method whose
catch block reads `this` or a parameter (extremely common) gets `null`/uninitialized.
`TCRepro` reproduces it at the **default** threshold (500), so it bites ordinary
application code (Spring/Hibernate/etc.) with hot `try{…}catch(e){ this.field… }` methods.

**It is NOT, by itself, the fix for the WildFly JUnit-platform cluster.** That cluster
(`annotationType must not be null` ×5, `AbstractMethodError …getId/…hasGenericInformation/
…getParent` ×3, `ClassCast Object→TestExecutionResult$Status` ×1) is a **separate**
B-gated JIT bug in the JUnit **discovery** path — see
[bug-06](bug-06-jit-junit-discovery-reflection-corruption.md). The original production run
shows **zero** bug-05 signatures (`grep "throwable"/"ThrowableCollector"` → 0): those tests
die in discovery (bug-06) *before* reaching the execution code where this bug lives.

Under `CRATONVM_JIT_THRESHOLD=2`, the Jupiter clustering classes surface this bug as a
deterministic NPE during *execution* (and, with more JIT warmup, bug-06 appears in
*discovery*):

```
java.lang.NullPointerException: Cannot read field 'throwable' because the object is null
    at org.junit.platform.engine.support.hierarchical.ThrowableCollector.add(ThrowableCollector.java:91)
    at org.junit.platform.engine.support.hierarchical.ThrowableCollector.execute(ThrowableCollector.java:77)
```
`ThrowableCollector.execute` is `try { executable.execute(); } catch (Throwable t) { add(t); }`.
Its catch block does `aload_0; aload_2; invokespecial add`, and `add` does
`aload_0; getfield throwable` (bytecode offset 7 = line 91). `aload_0` (`this`) returns
null ⇒ NPE.

The `pruneStackTrace` `CompactValue 0xfffc…` cases (vintage, ×2) are *plausibly* this bug
(`0xfffc…` = `CompactValue::int(0)` = an uninitialized int handler local re-encoded), but
were not standalone-reproduced; they may also be bug-06.

## Minimal standalone repro (no WildFly)
[`repro/TCRepro.java`](../../../cratonvm/wildfly-suite/repro/TCRepro.java) mirrors
`ThrowableCollector`: an instance method with `try { … } catch (Throwable t) { add(t); }`
where `add` reads `this.field`.

| VM | `TCRepro 50000` |
|----|------|
| HotSpot JDK 25 | `ok=50000 bad=0` |
| CratonVM B-on, `CRATONVM_JIT_THRESHOLD=2` (before) | `ok=2 bad=49998` (`NullPointerException: Cannot invoke add on null`) |
| CratonVM B-on, **default** threshold 500 (before) | `ok=500 bad=49500` — a **real** production bug, not a low-threshold artifact |
| CratonVM B-off (`CRATONVM_JIT_VIRTUAL_TIERUP=0`) / `CRATONVM_DISABLE_JIT=1` | `ok=50000 bad=0` |
| **CratonVM B-on (after fix)** | `ok=50000 bad=0` ✓ |

The first N iterations (N = threshold) pass interpreted; the bug appears the instant
`execute` is JIT-compiled. B-off / JIT-off are clean ⇒ the bug is wholly in the
instance-method JIT exception path. (Latent under static-only JIT: static-method catch
blocks rarely read uninitialized param locals and have no `this`.)

## Root cause (confirmed)
A JIT-compiled method runs entirely as native code with no live bytecode frame. When a
callee dispatched from it throws, the dispatch helper stashes the exception and returns
the `i64::MIN` deopt sentinel; the interpreter then calls
`route_jit_exception_through_method` (`vm/src/runtime/interpreter.rs`) to look up the
JIT'd method's exception table and, on a match, **push a fresh bytecode frame positioned
at the handler** so the catch block runs interpreted.

That frame was built with `NO_ARGS`:
```rust
// Pass an empty args slice — these locals are unreachable once PC jumps
// to the catch block (Java verifier guarantees catch-block locals are
// re-initialized before use).            // <-- WRONG ASSUMPTION
const NO_ARGS: &[Value] = &[];
let frame = Frame::new_pooled(/* … */ NO_ARGS, /* … */);
```
The verifier does **not** require a handler to reassign locals it reads. `this` (local 0
of every instance method) and unmodified parameters are live throughout the method,
including its catch blocks. With `NO_ARGS`, `Frame::new_pooled` fills every local with
`CompactValue::uninitialized()`, so `aload_0` in the handler yields null (and an
uninitialized int local re-encodes as `CompactValue::int(0)` = `0xfffc…`, the
pruneStackTrace panic value).

## Fix
Thread the JIT call's incoming arguments (`this` + declared params) — which
`execute_jit_call` already keeps bit-exact in `saved_args` for the deopt-restore path,
and which `execute_jit_call_decoded` has directly as `args_slice` — into
`route_jit_exception_through_method`, and build the handler frame with them instead of
`NO_ARGS`. `Frame::new_pooled → copy_args_to_locals` performs the category-2 (long/double)
two-slot expansion, exactly matching a normal method invocation, so locals `0..nargs` are
populated identically to method entry.

Incoming args are a verifier-consistent state for **any** handler in the method: an
exception can be thrown at the protected region's first instruction, where locals still
equal the method-entry values, so the handler's local-type merge always includes that
state.

**Residual limitation (pre-existing, unchanged):** locals first assigned *inside* the try
block (slot ≥ nargs) are still left uninitialized — recovering those needs a per-throw
deopt map of locals, which the JIT does not record (the deferred "precise JIT maps"
work). The dominant and previously-broken case — `this` and parameters — is now correct.

## Verification
- `TCRepro`: `bad 49998 → 0` (THRESH=2) and `49500 → 0` (default threshold).
- [`repro/ExcSemantics.java`](../../../cratonvm/wildfly-suite/repro/ExcSemantics.java) —
  6 scenarios (catch reads this+param, nested try/catch, try/catch/finally, static-method
  catch with a `long` param, throw-inside-catch, pre-try local) — **byte-identical to
  HotSpot** under B-on THRESH=2. Confirms the fix and no regression of normal exception
  control flow.
- WildFly clustering classes, **small** THRESH=2 batch (6 classes): the `ThrowableCollector`
  `this`=null NPE is gone; each fails with `LifecycleException: Could not start container`
  == HotSpot. **Caveat:** a *larger* warmed-up batch then surfaces the separate discovery
  bug ([bug-06](bug-06-jit-junit-discovery-reflection-corruption.md)) — so this fix alone
  does not make the full clustering module clean.
- ejb.interceptor classes (the `pruneStackTrace` cluster): no VM-bug signature (normal
  `ConfigurationException`, == HotSpot).

## Affected workloads
Any JIT-compiled instance method that catches an exception thrown by a callee and reads
`this`/params in the handler — generic, not WildFly-specific. The WildFly **execution**
phase would hit it (proven under THRESH=2), but in the production run the clustering tests
fail earlier in **discovery** (bug-06), so this fix does not change the WildFly cluster
counts on its own. It is kept as an independent correctness fix (standalone-proven).
