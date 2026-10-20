# The constant-`VarHandle` prefix refused a get in a cast context and an RMW used as a statement

Status: OPEN -- fix built behind two opt-in switches (JIT round 21 wave 1, lane updaters, wave 3), pending
the orchestrator's build, probe run and A/B. The remaining gaps were built behind four more opt-in
switches in waves 4 and 22 (sections at the end).
Area: `jit/src/ir_lower.rs` (`vh_const_site_shape`, `vh_const_core`, the new `vh_const_core_cast_return` /
`vh_const_result_screen` / `vh_const_class_by_name`), `jit/src/x64/op_invoke.rs` (the single-pass
constant-handle prefix), `vm/src/jit/helpers.rs` (`jit_well_known_class_id`, the new
`WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG` answer).
Found by: lane indy's `tools/probes/jit/r21/R21CfChain.java`. Its `vh-get-control` row costs 293 ns on dev
and 425 ns on wave 1, against HotSpot's 0 ns. `vh-cas-control`, on the same handle, costs 8 ns against
HotSpot's 11.

## Cause

JLS 15.12.3 gives a signature-polymorphic call a call-site descriptor that depends on its context:

- **Cast context.** The cast type becomes the return type. `((Integer) RESULT.getAcquire(h))` compiles
  to `(LR21CfChain;)Ljava/lang/Integer;`.
- **Expression statement.** The return type is `V`. `COUNT.getAndAdd(this, 1);` compiles to
  `(LFoo;I)V`.

`vh_const_core` refused a reference result whose class is narrower than `Object` unless the descriptor
names the field's own class and the caller is the coordinate class. `vh_const_site_shape` refused every
RMW whose return is not the field kind (`Z` for a CAS). So these sites always kept their call, while the
same handle's `Object`-typed CAS inlined. Both refusals return the call's correct answer; only the cost
is wrong.

## The fix (opt-in)

### `CRATONVM_JIT_VH_CONST_CAST_RETURN=1`

Applies to a reference `get` / `getVolatile` / `getAcquire` / `getOpaque` that the shape rule refused
for its narrower return.

- The core is built for an `Object`-returning twin of the site.
- The invoker's cast becomes a screen on the loaded value (`VhConstCore::result_check`,
  `vh_const_result_screen` on RAX). Null passes. An instance of the return class passes, through its
  supers display, or by exact class id when it has none. An array return admits only null.
- Anything else declines to the call, which throws the `ClassCastException`. The access is a pure load,
  so a decline only re-reads.
- The return class id comes from a new VM query (`vh_const_class_by_name`). It answers only when the
  name resolved for the caller is the one loaded class of that name, never loads a class, and is a
  `try_read`.
- Both tiers use it, and so does the planner's `vh_const_site_served`.

### `CRATONVM_JIT_VH_CONST_VOID_RMW=1`

`vh_const_site_shape` admits CAS / compareAndExchange / getAndAdd / getAndSet / getAndBitwise sites with
a `V` return. The prefix writes no result. The array arm inherits the rule, because it reuses the field
recogniser.

Both are perf changes, so per the lane rules they are opt-in, with the A/B to decide.

## Remaining gaps (not fixed)

- **A reference RMW in a cast context** (`(String) VS.getAndSet(h, v)`, a cast `compareAndExchange`).
  The cast must be decided before the swap, so it cannot be a post-access screen. Proposal UPDATERS21-6.
- **A reference ARRAY element** (`arrayElementVarHandle(Object[].class)`) has no arm in either tier. It
  needs the barrier gates and the covariant store check (AT-2's reference half). Proposal UPDATERS21-5.
- **A discarded `get` (`VH.get(h);`)** is still refused. It is dead code; nothing to win.

## How to verify

Probe: `tools/probes/jit/r21/R21UpdaterRows.java`.

- **Facts.** Every fact, including the new `kernel vh-*` rows and `vh-cast-get-*`, must equal HotSpot
  25 in both modes, on every arm.
- **Arms.** Run default, `CRATONVM_JIT_VH_CONST_CAST_RETURN=1` and `CRATONVM_JIT_VH_CONST_VOID_RMW=1`.
  - The CAST_RETURN arm should move `time vh-ref-cast-get` and `vh-ref-getacquire`.
  - The VOID_RMW arm should move `vh-int-getandadd-void` and `vh-int-cas-void`.
  - Both are expected to reach the `vh-ref-object-get` / `vh-int-getandadd` level.
- **Cross-check.** `R21CfChain` `vh-get-control` under CAST_RETURN.
- **Engagement.** `CRATONVM_DBG_JITC=1` should show a `vh-const-inline (ir)` line for each of those sites.

Unit tests (`cargo test -p cratonvm-jit --lib r21_updaters_`):

- `r21_updaters_a_cast_return_screens_the_loaded_reference`: the executed bytes for instance, null,
  another class, and the null-only array cast.
- `r21_updaters_a_void_rmw_site_is_a_shape_only_under_its_switch`.

## Wave 4 (lane updaters): two of the remaining gaps built, opt-in

- `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET=1` (UPDATERS21-5): `Object[]` element GET modes. Probe row
  `vh-oarr-getacquire`; `CRATONVM_DBG_JITC=1` prints `vh-const-ref-array-inline`.
- `CRATONVM_JIT_VH_CONST_CAST_RMW=1` (UPDATERS21-6): a cast reference `getAndSet`, with the cast decided
  before the swap. Probe row `vh-ref-getandset`.
- Unit tests: `r21_updaters_reference_array_get_hits_in_bounds_and_screens_the_cast` and
  `r21_updaters_a_cast_get_and_set_decides_the_cast_before_the_swap`.
- One existing test was updated: `the_array_plan_word_is_not_a_field_plan_and_round_trips`. It pinned
  "no plan word for `L`", which is no longer true.
- Still not built: reference-array sets / CAS / getAndSet (the covariant store check) and a cast
  `compareAndExchange`. Both were built in wave 22 (next section).

## Wave 22 (lane vhconst): the last two gaps built, opt-in

Status after this wave: every gap in "Remaining gaps" is built behind an opt-in switch, except the
discarded `get` (nothing to win) and a cast result on an ARRAY RMW. All of it is pending the
orchestrator's build, probe run and A/B.

- **`CRATONVM_JIT_VH_CONST_CAST_CAE=1`** (UPDATERS21-6): `(String) VO.compareAndExchange(h, e, v)` on a
  reference field.
  - **One read.** The outcome and the cast are decided from ONE read `O` of the field
    (`ir_lower::vh_const_cast_compare_and_exchange_bytes`).
  - **`O == expected`.** Screen `O`, then `LOCK CMPXCHG` exactly `O`.
  - **`O != expected`.** A failed exchange at that read; screen `O` as the witness.
  - **Failure.** A failed cast on either path, or a lost race, declines with nothing written. The call
    then exchanges and throws the `ClassCastException`, as HotSpot.
  - **Where.** Both tiers, through `vh_const_core_cast_return` / `vh_const_core_bytes`.
- **`CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE=1`** (UPDATERS21-5): `Object[]`-handle set / setOpaque /
  setRelease / setVolatile / CAS / compareAndExchange / getAndSet, with a `V`, `Z` or `Object` result.
  - **Halves.** Two, with the tier's operand reload between them:
    `vh_const_ref_array_store_screen_bytes`, then `vh_const_ref_array_store_access_bytes`.
  - **What it screens.** The array (non-null, a reference array); the field arm's reference-store
    gates; the covariant store check against the array's OWN component id, as the `aastore` lowering
    proves it (null, `Object[]`, exact component, or the supers displays via
    `aastore_display_arm_bytes`); then `index <u length`.
  - **Exceptions.** Every refusal declines with nothing written, so the NPE / AIOOBE / ASE and their
    order are the JDK body's.
  - **Plan.** The VM files the `Object[]` plan under this switch too (`vm/src/jit/helpers.rs`
    `vh_const_array_handle_kind`).
  - **Code.** `Lowerer::try_emit_ir_vh_const_ref_array_store_prefix` and
    `Compiler::emit_sp_vh_const_ref_array_store_prefix`.
- **Engagement.** `CRATONVM_DBG_JITC=1` prints `vh-const-ref-array-store-inline (ir|sp)`; the cast CAE
  prints the existing `vh-const-inline` line.
- **Probe.** `tools/probes/jit/r21/R21VhConstRefRows.java`.
  - Facts: `cae-cast-*` (including `cae-cast-wrong-hit`: HotSpot exchanges, THEN throws, so the field
    reads `n1`); `oarr-*` / `sarr-*` / `narr-*` / `csarr-*` / `nested-*` (ASE `msg=null`); `*-oob` (AIOOBE
    `Index 8 out of bounds for length 8`); `*-both-bad` (the ASE for `set`, the AIOOBE for setVolatile
    and CAS, from the JDK body's evaluation order); `null-*` (NPE).
  - Timing: `vh-ref-cast-cae`, `vh-obj-cast-cae`, `vh-oarr-set` / `-setrelease` / `-setvolatile` /
    `-cas` / `-cae` / `-getandset`, `vh-sarr-set` (exact component), `vh-narr-set` (a subclass value,
    proven through the supers displays; `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM=0` makes it keep the call).
- **Unit tests** (`cargo test -p cratonvm-jit --lib r21_vhconst_`):
  - `r21_vhconst_a_cast_compare_and_exchange_screens_the_witness_before_any_write`;
  - `r21_vhconst_a_cast_compare_and_exchange_shape_is_a_reference_cae`;
  - `r21_vhconst_reference_array_store_shapes_follow_their_switch`;
  - `r21_vhconst_reference_array_store_checks_covariance_and_gates_before_writing`.
- **Inventories bumped.** `layout_const_inventory` `ir_lower.rs` `ARRAY_LENGTH_OFFSET` 15 -> 16, and the
  `flag_and_header_contracts` `ARRAY_LENGTH_OFFSET as i64` audit 8 -> 9 (the store arm's index guard,
  checked `disp8_const`).

## Wave 25: ArrayStoreException message

(wave 25, lane vhase; pending orchestrator build)

- **Symptom.** `R21VhConstRefRows` (wave-24 binary, both modes, cold AND warm): every non-`set` store of an
  `Integer` into a `String[]` through `arrayElementVarHandle(Object[].class)` printed
  `java.lang.ArrayStoreException msg=java.lang.Integer`; HotSpot 25.0.3 prints `msg=null`. Not a JIT
  issue: the inline prefixes (`CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`) decline every unproven store to the
  call, so cold and warm both reach the same `VarHandle` native.
- **HotSpot's behaviour (JDK 25 `src.zip`, `VarHandleReferences.Array`).** Two roads:
  - plain `set`: `array[index] = handle.componentType.cast(value)`, an `aastore`, so the refusal names the
    value's class, `java.lang.Integer` (cold row `sarr-set-integer` and `R15Vh7ArrayCastOrder` `aseIn`
    agree);
  - `setVolatile` / `setOpaque` / `setRelease`, `compareAndSet`, `compareAndExchange*`,
    `weakCompareAndSet*`, `getAndSet*`: `runtimeTypeCheck` -> (array class != handle's `arrayType`)
    `reflectiveTypeCheck`, which catches the `Class.cast` CCE and throws `new ArrayStoreException()`:
    null message.
- **Cause.** `native-builtins/src/lang_invoke.rs` `vh_array_store_refusal` (CAS / CAE / getAndSet arms) and
  `vh_array_refusal_after_index` (all four `set*` modes) built every refusal with the `aastore` wording
  (`entry_value_type_display`).
- **Fix (default ON, both modes): `CRATONVM_VH_ARRAY_RUNTIME_TYPE_CHECK_NULL_MESSAGE`.** The refusal now
  goes through `vh_array_store_check_failure(.., aastore)`: `aastore` = the mode is plain `set` (class
  name kept); every other mode builds `ArrayStoreException` with an empty message, which
  `RuntimeError::as_java_throwable` (`types/src/error.rs`) now maps to a null `getMessage()` as it
  already did for `ClassCastException`. `=0` restores the class name on every mode. Plain `aastore`, the
  `arrayElementSetter` `MethodHandle` and `System.arraycopy` messages are untouched.
- **Not fixed: the warm `set` rows.** On HotSpot the warm `sarr-set-integer`, `narr-set-string`,
  `csarr-set-array-value`, `nested-set-object-array`, `oarr-set-oob`, `oarr-set-negative` and
  `sarr-set-both-bad` print `msg=null` because C2 inlined the JDK `set` body and its hot implicit
  `aastore` / bounds exceptions became OmitStackTraceInFastThrow throws. CratonVM's `VarHandle.set` is a
  native, so no compiled site exists for the fast-throw profile (`vm/src/runtime/exceptions.rs`
  `FastThrowKind`) to judge; those rows keep the cold text. Modelling it would need the native's
  ASE / AIOOBE to count as an implicit exception of the compiled caller's call site.
- **Unit tests.** `cratonvm-types` `an_empty_array_store_message_is_no_message`;
  `cratonvm-native-builtins` `a_runtime_type_check_array_store_refusal_has_no_message`.
- **Verify.** `R21VhConstRefRows` default, `--compatible` and `--nojit`: the 12 non-`set`
  `sarr-*-integer` cold/warm rows equal the reference; with the switch `=0` they print
  `msg=java.lang.Integer` again (control).
