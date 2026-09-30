# `MemoryLayout.arrayElementVarHandle` is a Bridge over JDK bytecode (removal record)

Status: OPEN (standing compatibility Bridge; remove when the condition below holds)
Area: FFM layout VarHandles (`native-builtins/src/phases_late/foreign_ffm.rs`)
Kind: `NativeKind::Bridge`, registered inside `register_p67_foreign_memory`'s explicit
`r.set_category(NativeKind::Bridge)` scope
Added by: round 14 wave 6 lane ffm6 (`r14w5-fixup2-layout-array-element-var-handle-patch-FIXED-20260929.md`)
Kill switch: `CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE` (default on; `0` runs the JDK bytecode again)

## What stands in front of real bytecode

`p67_memory_layout_array_element_var_handle`, registered for
`arrayElementVarHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle;`
on 26 receiver classes: the nine `java/lang/foreign/ValueLayout$Of*` / `AddressLayout`
interfaces, the nine `jdk/internal/foreign/layout/ValueLayouts$Of*Impl` classes,
`java/lang/foreign/MemoryLayout`, `SequenceLayout`, `SequenceLayoutImpl`, and the five
struct/group/union classes. The concrete JDK 25 body it shadows is
`jdk.internal.foreign.layout.AbstractLayout.arrayElementVarHandle`:

    return MethodHandles.collectCoordinates(varHandle(elements), 1, scaleHandle());

## Why it exists

That body returns a `java.lang.invoke.IndirectVarHandle`. Every `VarHandle` access mode
is answered by `native-builtins/src/lang_invoke.rs` (`varhandle_get_mode`,
`varhandle_set_mode`, the RMW natives), which serve CratonVM's side-table handles, the
FFM layout shape table and the real `SegmentVarHandle` -- not an `IndirectVarHandle`. It
fell through to the field-handle slot decode (slot 0 is the real `vform` reference, not an
`Int` kind), so `get` answered `null` and `set` returned without storing, for every carrier
and every mode. The Bridge mints a layout handle whose one index coordinate is scaled by
the layout's `byteSize()` and checked like `AbstractLayout.scale`
(`SegmentVhShape::scaled_index`).

## What would have to exist for it to go

The `VarHandle` access natives serving an `IndirectVarHandle` the way the JDK does:
`handleFactory.apply(mode, <target's mode invoker>)` (the target's invoker takes the
target `VarHandle` first), invoked with `(target.asDirect(), coordinates..., values...)`,
cached per mode (the real `VarHandle.methodHandleTable` field is there for it). Design and
risks: `docs/internal/fixed-bugs/r14w6-ffm6-indirect-var-handle-accesses-dropped-FIXED-20260929.md`.
Once that lands AND a probe shows `arrayElementVarHandle` get/set/CAS answering through it
at an acceptable cost (the Bridge is one table lookup per access; the indirect road is at
least one adapter dispatch), delete the 26 rows and the function.

## Confirm it is still needed

`rg -n '"arrayElementVarHandle"' native-builtins/src/phases_late/foreign_ffm.rs` lists the
rows; `rg -n 'IndirectVarHandle' native-builtins/src/lang_invoke.rs` shows whether the access
natives learned the indirect road.
