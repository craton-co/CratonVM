# Bug 07 — Every AnnotationProxy method call paid the full resolution-miss cascade (perf)

**Severity:** Medium — **perf, CratonVM-only**. Correctness was already fine; this is
a hot-path inefficiency in annotation-heavy reflection.

**Status: FIXED** (main checkout `C:\craton\cratonvm`, branch `dev`;
`vm/src/vm/vm_exec.rs` + `vm/src/jit/helpers.rs`).

## Symptom
CratonVM represents an annotation instance as a synthetic
`java/lang/annotation/AnnotationProxy` object that has **no bytecode methods** — its
methods (`annotationType()`, `value()`, element accessors, `equals`/`hashCode`/
`toString`) are served by the Rust `annotation_proxy_invoke_shared` rescue. That rescue
lived at the **very bottom** of `invoke_or_native` ("last resort before NSME"), so every
call on a proxy first traversed the entire class-load + method-resolution-miss + native-
probe cascade before being recovered. JIT-compiled, reflection-heavy framework code
(JUnit/Arquillian observer scanning — `Reflections.isObserverMethod`,
`ExtensionUtils.streamDeclarativeExtensionTypes`; Spring annotation scanning) calls these
thousands of times. In WildFly `ClusteredJPA2LCTestCase` discovery, the cascade dominated
runtime; with `CRATONVM_DBG_NSME=1` the per-call NSME-probe logging flooded and the class
timed out (303+ `AnnotationProxy.annotationType` probe lines in one short batch).

## Fix
Route AnnotationProxy method calls **before** the cascade:
- `invoke_or_native` (`vm_exec.rs`): a fast-path at the top — when the dispatch class is
  `java/lang/annotation/AnnotationProxy`, go straight to `annotation_proxy_invoke_shared`.
  Keyed on the **dispatch class** (not just arg0), so a *static* call that merely passes an
  annotation as its first argument (e.g. `Objects.requireNonNull(annotation)`, whose
  `class_name` is the declaring class) is unaffected. `annotation_proxy_dispatch_impl`
  already handles every method (members + `Object` `equals`/`hashCode`/`toString`/
  `getClass`), so routing all of them is correct.
- `jit_invoke_virtual_mic` (`helpers.rs`): an analogous fast-path next to the existing
  lambda-proxy short-circuit — when `receiver_class_id` is the cached AnnotationProxy id,
  dispatch directly, skipping the failed JIT compile-probe + the `invoke_or_native` call.
  Uses a lock-free cached cid (`annotation_proxy_cid_hint`, an `AtomicU32` warmed by the
  `invoke_or_native` fast-path); before warm-up the hint is `u32::MAX` (never a real id) so
  the JIT path simply falls through to the (correct, self-warming) slow path.

## Verification
- **Correctness unchanged:** `repro/JUnitReflRepro.java` (real
  `AnnotationSupport.findAnnotation` + `ReflectionSupport.findFields`) is **byte-identical
  to HotSpot** (`ref0`/`ref1` match, `ok=N`), and unchanged under `CRATONVM_DBG_GC_STRESS`
  (bug-06 GC-safety intact).
- **Perf:** the warmed clustering batch (CDIFailover, CommandDispatcher,
  ClusteredJPA2LCTestCase) under `CRATONVM_DBG_NSME=1` now emits **0**
  `AnnotationProxy.annotationType` probe lines (was 303+), and ClusteredJPA2LCTestCase —
  which previously timed out under the diagnostic — **completes** (status FAIL =
  benign no-container, == HotSpot), with **0** VM-bug signatures.
- No regression to bug-05 (`TCRepro`/`ExcSemantics` == HotSpot) or bug-06 (GC-stress repros).

## Caveat audit — GC-rooting of the fast-path return value (RESOLVED: safe)
The fast-path returns its result object as `obj.as_ptr() as i64` without going through
`safe_native_call`'s `native_pending_return` rooting. Audited:
- **Code parity:** the *normal* MIC path (`jit_invoke_virtual_mic`, helpers.rs ~3525) returns
  a bytecode-callee's result the exact same way — `Some(Value::Object(Some(obj))) =>
  obj.as_ptr() as i64`, with no `native_pending_return` set in the helper. So the
  AnnotationProxy and lambda-proxy fast-paths use the **identical, dominant, long-proven**
  return convention as ordinary monomorphic instance dispatch. The return is rooted by the
  JIT caller's operand-stack store + conservative root scan, not by `native_pending_return`
  (which only the native-call sub-path inside `safe_native_call` sets, as extra coverage
  across that boundary). There is no allocation between the helper returning and the JIT
  caller rooting the value, so no GC window exists.
- **Empirical:** `repro/CaveatRepro.java` hammers `annotationType()`/`value()`/`toString()`
  (the last returns a *freshly allocated* String, consumed immediately in an allocating
  expression) on a JIT-compiled call site under `CRATONVM_DBG_GC_STRESS=65536` — 80 000
  iters, byte-identical to HotSpot (`ok=80000 bad=0`), with heavy GC confirmed.

Conclusion: no tightening needed; the fast-path return handling is GC-safe and consistent
with the normal MIC path.
