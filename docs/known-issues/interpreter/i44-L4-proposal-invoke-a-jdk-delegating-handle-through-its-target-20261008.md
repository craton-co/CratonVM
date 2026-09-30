# Proposal: invoke a JDK `DelegatingMethodHandle` through its target

**Status: proposal, filed 2026-10-08 by interpreter round i1 wave 44, lane
L4. Step 1 built in wave 45 (behind a switch that is on); steps 2 and 3
wait for the host run of `L4W45DelegatingHandleTarget`.**

## Progress (wave 45) — lane L4

* **Step 1 built** (`native-builtins/src/lang_invoke.rs`, `mh_door_handle` /
  `mh_jdk_delegation_target`, switch
  `DELEGATING_HANDLES_DISPATCH_THROUGH_TARGET`, `true` in its own last
  commit). The `invoke`, `invokeExact` and both `invokeWithArguments`
  registrations, `mh_invoke_door` / `mh_invoke_exact_door` (which the
  invoker arms enter with the handle they were given), `mh_invoke_basic`
  and `mh_dispatch_body` (a combinator's target) replace a handle WITHOUT
  this model's slots (`object_num_fields <= MH_BOUND`) whose class is a
  subclass of `java/lang/invoke/DelegatingMethodHandle` by its `target`
  field, nested delegation followed up to 8 deep. The field is read, not
  `getTarget()` called: the class is sealed, all four permitted subclasses
  keep a `private final MethodHandle target`, and a field read runs no Java
  (nothing the door holds can move). Taken only when the delegating
  handle's `type` is its target's; `MethodHandleImpl$AsVarargsCollector` is
  left alone (its `asType` collects at another arity, which the target does
  not). Hot-path cost: one width compare per door; synthetic handles are 25
  slots wide and never enter the cold half.
* **Positive control / step-2 question:** probe
  `tools/probes/interp/L4/L4W45DelegatingHandleTarget.java` (13 rows,
  HotSpot 25's lines in its header; run with `--add-opens
  java.base/java.lang.invoke=ALL-UNNAMED`) builds the JDK handles through
  reflection on `MethodHandleImpl.makeIntrinsic` and
  `makeArrayElementAccessor`, so nothing pinned stands in front, then
  invokes them through every door. With `CRATONVM_DBG_MH_DISPATCH=1` each
  `intrinsic-*` invocation prints `[MH_DELEGATING]
  java/lang/invoke/MethodHandleImpl$IntrinsicMethodHandle (II)I -> target
  java/lang/invoke/MethodHandle`. On the base the doors read `MH_KIND` /
  `MH_DESC` out of the delegating handle's bounds. **What the host run
  decides:** if `intrinsic-class` and `accessor-class` print the class, the
  JDK constructor (`chooseDelegatingForm` -> `makeReinvokerForm` ->
  `LambdaForm.prepare`) survives over CratonVM's handles, step 2 is not
  needed, and step 3 can start (retire `arrayElementGetter` /
  `arrayElementSetter` / `arrayLength` through
  `native-api/src/retired_shadow.rs` with `L4W44RecordDeserialization` and
  this probe as guards). If they print an exception, its text names the
  failing step of the constructor; step 2 is then the narrow `Intrinsic`
  on `DelegatingMethodHandle.chooseDelegatingForm(MethodHandle)` the
  Direction describes, answering, for a target with this model's slots, a
  `LambdaForm` whose `prepare()` is a no-op (`isCompiled = true`, a
  non-null `vmentry`), and for any other target the JDK body's own two
  arms (`SimpleMethodHandle` -> `target.form`; else `makeReinvokerForm(target,
  MethodTypeForm.LF_DELEGATE, DelegatingMethodHandle.class, NF_getTarget)`).
* `identity` is not a step-3 candidate as the Direction hoped: JDK 25
  `MethodHandles.makeIdentity` wraps a `SimpleMethodHandle.make(type,
  LambdaForm.identityForm(..))`, a form-driven handle this model cannot
  invoke either, so its target is no better than the wrapper.

## Why

Several `MethodHandles` factories are pinned natives
(`vm/src/vm/vm_exec.rs` `check_override`, the "METHODHANDLES ..." rows)
only because their JDK 25 bodies hand back a real JDK handle the `MH_KIND_*`
model cannot invoke. Wave 44 read the one that blocks the
`copyWith`-workaround retirement
(`i43-L4-proposal-retire-the-copywith-workarounds`, step 1):
`MethodHandleImpl.makeArrayElementAccessor` ends in `makeIntrinsic(mh,
intrinsic)`, a `MethodHandleImpl$IntrinsicMethodHandle`. The same class is
what `MethodHandles.identity` answers (the `identity` pin's comment), and
`asVarargsCollector`'s JDK body builds `MethodHandleImpl$AsVarargsCollector`,
another `DelegatingMethodHandle` (the `asVarargsCollector` pin's comment).

A `DelegatingMethodHandle` is behaviourally its target: `getTarget()` names
the handle it forwards to, and `IntrinsicMethodHandle` adds only an
intrinsic tag (`intrinsicName()`) the JIT reads. Its target is, in these
routes, one of CratonVM's own synthetic handles (`findStatic` of
`ArrayAccessor.getElementI`, then `viewAsType` -> `copyWith`, registered in
wave 43).

## Direction

1. Teach the invoke door (`lang_invoke::mh_invoke_door` /
   `mh_dispatch_body`) and `invokeBasic` (`mh_invoke_basic`) to recognise a
   real JDK `DelegatingMethodHandle` subclass instance without CratonVM's
   slots (`object_num_fields <= MH_BOUND`, class a subclass of
   `java/lang/invoke/DelegatingMethodHandle`), read its target through the
   JDK's own `getTarget()` (an ordinary virtual call; `IntrinsicMethodHandle`
   and `AsVarargsCollector` keep it in a final field), check the call-site
   type against the delegating handle's own `type`, and dispatch the target.
2. Make the construction side survive: `DelegatingMethodHandle(MethodType,
   MethodHandle)` runs `chooseDelegatingForm(target)` ->
   `makeReinvokerForm`, which reads `target.form` and builds or caches a
   `LambdaForm`. Either give the synthetic handles a shared placeholder
   `LambdaForm` per basic type (the cache path, `MethodTypeForm.cachedLambdaForm`)
   or register a narrow `Intrinsic` for `chooseDelegatingForm` that answers a
   shared inert form. Measure which one the JDK code tolerates.
3. Then unpin, one factory at a time, with the probes that pinned them:
   `arrayElementGetter` / `arrayElementSetter` / `arrayLength`
   (`L4W44RecordDeserialization`, `ObjectStreamClass$RecordSupport`),
   `identity` (`L4W40*` / `L4W42CombinatorChecks`), `asVarargsCollector` /
   `asFixedArity` (`probes/VarargsCollectorProbe.java`, RJdkHandles).

## Cost and measurement

Per-call cost: one class check on the (already cold) path for handles
without CratonVM's slots, which today answer `null` or an internal error.
Correctness first: every probe above, both modes. `--compatible` keeps its
pins (the retirement mechanism in `native-api/src/retired_shadow.rs` re-tags
per triple for `--jdk-only` only).
