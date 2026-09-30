# JIT round 14 lane ffm7 proposals (IndirectVarHandle follow-ups)

Ranked. Context: round 14 wave 7 served `IndirectVarHandle` accesses through the
composed mode handle (`lang_invoke.rs` `vh_indirect_serve`, FFM6-1).

## F7-1 Typed invocation of the composed mode handle (drop the per-access Object[])

- Benefit: every access through a combinator today allocates an `Object[]`, boxes each
  primitive coordinate/value and runs the `invokeWithArguments` door (generic-type
  `asType` per argument), then the target's native boxes its result again. A
  `insertCoordinates(x, 0, h).get()` loop is several allocations per access where
  HotSpot inlines to a field load. Typed dispatch removes the array and the argument
  boxes, and makes the call-site conversions exact (closes item 2 of
  `r14w7-ffm7-indirect-var-handle-residuals-20260929.md`).
- Cost: `vm_exec` arms the full call-site descriptor for a VarHandle receiver whose
  class is `IndirectVarHandle` (one class-name compare on that path only); the native
  calls the `invoke` door body with that descriptor (`VarHandle` prepended) and the raw
  arguments.
- Risk: medium (touches the interpreter-round `vm_exec.rs` dispatch block; the door's
  return conversion must match the VarHandle natives' boxed-return contract).
- First step: count accesses served (`vh_indirect_serve` hits) per run under
  `CRATONVM_DBG_*` to size the win on an FFM-heavy workload before touching `vm_exec`.

## F7-2 Retire the `arrayElementVarHandle` Bridge

- Benefit: `docs/jdk-only/ffm-array-element-var-handle-bridge-20260929.md` records 26
  Bridge rows that exist only because the JDK's
  `AbstractLayout.arrayElementVarHandle` produced an `IndirectVarHandle` nobody served.
  With FFM6-1 the JDK road should work; retiring the rows removes a class of
  hand-maintained layout arithmetic (scale, index bound, enclosing check).
- Cost: run `R14Ffm6ArrayElementVarHandle` under
  `CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE=0` (JDK road, now served) against HotSpot; if
  green in every arm, delete the registration and its switch.
- Risk: medium: the JDK road goes `collectCoordinates(varHandle(path), 1, scaleHandle())`
  through `invokeWithArguments` per access, so it is slower than the Bridge until F7-1
  lands. Do F7-1 first or accept the regression knowingly.
- First step: the probe run above (orchestrator; no code).

## F7-3 Recognise `insertCoordinates` / `dropCoordinates` over a field handle at mint

- Benefit: the two most common combinators over a plain field handle
  (`insertCoordinates(fieldVh, 0, receiver)` = a bound field; `dropCoordinates`) need no
  adapter at all: a side-table row with a bound receiver (or an ignored coordinate)
  answers them on the existing fast paths, including the JIT's
  `varhandle_instance_field_plan_for_handle`.
- Cost: a native for `VarHandles.insertCoordinates` that, when the target has a
  side-table row of kind INSTANCE and `pos == 0`, mints a row with the bound receiver
  pinned as a GC-visible field; everything else falls back to the JDK bytecode.
- Risk: medium-high: a new VarHandle shape and a new registration (explicit
  `NativeKind::Intrinsic` review, jdk-only rules); identity/`toString` of the handle
  differ from HotSpot's.
- First step: measure how often `insertCoordinates` over a field handle occurs in the
  real-workload corpus before designing the row.
