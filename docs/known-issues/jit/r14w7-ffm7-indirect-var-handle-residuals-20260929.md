# IndirectVarHandle residuals after FFM6-1 (exactness, narrow call sites)

Status: OPEN
Area: `VarHandle` access natives (`native-builtins/src/lang_invoke.rs`, block
"`IndirectVarHandle` (round 14 wave 7, lane ffm7; proposal FFM6-1)")
Severity: medium (item 3: three filterValue roads throw NoSuchMethodError; items 1-2 a missing exception; never a wrong value)
Found by: round 14 wave 7 lane ffm7

## What is wrong

Round 14 wave 7 serves every access through an `IndirectVarHandle` by invoking the
composed mode handle through `invokeWithArguments` (`vh_indirect_dispatch`). Two HotSpot
refusals are not reproduced:

1. **Exact indirect handles.** `MethodHandles.filterValue(..).withInvokeExactBehavior()`
   runs `IndirectVarHandle.withInvokeExactBehavior` (real bytecode) and sets `exact`.
   HotSpot then throws `WrongMethodTypeException` when the call-site type is not exactly
   `accessModeType(mode)`. CratonVM's exactness door
   (`varhandle_exact_call_site_refusal`, `lang_invoke.rs` ~5152) knows side-table
   handles, real array handles and FFM layout handles only; for an indirect handle it
   returns `None` (accept), and `vh_indirect_serve` converts the arguments
   permissively.
2. **Narrowing through an `int` slot.** A call site that passes `int` where the handle's
   coordinate or value is `char`/`short`/`byte`/`boolean` is boxed as the DECLARED type
   (`vh_indirect_box_input`), because a `Value::Int` cannot say which it was. HotSpot's
   `asType` refuses `int -> char` with `WrongMethodTypeException`. The static call-site
   descriptor is not available to the native (`poly_call_site` publishes only the value
   classes, `take_vh_value_site`).

## Proposed fix

1. In `varhandle_exact_call_site_refusal`, `None` arm: when
   `is_indirect_var_handle(ctx, vh)` and the real `exact` field is set, build the
   expected descriptor from the handle's `value` and `coordinates` fields (mirror ->
   descriptor with a REFUSE-on-unknown helper, not `mirror_to_descriptor`, whose
   `Object` fallback would invent a mismatch) and call `varhandle_exact_mismatch`.
2. Have `vm_exec`'s signature-polymorphic dispatch `arm` the full call-site descriptor
   for VarHandle receivers of class `IndirectVarHandle` (it already arms it for
   `MethodHandle.invoke`), and let `vh_indirect_dispatch` go through the `invoke` door
   (`mh_invoke_door` semantics, call site = the arm with `VarHandle` prepended) instead of
   `invokeWithArguments`. That also removes the per-access `Object[]` and boxing (see
   `jit-r14-ffm7-proposals.md` F7-1).

## Confirm

Java probe: `VarHandle e = MethodHandles.filterValue(x, plus, minus).withInvokeExactBehavior();
e.set((Object) h, 1)` -> HotSpot `WrongMethodTypeException`; CratonVM stores. And a
`char` field under `insertCoordinates` set with an `int` argument -> HotSpot WMTE.

## Round 14 orchestrator: w7b probe run

`R14Ffm7IndirectVarHandle` on w7b (default `--jdk-only`): 8 of 11 roads match HotSpot 25
(`insertCoordinates` get/set and getAndAdd, `dropCoordinates`, `permuteCoordinates`,
`collectCoordinates`, `filterCoordinates`, `insertCoordinates` over a long field, and the
`filterValue` CAS road), and the describe line matches. The three roads whose handle factory
applies a VALUE filter to a plain access mode (`filterValue` get/set, the nested
`insertCoordinates(filterValue(..))`, and `filterValue` over a static long) throw
`NoSuchMethodError: java/lang/invoke/MethodHandle.editor()Ljava/lang/invoke/LambdaFormEditor;` from
`MethodHandles.collectReturnValue`. The JDK's `VarHandles.filterValue` factory composes with
`MethodHandles.filterReturnValue` / `collectArguments`, and CratonVM's `MethodHandle` carriers have
no `LambdaForm` editor. This is a MethodHandle-layer gap, not a VarHandle one, and the access now
THROWS where it used to answer null silently (the floor this wave set).

Item 3 (new): serve `filterReturnValue` / `collectArguments` for the composed mode handle without
`MethodHandle.editor()`. For example, `vh_indirect_serve` can recognise the `filterValue` factory
and apply the two filters itself (`filterToTarget` on the stored value, `filterFromTarget` on the
result) around the target's access, or the MethodHandle combinators can get a native. Severity:
medium. It is an exception, not a wrong answer, and `CRATONVM_VH_INDIRECT_SERVE=0` gives the UOE
floor.
