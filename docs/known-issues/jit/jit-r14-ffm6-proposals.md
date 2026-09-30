# JIT round 14 wave 6, lane ffm6: proposals (FFM layout VarHandles)

Ranked. None of these is implemented; each names its first concrete step.

## FFM6-1 Serve `IndirectVarHandle` through its `handleFactory` (retire the arrayElementVarHandle Bridge)

- Benefit: fixes the silent-drop defect for every VarHandle combinator (`filterValue`,
  `filterCoordinates`, `insertCoordinates`, `collectCoordinates`, `dropCoordinates`,
  `permuteCoordinates`) and lets `docs/jdk-only/ffm-array-element-var-handle-bridge-20260929.md`
  retire 26 Bridge rows. Defect page:
  `r14w6-ffm6-indirect-var-handle-accesses-dropped-FIXED-20260929.md`.
- Cost: one helper in `lang_invoke.rs` (~150 lines) plus a per-handle cache in the real
  `VarHandle.methodHandleTable`; a unit test needs a mock lambda for `handleFactory`.
- Risk: medium. It runs JDK lambdas and `collectArguments` adapters from inside an access
  native; the boxing contract between the outer call site, the adapter and the target's
  native must be pinned by a probe before the Bridge goes.
- First step: implement the helper for `get`/`set` only, behind `CRATONVM_VH_INDIRECT_SERVE`,
  and run the page's `filterValue` / `insertCoordinates` probe.

## Round 14 wave 7 (lane ffm7): FFM6-1 landed

All 31 access modes are served (`lang_invoke.rs` `vh_indirect_serve`, first step of the
seven access natives), mode handle cached in `VarHandle.methodHandleTable`, invoked
through `invokeWithArguments` with `directTarget` leading; unservable accesses throw
UOE. Switches `CRATONVM_VH_INDIRECT_SERVE` / `CRATONVM_VH_INDIRECT_REFUSE` (default on).
`varType`/`coordinateTypes` of an indirect handle answer too. NOT done: retiring the 26
`arrayElementVarHandle` Bridge rows -- that needs the probe
`R14Ffm6ArrayElementVarHandle` green under `CRATONVM_FFM_ARRAY_ELEMENT_VAR_HANDLE=0`
first (see `jit-r14-ffm7-proposals.md` F7-2).

## FFM6-2 Multi-index layout handles

- Benefit: `seq2d.varHandle(sequenceElement(), sequenceElement())` and
  `seq.arrayElementVarHandle(sequenceElement())` are refused today
  (`UnsupportedOperationException`, "one open sequence index"); matrix code over FFM uses
  exactly these.
- Cost: `SegmentVhShape` carries `[stride; 4]` + `[bound; 4]` + a count instead of one
  stride (it is `Copy` and stored per handle; 64 more bytes per row); `layout_vh_coordinates`,
  `layout_vh_locate`, `layout_vh_index_refusal` and the value-index arithmetic
  (`if shape.stride > 0 { 4 } else { 3 }`, three sites) generalise to `3 + n`.
- Risk: low-medium: the value index moves, and every access mode reads it.
- First step: replace the three `stride > 0` value-index sites with one
  `fn layout_vh_value_index(shape) -> usize` (pure refactor), then widen the shape.

## FFM6-3 Memoise the layout-handle mint per (layout, path)

- Benefit: JDK 25 caches `ValueLayout.varHandle()` in `AbstractValueLayout.handle`; CratonVM
  mints a new synthetic `VarHandle` and a new shape-table row on every `varHandle()` /
  `arrayElementVarHandle()` call, so code that calls them per operation (common in
  generated bindings) allocates and grows `P67_MEMORY_SEGMENT_VH_TABLE` per call.
- Cost: for the no-path case, store the minted handle in the real layout's `handle` field
  (it exists on `AbstractValueLayout`) and return it when set.
- Risk: low; identity of `JAVA_INT.varHandle()` across calls becomes HotSpot's (same object).
- First step: `p67_var_handle_for_layout` reads/writes `handle` by name when
  `resolve_field_index_by_class_id(class, "handle")` answers.

## FFM6-4 Serve `MemoryLayout.scaleHandle()` / `byteOffsetHandle()` / `sliceHandle()` natively-free

- Benefit: these return JDK method handles over `AbstractLayout.scale` etc.; they work only
  if `MhUtil.findVirtual` on the `MemoryLayout` interface and `bindTo` behave for our
  carriers. Not measured; a probe would tell whether they are another silent family.
- Cost: a probe first (`scaleHandle().invokeExact(layout?)`...), then either nothing or a
  targeted fix in the MH layer.
- Risk: none for the probe.
- First step: add a `scaleHandle`/`byteOffsetHandle` road to a round-15 probe.
