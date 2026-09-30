# JIT round 14 wave 7, lane review7: proposals

Ranked. Each is behind a new default-on kill switch when it lands.

## RV7-1. Layout VarHandles with several open `sequenceElement()` indices

- **Now:** `p67_var_handle_for_path` refuses `open_strides.len() > 1`, and
  `arrayElementVarHandle` refuses any path that opens one
  (`UnsupportedOperationException`, `foreign_ffm.rs`). Matrices
  (`sequenceLayout(R, sequenceLayout(C, JAVA_INT)).varHandle(sequenceElement(),
  sequenceElement())`) and arrays of structs with an inner array are common in
  FFM code and fail outright.
- **Benefit:** a whole class of FFM programs stops throwing.
- **Cost:** `SegmentVhShape` is `Copy` and keyed per handle; add a small fixed
  array of `(stride, bound)` (say 4) plus a count, extend
  `layout_vh_coordinates`, `layout_vh_index_refusal` (one `checkIndex` per
  level, outermost first) and `layout_vh_locate` (sum of scaled indices; value
  slot `2 + 1 + n`).
- **Risk:** low-medium (coordinate/value slot arithmetic in every access mode).
- **First step:** a unit test of the shape for a 2x3 `int` matrix in
  `r12w7_ffm4_segment_vh_tests`, then the shape field.

## RV7-2. A census row for SS8-1 a builds refused at an unproven held pc

- **Now:** `IrBuilder::begin_splice` returns `None` for a held pc whose
  receiver is not proven the window's monitor, refusing the whole build; the
  rebuild drops the enclosing synchronized site. Nothing counts how often.
- **Benefit:** tells whether proving through settled φs (as S5-2 does on the
  finished graph) is worth doing in the builder.
- **Cost:** one `SYNC_SPLICE_CENSUS_ROWS` entry and one increment.
- **Risk:** none (census only; needs a production reader: the existing census
  line prints every row).
- **First step:** add `nested-same-receiver-refused` next to
  `nested-same-receiver` in `jit/src/ir.rs`.

## RV7-3. Answer direct calls to OTHER compiled methods from their entry plan

- **Now:** CE-1 answers only a direct SELF-call, because only this body's own
  entry fast return / fold is known at lowering.
- **Idea:** publish the callee's `EntryFastReturn` + fold table on its
  `CompiledMethod`; a direct cross call (`emit_direct_cross_call`) to a callee
  whose plan is published and whose code cannot be replaced without
  invalidating the caller (the same dependency the direct bind already holds)
  answers in the caller like CE-1.
- **Benefit:** recursive helper pairs (`even/odd`, `ackermann` leaves, tree
  walkers with a base-case helper) skip the CALL/RET on their leaves.
- **Cost/Risk:** medium: the not-entrant patch at the callee's offset 0 is
  no longer consulted, so the caller must be invalidated with the callee
  (today's direct-bind dependency must be checked to cover redefinition too).
- **First step:** measure how many direct cross calls target a callee with an
  entry fast return (`CRATONVM_DBG_IR_SELF_CALL_ANSWER`-style census line in
  `emit_direct_cross_call`).
