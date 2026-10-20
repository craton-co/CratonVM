# Known gap: `SHOW_HIDDEN_FRAMES` walks lack the frames of the `LambdaForm`s HotSpot spins

**Status: KNOWN GAP (unaddressable by design).** Split off by the interpreter round i5 orchestrator
(2026-10-16) from page `i104-L3-served-invoke-with-arguments-lists-no-frame`. That page's fixed
part (the default, `SHOW_REFLECT_FRAMES` and throwable rows) was verified and retired.

## Symptom

`tools/probes/interp/L3/L3W104WalkOptionsThroughHandles.java`: two `hidden` rows
(`StackWalker` with `SHOW_HIDDEN_FRAMES`) still differ from HotSpot 25, in both modes. All other
rows are SAME. HotSpot 25.0.3 (Windows; Linux prints the same frames):

```
invokeWithArguments hidden: [..::walk, ..::target, ..::targetInt, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments]
handle-to-Method.invoke hidden: [..::walk, ..::target, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, jdk.internal.reflect.DirectMethodHandleAccessor::invokeImpl, jdk.internal.reflect.DirectMethodHandleAccessor::invoke, java.lang.reflect.Method::invoke, java.lang.invoke.DirectMethodHandle$Holder::invokeSpecial, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.LambdaForm$MH::invoke_MT]
```

CratonVM (builds `6909a8bc2b` / `728410c07f`):

```
invokeWithArguments hidden: [..::walk, ..::target, ..::targetInt, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments]
handle-to-Method.invoke hidden: [..., java.lang.reflect.Method::invoke, java.lang.invoke.LambdaForm$DMH::invokeVirtual, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.LambdaForm$MH::invoke_MT]
```

## Why this is not fixed

The missing `LambdaForm$MH::invoke` frames are methods of hidden classes that HotSpot SPINS at run
time (`LambdaForm$MH/0x...`): the spreader and the generic invoker forms that `invokeWithArguments`
needs when the shape is not pregenerated in `$Holder`. CratonVM serves these calls without spinning
those classes. Naming the frames would invent hidden classes that `StackFrame.getDeclaringClass()`
and `getMethodType()` would then have to answer.

The second row's instance holder is a related case. HotSpot shows `DirectMethodHandle$Holder::invokeSpecial`, and
CratonVM names the spun `LambdaForm$DMH::invokeVirtual`. The served frame does not record which kind
the JDK would have picked.

Listing only the `$Holder` frames would leave the rows different, so it buys nothing. The same
class of gap already applies to `invokeExact` rows whose shape is not pregenerated.

## What would close it

Running the JDK's own `LambdaForm` spinning (`InvokerBytecodeGenerator`) for these shapes instead of
serving them, which is the direction of proposal `i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`.
Until then, these two rows are expected DIFFs in the battery.

## Wave 108

Interpreter round i5 wave 108, lane L3. **Partly fixed in code (not built or run); the page stays
open for the `invokeWithArguments hidden` row.**

### The `handle-to-Method.invoke hidden` row: a defect, fixed

The bottom of this row was not a spun-class gap. HotSpot's `DirectMethodHandle$Holder::invokeSpecial`
is a pregenerated holder; CratonVM listed the spun `LambdaForm$DMH::invokeVirtual` because its
`MethodHandleNatives.resolve` (`native-builtins/src/lang_invoke.rs`, `native_mhn_resolve_uncensused`)
kept `REF_invokeVirtual` for `findVirtual(Method.class, "invoke", ..)`. HotSpot's
`LinkResolver::runtime_resolve_virtual_method` / `CallInfo::set_virtual` treats a statically bound
method as a `direct_call`, and `init_method_MemberName` writes `REF_invokeSpecial` for it. That covers
a private method, a final method, and any method of a final class (`java.lang.reflect.Method` is
final). The interface-to-`Object` case is included too. `DirectMethodHandle.preparedLambdaForm`
then picks `LF_INVSPECIAL`, which the JDK image pregenerates for this shape. `native_mhn_init` has
written this kind for a reflected `Method` since SBR3; the resolve path now agrees with it.

* `mhn_virtual_is_direct_call` (new) decides the kind from the method the wave-23 search found (its
  own flags and holder) and fails closed when the search did not find it. A default method is never
  a direct call. A private interface method is (`REF_invokeInterface` request, `invokeSpecial`
  member).
* `real_direct_target_decode` maps a `REF_invokeSpecial` member back to the request's kind for the
  served real-handle lanes on any handle that is not a `$Special`: 5 on a plain
  `DirectMethodHandle`, 9 on a `$Interface`. Those lanes keep serving `findVirtual(String.class, ..)`
  and similar handles exactly as before (a virtual dispatch binds the same method). Only
  `findSpecial` / `unreflectSpecial` mint a `$Special`.
* `hidden_invoke_standin_frames` (`vm/src/runtime/stackwalker.rs`) now also names the holder of a
  statically bound instance target on a served `invokeExact` / `invoke` / `Method.invoke`:
  `DirectMethodHandle$Holder.invokeSpecial` with shape `(Object, Object, args...)`. A new
  `HiddenInvoke` row backs this, and it fails closed when the image does not pregenerate the shape.
* The `--compatible` finder's refusal text (`lk_enforce_find_access`) names a final method found
  virtually as `/invokeSpecial`, matching what HotSpot's `MemberName.toString` prints.

Probe: `tools/probes/interp/L3/L3W108FinalVirtualHandleKind.java` (HOTSPOT_EXPECTED block; rows
`* below`, `* reveal`, null receiver, result). Also rerun `L3W104WalkOptionsThroughHandles`: row
`handle-to-Method.invoke hidden` should now be SAME in both modes.

**What the orchestrator must check first:**

1. `L3W108FinalVirtualHandleKind` in both modes, JIT and `--nojit`.
2. The `reveal` rows. They must still say `invokeVirtual`; the JDK's `revealDirect` maps the kind
   back through `isInvokeSpecial()`.
3. `L4W48LookupFinderAccessText`, `L4W25RevealDirect`, `L4W54RealJdkHandleArgs` and
   `L4W55LinkToFromBytecode`, which exercise the kinds.
4. A `findVirtual` microbench row over a final class (for example `String.length`), in case a fast
   lane that keys on kind 5 was missed. `real_direct_target_decode` is the one decode the lanes
   share.

### The `invokeWithArguments hidden` row: stays a gap

Remaining DIFF, both modes (HotSpot 25.0.3 above). CratonVM lists
`targetInt, DirectMethodHandle$Holder::invokeStatic, Invokers$Holder::invokeExact_MT,
MethodHandle::invokeWithArguments`. HotSpot has two more `LambdaForm$MH::invoke` frames between the
holder and the invoker:

* the `asType(genericMethodType(1))` adapter (`(Object)Object` to `(int)int`);
* the spread invoker that `invocationType.invokers().spreadInvoker(0)` binds.

Both are methods of hidden classes HotSpot spins (`LambdaForm$MH/0x...`). The probe's `fold` strips
the address, so the names alone would compare. But `MethodHandle.invokeWithArguments` is served by
`Intrinsic`s in both modes (`lang_invoke.rs`, the two `register_with_kind(mh,
"invokeWithArguments", ..)` rows). No such class exists, so a listed frame would have no
`ClassId`. Every walk path that builds a `StackFrameInfo` / `ClassFrameInfo` from an entry
(`lang_stackwalker.rs` `sfi_element_from_walk_lines`, `populate_class_frame_info`) needs one. They
would drop the frame or answer `getDeclaringClass()` wrongly under `RETAIN_CLASS_REFERENCE`.

What would close it: run the JDK's `invokeWithArguments` body instead of the `Intrinsic` when the
walk can see it. Two ways to do that:

* the `Intrinsic`s decline in favour of the JDK bytecode, which then spins and runs the forms. This
  is a performance decision (L4), behind a switch with an A/B.
* or the door spins the spread invoker through the JDK on a SHOW_HIDDEN capture.

The same applies to the `-XX:+ShowHiddenFrames` throwable of that row, which the probe does not
print.

## Wave 108 hand-back

Built as w108 (`9218e79892`, Windows). The final-class, final-method, private, unreflect and
`Method.invoke` rows of `L3W108FinalVirtualHandleKind` are SAME in both modes, with the JIT and
with `--nojit`. Two follow-ups are fixed in code (not built or run):

1. **Regression in `L4W48LookupFinderAccessText` rows `pkg-private-ctor` and `mod-package-ctor`.**
   CratonVM printed `<init>(..)void/newInvokeSpecial`; HotSpot prints `/invokeSpecial`.
   - HotSpot's `init_method_MemberName` writes `IS_CONSTRUCTOR | REF_invokeSpecial` for every
     resolved constructor (the `is_initializer` arm of `direct_call`). `Lookup.checkAccess`'s
     refusal prints that kind.
   - `native_mhn_resolve_uncensused` now treats a found `<init>` (request `REF_newInvokeSpecial`)
     as a direct call too, and settles it as kind 7, under the same switch
     `CRATONVM_MHN_RESOLVE_DIRECT_CALL`.
   - `DirectMethodHandle.makeAllocator`'s `asConstructor()` turns the kind back into
     `REF_newInvokeSpecial` for the handle's member. Allocator handles and every native reading a
     handle's member still see 8.
   - The `pub-*-ctor` rows keep `/newInvokeSpecial`, as HotSpot does. Their access failure is
     raised inside resolve, before the flags write.
   - The `--compatible` finder text (`lk_enforce_find_access`) already printed `invokeSpecial` for
     a constructor; the wave-108 hunk there leaves that arm unchanged.
   - I could not trace from the source how w107 printed the HotSpot text. Check the row in both
     modes first.
2. **`L3W108FinalVirtualHandleKind` row `plain findVirtual below`** (a gap that predates wave
   108). On a served `invokeExact` of a non-final virtual method, no hidden frame was listed
   below the target. `hidden_invoke_standin_frames` now names `DirectMethodHandle$Holder.invokeVirtual`
   (shape: handle and receiver first) in these conditions:
   - the target's class is not an interface;
   - no interface that class or its superclasses implement declares the method
     (`superinterface_declares`; it fails closed);
   - the image pregenerates that shape.

   Such a method is reached by `findVirtual` / `unreflect` / `bindTo` on a class, which gives
   HotSpot's `invokeVirtual`. The remaining residuals:
   - **`findSpecial` handle of such a method:** HotSpot shows `invokeSpecial`. The frames do not
     say how the handle was looked up, so this is named wrongly; before, nothing was named.
   - **A method an interface declares:** the handle could be an `$Interface` (`invokeInterface`),
     so nothing is named.
   - **A shape `DirectMethodHandle$Holder` does not pregenerate:** HotSpot spins
     `LambdaForm$DMH`, and nothing is named.

   Expected row: `plain findVirtual below: DMH::invokeVirtual`.

## Wave 109

Interpreter round i5 wave 109, lane L4. **Built behind a default-OFF switch (not built or run):
`CRATONVM_MH_IWA_JDK_BODY`.** The page stays open until the orchestrator has the frames and the
A/B; with the switch on and the rows SAME it can close as "fixed under the switch" or the switch
can be flipped after the A/B.

### Decision: run the JDK body, do not model the frames

Two ways were on the table. The stand-in rows (`hidden_invoke_standin_frames`) cannot name the two
frames faithfully: they are methods of hidden classes HotSpot spins per shape, and this VM never
defines them when the `Intrinsic` serves the call, so `getDeclaringClass()` /
`RETAIN_CLASS_REFERENCE` and `getMethodType()` would have nothing true to answer (L3's wave-108
finding). The JDK body, on the other hand, already runs in this VM on the door's decline path
(`mh_real_invoke_with_arguments_door` falls back to `invoke_virtual_bytecode_only` when its lanes
decline), and the `invoke converting` row of `L3W104WalkOptionsThroughHandles`, which goes through
the JDK's `asType` adapter as a real spun `LambdaForm$MH`, is already SAME. So the faithful and
cheap-to-build fix is to let the body run.

### Built

`native-builtins/src/lang_invoke.rs`, `mh_iwa_jdk_body` (called first by both
`MethodHandle.invokeWithArguments` registrations): with the switch, a REAL handle that is not a
variable-arity collector and not a VM-minted downcall carrier (`mh_with_arguments_runs_real`)
runs `invoke_virtual_bytecode_only(this, "invokeWithArguments", <overload>, args)`. No reflective
mark is entered (the body's own interpreter frame is the `invokeWithArguments` frame, so the
wave-104 splice must not list it again). The `List` overload's body calls the array overload
virtually, which comes back to the `Intrinsic` and runs the array body. Varargs collectors keep
the door (their override's `super.invokeWithArguments` would loop through a virtual re-dispatch,
see `mh_real_invoke_with_arguments_door`). Carriers keep the door (`NoSuchMethodError:
MethodHandle.editor()` on the JDK body).

Switch: `NativeFlags::mh_iwa_jdk_body`, `truthy_word`, read per call; `types/src/flag_groups.rs`
row `mh-iwa-jdk-body` and `types/tests/flag-surface.txt` line added.

### Probes

* `tools/probes/interp/L4/L4W109IwaHiddenFrames.java` (HOTSPOT_EXPECTED): `array` and `list` rows,
  default / hidden / trace / result. Switch off: the two `hidden` rows lack both
  `LambdaForm$MH::invoke` frames (as today). Switch on: every row SAME.
* `tools/probes/interp/L3/L3W104WalkOptionsThroughHandles.java` row `invokeWithArguments hidden`
  with `CRATONVM_MH_IWA_JDK_BODY=1`, both modes.
* Bench `tools/probes/interp/L4/L4W109InvokeWithArgumentsBodyBench.java` (`TIME` rows
  `static-int`, `static-ref`, `virtual`, `bound`, `list`, control `varargs-control`; checksums
  and `wmte` in HOTSPOT_EXPECTED). SBR4 measured the JDK body at 106-149 us against 25 us for the
  old carrier door; the served real-handle lanes are much faster now. Expect the switch-on rows
  to be several times the switch-off rows unless the spun forms compile; `varargs-control` flat.

### What the orchestrator must check first

1. `L4W109IwaHiddenFrames` with the switch on, `--jdk-only` and `--compatible`, JIT and
   `--nojit`. If the `hidden` rows show the two `LambdaForm$MH::invoke` frames but LACK
   `Invokers$Holder::invokeExact_MT`, the body's `spreadInvoker(0).invokeExact(..)` was served by
   the native `invokeExact` door with no linker activation; then the stand-in table would have to
   add the invoker frame for a caller standing at that `invokeExact` above a real `LambdaForm$`
   frame. `hidden_invoke_standin_frames` has an `adapter` arm meant for exactly that, but the
   `real_linker(target)` `continue` above it skips every `LambdaForm$` target, so the arm is never
   reached (L3's code; not changed here).
2. If a `LambdaForm$...::interpret...` / `LambdaForm::interpretWithArguments` frame appears
   instead, the form was interpreted rather than spun: compare with the `invoke converting` row.
3. The MH battery with the switch on (`tools/probes/interp/L4/*`, in particular
   `L4W84InvokeWithArgumentsJumbo`, `L4W95IwaVarargsSuper`, `L4W98SpreaderConversion`), and the
   condy bootstrap probes (condy calls `invokeWithArguments` for a bootstrap with arguments).
4. The A/B rows above, `--jdk-only` and `--compatible`, JIT and `--nojit`.

## Wave 108 hand-back 2

w109 (`26d38e1431`): `L3W108FinalVirtualHandleKind` row `plain findVirtual below` still DIFF
(`L3W108FinalVirtualHandleKind::lambda$main$3`). Cause: the shape, not the branch's conditions.
JDK 25.0.3's image (Temurin `lib/modules`, read offline with a jimage reader) pregenerates
`DirectMethodHandle$Holder.invokeVirtual` for exactly two shapes: `(Object,Object)Object` and
`(Object,Object,Object)void`. `invokeSpecial` has many shapes, including `(Object,Object)void`,
which is why the final, private and unreflect rows were SAME. A void no-argument virtual target
(`(Object,Object)void`) therefore runs under a SPUN `LambdaForm$DMH/0x...::invokeVirtual` on
HotSpot. `holder_entry` finds no such holder method and, as designed, names nothing, so the walk
went straight to the caller.

That frame belongs to the same unnameable class as the `invokeWithArguments` row: a hidden class
HotSpot spins and this VM does not, with no `ClassId` for the walk's frame builders. It stays in
this page's gap.

The probe row now uses an `Object`-returning target (`Object plain()`), which runs under the
pregenerated `(Object,Object)Object` holder on HotSpot. The wave-108 `invokeVirtual` stand-in
branch names that holder. Expected: `plain findVirtual below: DMH::invokeVirtual` in both modes.
The void case remains as a known gap: any served `invokeExact` / `invoke` of a virtual target
whose erased shape is not one of the two pregenerated ones (most of them) lists no frame below
the target in a `SHOW_HIDDEN_FRAMES` walk. The same holds for `invokeSpecial` / `invokeStatic`
shapes the image lacks.

## Wave 110

w109 A/B: `CRATONVM_MH_IWA_JDK_BODY` was 15-30x slower (`static-int` 1385 -> 37485 ns), so it stays
OFF and this row stays a gap. Why the JDK body costs ~35 us here is page
`i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call-20261212`. In short: every JDK cache
hits, and the cost is ~6 native-to-Java re-entries along the `LambdaForm` chain at ~5 us each,
which the JIT cannot link through spun hidden classes. That page also has the census commands
(scope `iwa-body`, built in wave 110). The row can close once that cost is fixed and the switch
flips (check `L4W109IwaHiddenFrames` with the switch on at that point).

## Wave 114 (lane L4): what the `i110-L4` work changes here

The `invokeWithArguments hidden` row's fix (`CRATONVM_MH_IWA_JDK_BODY`) is still too costly to turn
on. Wave 114 removes one of the costs: `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` (default off, not
built or run) answers the body's `asType` from the handle's `asTypeCache` instead of an
interpreted re-entry. The w113 census measured that re-entry at ~11 us of the ~35-63 us body. The
frames the row needs do not change: `asType` has no frame of its own in HotSpot's walk, and the
spread invoker's and adapter's `LambdaForm$MH::invoke` frames still come from the JDK body. What
still keeps the body expensive is the `invokeExact_MT` -> `invokeBasic` chain (~34 us): the generic
re-entry cost (L7) and compiled `invokeBasic` into a spun hidden class (L2). The gap stays until
those two land and the orchestrator re-measures `L4W109InvokeWithArgumentsBodyBench` with both
switches on.

## Wave 115 (interpreter round i6, lane L4): the method-handle side

**OPEN** (known gap; nothing changed for the frames in wave 115). By reading:

* **The body allocates its spreader on every call, on HotSpot too.** JDK 25's
  `Invokers.spreadInvoker(0)` is `genericInvoker().asSpreader(Object[].class, n)` and is not
  cached; only the generic invoker is. The spread invoker's `LambdaForm$MH::invoke` frame is
  therefore a frame of a form that is shared between calls, under a fresh `BoundMethodHandle`
  each time, so a frame census or a JIT check must key on the form's spun class, not the handle.
  The cost side is corrected in `i110-L4-...` ("Wave 115").
* **`--compatible` may not show the spread invoker's frame even with `CRATONVM_MH_IWA_JDK_BODY=1`.**
  Under `--jdk-only`, `asSpreader` runs the JDK's bytecode (SBR4 MHCONS). Under `--compatible`,
  this VM's `MethodHandle.asSpreader` native stays in front of it, and its real-handle arm
  (`mh_foreign_adapter_through_jdk_cold`) is gated by `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK`
  (KEPT-OFF, `i54-L4`). So the JDK body's `genericInvoker().asSpreader(..)` answers this VM's
  `MH_KIND_SPREAD` carrier. Its `invokeExact` is then this VM's door, with no spun spread form and
  no `Invokers$Holder::invokeExact_MT` activation for that call. When the orchestrator runs
  `L4W109IwaHiddenFrames` / `L3W104WalkOptionsThroughHandles invokeWithArguments hidden` with the
  switch on under `--compatible`, check this first: one `LambdaForm$MH::invoke` frame (the
  `asType` adapter's) instead of two, or no `invokeExact_MT`, is this cause, not a walk defect.
  The `--jdk-only` row is unaffected.
* The cost that keeps `CRATONVM_MH_IWA_JDK_BODY` off now has three named owners: L4 (the body
  compiles: proposal `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`),
  L2 (`i115-L2-proposal-compiled-invoke-basic-linked-by-class-id`) and L7 (the per-re-entry cost,
  `i114-L7-native-reentry-cost-split`).

## Wave 115 (interpreter round i6, lane L3)

**SWITCH-OFF** (the walk side; not built or run). `CRATONVM_STANDIN_SPUN_IWA_FRAMES` (default
OFF, `truthy_word`, `NativeFlags::standin_spun_iwa_frames`, read per walk; INVENTORY COMPAT
`standin-spun-iwa-frames`). The question this wave was asked: can a walk list HotSpot's spun
`LambdaForm$MH` frames for a served `invokeWithArguments` without running the 35 us JDK body?
Yes, for a direct handle, and that is what is built.

### What HotSpot lists (measured, JDK 25.0.3, Windows)

`tools/probes/interp/L3/L3W115SpunIwaWalkFrames.java` prints every frame with its method type, bci,
line and whether its class is hidden. Innermost first:

```
target
DirectMethodHandle$Holder::invokeStatic                 (pregenerated; already named by the VM)
LambdaForm$MH::invoke(Object x (n+1))Object  [hidden]   the asType adapter: only when the
                                                         handle's type is not genericMethodType(n)
LambdaForm$MH::invoke(Object,Object,Object)Object [hidden]  the spread invoker (always)
Invokers$Holder::invokeExact_MT                          (pregenerated; already named by the VM)
MethodHandle::invokeWithArguments
```

So the frames are NOT a function of the walk alone. Their classes are spun per form (the spread
invoker's class is shared by every handle of arity n; the adapter's depends on the handle's type),
and their bcis come from the JDK's `InvokerBytecodeGenerator`. A handle of type
`genericMethodType(n)` (`tObj(Object)Object`) has no adapter frame. After 127 calls through the
same handle HotSpot customizes the adapter: the names stay, but the bytecode changes (bci 31 -> 35
for `tInt`).

### Why naming them from the walk is sound now

The obstacle recorded in waves 108-110 was that a named frame needs a real class for
`getDeclaringClass()` / `RETAIN_CLASS_REFERENCE`, and this VM never spins those classes when it
serves the call. The walk can ask the JDK for exactly the forms the body would use, without running
the body for the call. JDK 25's own steps (`MethodHandle.invokeWithArguments` bytecode):

* spread invoker: `MethodType.genericMethodType(n).invokers().spreadInvoker(0)`, then the
  `vmentry` of its `form`;
* adapter: `asType(genericMethodType(n))` on a direct handle of the frame's target. The adapter's
  form is a function of the handle's type and basic type, not of its kind, so a handle from
  `IMPL_LOOKUP.findStatic` / `findVirtual` / `findConstructor` of the target yields the same form.

Each frame is the form's `vmentry` method at its one `invokeBasic` call, with the class's own
`SourceFile` (`LambdaForm$MH`, line -1, as HotSpot). Its class is a real hidden class. The JDK
caches these forms, so the classes are the ones the body would have run, except a customized
adapter (above).

### Code

* `native-builtins/src/lang_stackwalker.rs`: `add_spun_invoke_with_arguments_frames`, called from
  `native_call_stack_walk_of` after `add_hidden_standin_frames` for a walk that shows hidden frames.
  The walker and the frame buffer are pinned across it, since Java runs.
  * `spun_iwa_sites` finds a target directly above the VM's `DirectMethodHandle$Holder` frame,
    then `Invokers$Holder.invokeExact_MT`, then the array overload's
    `MethodHandle.invokeWithArguments`. `n` is the holder's parameter count minus the handle.
  * `spun_iwa_entries` runs the JDK steps above, fail-closed. A thread-local guard stops a walk
    inside those calls from recursing.
* `native-api/src/registry.rs`: `NativeContext::spun_lambda_form_frame(class_id, name, descriptor)`,
  default `None`.
* `vm/src/vm/vm_exec.rs`: implements it with `stackwalker::spun_lambda_form_entry`, which takes
  only a `java/lang/invoke/LambdaForm$MH...` class and a method with exactly one `invokeBasic`
  call.
* Unit tests (`lang_stackwalker.rs`): `i115_l3_spun_iwa_sites_need_the_whole_served_chain`,
  `i115_l3_descriptor_parameter_count_reads_every_kind`.

### Residuals (expected DIFFs with the switch on)

* **A handle that is not direct** (bound, inserted, filtered). Its own forms stand between, and the
  target's arity is not `n`. Today the VM names the holder frame there only when the target looks
  directly bound. Where it does, this lists the direct-handle answer, which is wrong for such a
  handle. Before the switch, nothing was listed, which was also a DIFF.
* **A customized adapter** (127+ calls through one handle). Same names; HotSpot's bci differs.
  The probe's `again-int` row prints no spun bci for this reason.
* **`--compatible`.** This VM's `MethodHandle.asSpreader` native answers the JDK's
  `genericInvoker().asSpreader(..)` with a carrier (lane L4's Wave 115 note above), which has no
  spun form. The walk then lists nothing extra in `--compatible` (fail-closed). Expected: the
  switch-on rows SAME under `--jdk-only`, unchanged under `--compatible`.
* **The `-XX:+ShowHiddenFrames` throwable.** Not touched: no Java may run inside a capture.
* **Unpregenerated `invokeExact_MT` shapes** (`invokeExact` of `(int)int`: HotSpot shows a spun
  `LambdaForm$MH::invokeExact_MT`). This is the same kind of gap and could take the same approach
  (`Invokers.invokeHandleForm(..).vmentry`). Not built.

### What the orchestrator should run

1. `L3W115SpunIwaWalkFrames` (HOTSPOT_EXPECTED) under `--jdk-only` and `--compatible`, JIT and
   `--nojit`, with `CRATONVM_STANDIN_SPUN_IWA_FRAMES` unset and `=1`. Switch off: the `hidden`
   rows lack the `LambdaForm$MH` frames, and every other row is SAME. Switch on, `--jdk-only`:
   every row SAME. With `CRATONVM_DEBUG_STACKWALK=1`, each listed frame prints a
   `[SW-DBG] spun-iwa at=..` line.
2. `L3W104WalkOptionsThroughHandles`, row `invokeWithArguments hidden`, switch on: SAME under
   `--jdk-only`.
3. With the switch on: `L4W109IwaHiddenFrames` (switch `CRATONVM_MH_IWA_JDK_BODY` off) and the
   L3 walk battery (`L3W46InvokeFramesInWalks`, `L3W98StackWalkerOptions`,
   `L3W54StackFrameInfoShapes`). Nothing but the hidden IWA rows may change.

## Wave 116 (interpreter round i6, lane L3): hand-back, switch on lists nothing

**SWITCH-OFF** (not compiled or run). Hand-back on w115 (`b61e92bf3a`, Windows release): with
`CRATONVM_STANDIN_SPUN_IWA_FRAMES=1` no spun frame appeared, in both modes, JIT and `--nojit`, and
the run printed no reason. This wave makes every decline say why, and fixes the most likely
cause found by reading.

### Likely cause: the form had no spun `vmentry` when the walk asked

This VM's C33 bypass (`lang_invoke.rs`: `native_ibg_generate_interpreter_entry_point` /
`native_ibg_generate_customized_code`) hands `LambdaForm.prepare()` and `compileToBytecode()` a
`MemberName` that this VM fabricates. Its `clazz` is `java/lang/invoke/LambdaForm` itself; a real
`LambdaForm$MH/0x..` class is spun only when `CRATONVM_IBG_SPIN_ON_MISS` (default on) gets the JDK
generator through. A form that was only `prepare()`d with the interpreter entry, or whose
`vmentry` is still unset, therefore names no `LambdaForm$MH` class. The w115 code then declined
silently in `form_entry_class` / `spun_lambda_form_entry`: the class name did not start with
`java/lang/invoke/LambdaForm$MH`.

The fix in `native-builtins/src/lang_stackwalker.rs`, `form_entry_class`:

* when the form's entry is absent or is not a `LambdaForm$MH` class, and the form is not compiled
  yet (`isCompiled == false`), the walk runs the JDK's own `LambdaForm.compileToBytecode()`;
* that is what the handle's constructor does on HotSpot (`prepare()` with
  `COMPILE_THRESHOLD == 0`), and what the body would have done at its first call;
* with spin-on-miss it spins the form's class, and the walk then reads the entry again;
* a form already compiled (fabricated or not) keeps its entry, and the decline line says so.

The two halves (adapter, spread invoker) are now asked independently, so a decline names every
step that gave up.

### Decline lines (`CRATONVM_DEBUG_STACKWALK=1`)

Every line starts `[SW-DBG] spun-iwa decline:`. Each says what it saw:

* `no served chain under invokeWithArguments..: <three frames above it>`: the walk had an
  `invokeWithArguments` frame, but no direct-handle holder plus `Invokers$Holder.invokeExact_MT`
  chain above it (or a frame without a descriptor or class id). The frames are printed with
  their descriptors and ids.
* `<step>: raised <error>` / `<step>: answered <value>`: a Java call failed. The steps are
  `MethodType.genericMethodType`, `spread: MethodType.invokers()`,
  `spread: Invokers.spreadInvoker(0)`, `adapter: MethodType.fromMethodDescriptorString`,
  `findStatic` / `findVirtual`, `adapter: IMPL_LOOKUP.findConstructor`,
  `adapter: asType(genericMethodType(n))`.
* `<spread|adapter>: form <class> entry <class or why> (isCompiled=..)[; compiling it]`, then
  `compiled, entry still <class>` or `compileToBytecode raised ..`: the vmentry case above.
* `<spread|adapter>: <class> has no invoke<desc> with one invokeBasic (methods: ..)`: the class
  is spun but the method shape or the single-`invokeBasic` rule did not match. Its methods are
  listed.
* `site <class>.<method><desc> (n=..): nothing listed`: the summary for each site.

Successful frames still print `[SW-DBG] spun-iwa at=..` as before.

### What to run

* `CRATONVM_DEBUG_STACKWALK=1 CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 cratonvm --jdk-only -cp . L3W115SpunIwaWalkFrames 2>&1 | grep spun-iwa`
* Expected after the fix: `static-*` rows SAME under `--jdk-only`.
* If they still differ, the first decline line of each kind names the step; send it back to
  this lane.
* Under `--compatible` a `spread:` decline is expected (L4's Wave 115 note: the `asSpreader`
  native's carrier has no spun form).

## Wave 117 (interpreter round i6, lane L3): second hand-back, the step printed nothing

**SWITCH-OFF** (not compiled or run). On w116 (`af075b21ac`) the run printed 2513 `[SW-DBG]`
lines and none containing `spun-iwa`, not even a decline.

### Findings by reading

* **The gate is the only route.** `add_hidden_standin_frames` and the spun step have one caller,
  `native_call_stack_walk_of`. `fetchStackFrames` indexes the list that call built
  (`SW_FRAME_CACHE`). The throwable capture (`vm_exec::insert_show_hidden_standin_frames`) and
  `Thread.getStackTrace` (`lang_system.rs`) do not run Java and are not meant to list spun frames.
  So the step either was not called, or was called and stayed silent.
* **The direct spelling is honoured.** `flag_groups::resolve` only reports a direct
  `CRATONVM_STANDIN_SPUN_IWA_FRAMES=1` as `legacy_direct` (the per-flag notice); the process
  environment keeps it, and `NativeFlags` reads it with `truthy_word`, as for
  `CRATONVM_MH_IWA_JDK_BODY`. `CRATONVM_COMPAT=standin-spun-iwa-frames` is the same thing.
* **A silent path existed.** When no served chain matched, the wave-116 "no served chain" line
  was printed only for a frame whose class name was spelled exactly
  `java/lang/invoke/MethodHandle`. The site test compared the same internal spellings. Some
  capture paths hand dotted names: the served frame's own predicate,
  `stackwalker::is_invoke_with_arguments_frame`, accepts both spellings. The holder frames the
  VM inserts are internal-named, so a walk whose captured frames were dotted matched nothing and
  printed nothing. This is the likely cause, but it is not proven.

### Changed (`native-builtins/src/lang_stackwalker.rs`)

* `names_class` compares a frame's class name in either spelling. The site test uses it, and
  treats an `invokeWithArguments` frame with no descriptor directly under the invoker as the
  array overload.
* New `CRATONVM_DEBUG_STACKWALK=1` lines:
  * `[SW-DBG] spun-iwa gate shows_hidden=true switch=<bool> lists_standins=<bool> frames=<n>`:
    once per walk that shows hidden frames, before the gate. It says which input is false.
  * `[SW-DBG] spun-iwa called frames=<n> busy=<bool>`: once per call of the step.
  * `[SW-DBG] spun-iwa decline: window under <iwa frame>: <target> | <holder> | <invoker>`:
    every window under an `invokeWithArguments` frame that is not the served chain.
  * `[SW-DBG] spun-iwa decline: target .. class_id=.. descriptor=..`: a target missing either.
  * The wave-116 per-step decline lines, as before.

### What to run

```
CRATONVM_DEBUG_STACKWALK=1 CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 cratonvm --jdk-only -cp . L3W115SpunIwaWalkFrames 2>&1 | grep spun-iwa | sort | uniq -c | sort -rn | head -40
```

Read the result in this order:

1. No `gate` line: the walk did not reach this native with `shows_hidden`.
2. `switch=false`: the flag snapshot. Then retry with `CRATONVM_COMPAT=standin-spun-iwa-frames`
   and report.
3. `called` but only `window` lines: the frame shapes are printed there.
4. Per-step decline lines: as in Wave 116.

## Wave 120 (interpreter round i6, lane L3): the step was on the wrong route

**SWITCH-OFF** (in code; not compiled or run). The cause of the w115 and w116 hand-backs (no spun
frame, and no `spun-iwa` line at all) is found by reading.

**Cause.** The `StackWalker.walk` the probe makes is served by
`native-builtins/src/phases_late/reflect_invoke.rs` (`p59_sw_walk` / the forEach variant, through
`walk_frames_screened`), not by `lang_stackwalker::native_call_stack_walk_of`
(`AbstractStackWalker.callStackWalk`).

* Both routes call `add_hidden_standin_frames`, which is why the `DirectMethodHandle$Holder` and
  `Invokers$Holder` stand-ins appeared.
* Waves 115-117 put the spun step, its gate line and all its decline lines on the
  `callStackWalk` route only. So the switch could neither list a frame nor print a line on the
  probe's walk.
* The single `spun-iwa` line seen on w115 came from some walk that does take the `callStackWalk`
  route.
* The wave-117 notes ("the gate is the only route") missed this: the search for callers covered
  `native-builtins/src/*.rs` only, not `phases_late/`.

**Fix.** `walk_frames_screened` now runs `lang_stackwalker::add_spun_invoke_with_arguments_frames`
right after its `add_hidden_standin_frames`, under the same gate:

* the walker shows hidden frames;
* `CRATONVM_STANDIN_SPUN_IWA_FRAMES` is on;
* the VM lists hidden stand-ins.

Both callers hold every reference through pins (the function, the walker), as they already must
for `walker_shows_hidden_frames`, which runs Java. The thread-local guard still stops a walk made
inside the step's Java calls from recursing. `Thread.getStackTrace`'s use of
`add_hidden_standin_frames` (`lang_system.rs`, under `-XX:+ShowHiddenFrames`) is left alone.

**What still blocks a full SAME** (unchanged from Wave 115, stated precisely):

1. **A handle that is not direct** (bound, inserted, filtered). Its own forms stand between, and
   `n` is not the target's arity. The step answers for a direct handle only.
2. **An adapter HotSpot customized after 127 calls through one handle.** Same names, other
   bytecode, so another bci. The probe's `again-int` row prints no spun bci for this.
3. **`--compatible`.** This VM's `MethodHandle.asSpreader` native answers the JDK's
   `genericInvoker().asSpreader(..)` with a carrier that has no spun form (lane L4's Wave 115 note),
   so the spread step declines. Fixing that is L4's `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK`
   (KEPT-OFF).
4. **The `-XX:+ShowHiddenFrames` throwable.** No Java may run inside a capture, so the step cannot
   ask the JDK there.
5. **An `invokeExact` whose `invokeExact_MT` shape the image does not pregenerate.** Same family,
   not built.

**Run.**

```
CRATONVM_DEBUG_STACKWALK=1 CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 cratonvm --jdk-only -cp . L3W115SpunIwaWalkFrames 2>&1 | grep spun-iwa | sort | uniq -c | sort -rn | head -40
CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 cratonvm --jdk-only [--nojit] -cp . L3W115SpunIwaWalkFrames   (expected: SAME)
```

The first command should now print `spun-iwa called` lines. If any row still differs, the decline
lines from waves 116-117 name the step.

## Wave 121 (interpreter round i6, lane L3): the leftovers that need no Java in a capture

**SWITCH-OFF** (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`, new, default off). Not built or run by the
lane. Residual 5 is built. Residuals 1-4 are blocked, for the reasons stated below.

### Residual 5: the spun linker of an unpregenerated `invokeExact` (built)

Measured on HotSpot 25.0.3 (`tools/probes/interp/L3/L3W121SpunLinkerFrames.java`, `HOTSPOT_EXPECTED`,
`-Xint` and default flags agree). Only `(Object)Object` links to `Invokers$Holder.invokeExact_MT`.
`(I)I`, `()V`, `(II)I`, `(J)J` and `(String,double)double` link to a spun
`LambdaForm$MH/0x...invokeExact_MT`, whose descriptor is the handle, the basic parameters and the
appendix: `(I)I` gives `invokeExact_MT(Object,int,Object)int@19`. `(J)J` and `(String,double)double`
also show a spun `LambdaForm$DMH.invokeStatic` holder.

HotSpot links a site through `Invokers.methodHandleInvokeLinkerMethod`. That returns the `vmentry` of
`invokeHandleForm(T, false, LF_EX_LINKER = 10)`, checked against JDK 25's bytecode with `javap`. That
form erases `T` to `T.basicType()`.

The walk step (`native-builtins/src/lang_stackwalker.rs`) does four things:
1. `add_spun_linker_frames` finds each frame that stands at an `invokevirtual MethodHandle.invokeExact`.
   The class bytes are read through the new `NativeContext::method_handle_invoke_exact_site`
   (`vm_exec.rs`, `stackwalker::method_handle_invoke_exact_site`).
2. It acts only where the frame above that caller is a `DirectMethodHandle$Holder` or a
   `LambdaForm$` activation that is not itself a linker (`spun_linker_callee`).
3. It asks the JDK for the form, from `fromMethodDescriptorString(basicType, null)` and
   `Invokers.invokeHandleForm(.., false, 10)`. It then takes the form's entry class through the
   wave-116 compile-on-miss step, now split out as `lambda_form_entry_class`.
4. It lists the frame of `invokeExact_MT` at its one `invokeBasic` call
   (`spun_lambda_form_frame`), right above the caller.

Other details:
* The form is memoised per basic type within a walk.
* Shapes that take more than 253 slots get HotSpot's customized linker and are declined.
* `add_spun_lambda_form_frames` runs the IWA step and then this one, each under its own switch.
  Both walk routes (`native_call_stack_walk_of`, `phases_late/reflect_invoke.rs`
  `walk_frames_screened`) now call it.
* The same `SPUN_IWA_BUSY` guard applies.
* Under `CRATONVM_DEBUG_STACKWALK=1` the step prints `spun-linker called/at=` and the shared
  decline lines.
* Unit test: `i121_l3_spun_linker_basic_type_and_callee`.

Expected with the switch on:
* `int(int)`, `void()` and `int(int,int)` become SAME.
* `Object(Object)` stays SAME.
* `long(long)` and `double(String,double)` stay DIFF. Their spun `LambdaForm$DMH` holder is not
  named by the VM (`holder_entry` finds no pregenerated shape). Over a bare target, the step
  declines rather than guess whether the frame above the caller is the call's target or something
  the link ran.

The next step in the same family would be the spun DMH holder of a **static** target, whose kind
is certain. It would come from `IMPL_LOOKUP.findStatic(..).form.vmentry`, at its `linkToStatic`
call. That needs a VM reader for `LambdaForm$DMH` classes and `linkTo*` sites, because
`spun_lambda_form_entry` takes `LambdaForm$MH` and `invokeBasic` only. Not built.

### Residuals 1-4: why they stay

1. **A handle that is not direct.** The walk sees frames, not the handle. HotSpot's extra
   `BoundMethodHandle` / `LambdaForm$MH` frames depend on the handle's own forms, so naming them
   would need the served `invokeWithArguments` (lane L4's `Intrinsic`) to note the handle on the
   thread's record for the walk to ask. That is a cross-lane change to the served call, not a
   walk-side one.
2. **The customized adapter.** HotSpot customizes the handle that its linker's `checkCustomized`
   counts past 127 calls. The served call never runs that linker, so no count exists to read. A
   faithful answer needs the served call to bump `customizationCount` as `Invokers.maybeCustomize`
   would (lane L4), and then the walk reads `form.customized`. Not walk-side either.
3. **`--compatible`.** The carrier `asSpreader` remains L4's (`CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK`).
4. **The `-XX:+ShowHiddenFrames` throwable.** A capture may run no Java. Unchanged by design.

### Run

```text
cratonvm --jdk-only -cp . L3W121SpunLinkerFrames
CRATONVM_STANDIN_SPUN_LINKER_FRAMES=1 cratonvm --jdk-only [--nojit] -cp . L3W121SpunLinkerFrames
CRATONVM_DEBUG_STACKWALK=1 CRATONVM_STANDIN_SPUN_LINKER_FRAMES=1 cratonvm --jdk-only -cp . L3W121SpunLinkerFrames 2>&1 | grep spun-linker
(the same under --compatible; and with both CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 and the new switch: L3W115SpunIwaWalkFrames, L3W104WalkOptionsThroughHandles, L3W46InvokeFramesInWalks, L3W98StackWalkerOptions must not move outside the rows named)
```

`L3W46InvokeFramesInWalks`'s `mh.invokeExact()` hidden row (a static `target()V`) should turn SAME
with the switch on. That row is HotSpot's `LambdaForm$MH.invokeExact_MT(Object,Object)V@18`, the
`()V` linker.

## Wave 123 (interpreter round i6, lane L3): residuals 1-4 re-read

**OPEN, still blocked.** Nothing built. Residuals 1-4 were re-read against the current code: the
w120 route fix (`walk_frames_screened`) and the w121 `CRATONVM_STANDIN_SPUN_LINKER_FRAMES`. Neither
makes any of them reachable from the walk. Date 2026-10-19.

1. **A handle that is not direct.** Still no record of the handle on the thread. The served
   `invokeWithArguments` notes nothing the walk could read. Both walk routes see frames only. The
   w121 linker step acts at an `invokeExact` site, not at a served `invokeWithArguments`.
2. **The customized adapter.** Wave 117 (lane L4, `CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`) added a
   `customizationCount` bump. It is in `lang_invoke.rs`, on the latched direct lane of a
   `DirectMethodHandle` `invokeExact`. The served `invokeWithArguments` still never counts its
   adapter, so `form.customized` stays unset where HotSpot would have customized. The walk could
   read `form.customized`, but it would always read "not customized". The fix is still the
   served call's to make (lane L4).
3. **`--compatible`.** `CRATONVM_MH_FOREIGN_COLLECTORS_RUN_JDK` is still default off and kept off
   (`types/src/flags.rs`: its `BoundMethodHandle` over a spun form "the default world cannot
   run"). So `asSpreader` still answers a carrier with no spun form.
4. **The `-XX:+ShowHiddenFrames` throwable.** Unchanged by design: a capture runs no Java.

## Wave 132 (lane L4): residual 2, the served call now counts its adapter

**SWITCH ON (parity), page still OPEN.** Not built or run by the lane. Date 2026-10-19.

`CRATONVM_MH_IWA_SERVED_CUSTOMIZATION` is new. It is `NativeFlags::mh_iwa_served_customization`,
`on_unless_zero`: default ON, `=0` restores the old served call. Its `flag_groups` token is
`mh-iwa-served-customization`, and it has a `flag-surface` line.

### What HotSpot does (measured, 25.0.3, default flags and `-Xint` agree)

The JDK body runs `spreadInvoker(0).invokeExact(asType(genericMethodType(n)), args)`. The spread
invoker is the generic invoker's form (`LF_GEN_INVOKER`) edited to spread its array. Per call it
runs `checkGenericType(adapter, generic)`, then `Invokers.checkCustomized(adapter)`, then
`invokeBasic`. So the adapter is counted exactly once per call.

* The adapter is the `asTypeCache` entry, or the handle itself when its type already is
  `genericMethodType(n)`.
* After `k <= 127` calls its `customizationCount` is `k`. The 128th call customizes it, and the
  count then stays 127.
* A target that throws, and an adapter that raises `ClassCastException`, are counted.
* An `asType` that throws (wrong arity) is not counted.
* `-Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=3` customizes at the 4th call. `=-1` never
  counts: the invoker form then has no `checkCustomized`.
* **New finding.** A customized *direct* handle (a `genericMethodType(n)`-typed handle, whose
  adapter is itself) runs a spun `LambdaForm$DMH/0x..::invokeStatic`, not the pregenerated
  `DirectMethodHandle$Holder::invokeStatic`. The `walk` argument of the probe prints it:
  `walk customized [java.lang.invoke.LambdaForm$DMH/ADDR::invokeStatic, ...]`. So residual 2 also
  covers the holder stand-in this VM inserts for such a handle after 127 calls, not only the
  adapter's bci.

The served door counted nothing. Every count stayed 0, and `form.customized` stayed null.

### Built (`native-builtins/src/lang_invoke.rs`)

The change is in `mh_real_invoke_with_arguments_lanes`, the served door of a REAL handle, which is
used in both modes. Under the switch:

1. It asks for the adapter first: `mh_real_iwa_adapted`, the JDK's own `genericMethodType` and
   `asType`, memoised per epoch. Before this change, the direct lane (`mh_real_iwa_direct`) never
   asked for it.
2. It runs `iwa_served_check_customized` on the adapter:
   * default threshold: `real_direct_lane_customization`'s reads. A count below 127 is raised
     here, and the old value is kept. The due call runs the JDK's own `Invokers.maybeCustomize`
     (`customize()`);
   * `CUSTOMIZE_THRESHOLD < 0`: nothing;
   * any other threshold: the JDK's own `Invokers.checkCustomized` (Java on each call).
3. The lanes then run as before (`mh_real_iwa_served_lanes`, split out unchanged). They reuse the
   pinned adapter.
4. If every lane declines and the JDK body runs instead, the count is taken back
   (`iwa_served_uncount`), because that body counts the call itself.
5. If `customize()` throws, the call raises it. An `asType` that throws sets `type_threw` as
   before, and nothing is counted.

The body arm (`CRATONVM_MH_IWA_JDK_BODY`, native steps) is unchanged: it runs the real linker,
which counts by itself.

### What is still open for residual 2: the walk's side

The spun-IWA walk step (`lang_stackwalker::spun_iwa_entries`) still builds its own adapter from an
`IMPL_LOOKUP` handle of the frame's target. It never sees the user's handle, so it would read "not
customized" even now. Reading the real answer needs a cross-crate record of the served call's
adapter on the thread. That record is the one residual 1 also needs: a rooted reference on the
`invokeWithArguments` reflective mark (`enter_reflective_call`), readable by both walk routes.
Then:

* the adapter frame comes from `adapter.form.vmentry`, the customized class, at its
  `invokeBasic`;
* for a generic-typed direct handle, the holder stand-in is replaced by the customized
  `LambdaForm$DMH` entry.

That is L3's walk work plus a `native-api` / `vm_exec` record. It is not built here.

### Probe

`tools/probes/interp/L4/L4W132IwaCustomization.java`. It has a `HOTSPOT_EXPECTED` block, and
default flags and `-Xint` print the same. It reads `customizationCount` and `form.customized` by
reflection, so it needs `--add-opens java.base/java.lang.invoke=ALL-UNNAMED`. Its rows:

* the families `static-int`, `static-obj`, `virtual` and `list`, at `k = 0, 1, 2, 126, 127, 128,
  200`;
* `throws`, `badarg` and `arity`.

### What the orchestrator must run

```text
O="--add-opens java.base/java.lang.invoke=ALL-UNNAMED"
for mode in --jdk-only --compatible; do for jit in "" --nojit; do
  cratonvm $mode $jit $O -cp . L4W132IwaCustomization                                    # pass: == HOTSPOT_EXPECTED
  CRATONVM_MH_IWA_SERVED_CUSTOMIZATION=0 cratonvm $mode $jit $O -cp . L4W132IwaCustomization  # expected DIFF: all counts 0
  CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1 \
    cratonvm $mode $jit $O -cp . L4W132IwaCustomization                                  # pass: == HOTSPOT_EXPECTED
done; done
cratonvm --jdk-only --nojit $O -Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=3 -cp . L4W132IwaCustomization   # pass: the THRESHOLD_3 note
cratonvm --jdk-only --nojit $O -Djava.lang.invoke.MethodHandle.CUSTOMIZE_THRESHOLD=-1 -cp . L4W132IwaCustomization  # pass: every count 0
```

Regression, switch on against `=0`, `--jdk-only` and `--compatible`, `--nojit`. Each must print
the same in both arms:

* `L3W115SpunIwaWalkFrames`, plus the same with `CRATONVM_STANDIN_SPUN_IWA_FRAMES=1`;
* `L3W104WalkOptionsThroughHandles`, `L4W109IwaHiddenFrames`, `L4W109IwaReceiverCast`;
* `L4W84InvokeWithArgumentsJumbo`, `L4W95IwaVarargsSuper`, `L4W116IwaNativeSteps`;
* `L4W117DirectHiddenLatch`, `L4W130ExactPushAndLinkMemo`.

Cost check, P-cores, `--nojit`: `L4W109InvokeWithArgumentsBodyBench` base against `=0`, five
interleaved reps, medians. Expected: about +0.1-0.3 us on `static-int`, `static-ref` and
`virtual`. That is the extra adapter-memo probe, the pin and two or three field reads, and one
write until the 128th call. If any row is above 1.10x, report it before the switch stays on.
