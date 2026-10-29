# Proposal: converting and bound-static REAL handles in the constant bind and the compile doors (MH6-1 / MH7-4 remainder)

> **j27 wave 8 (lane W8CLI, 2026-10-28): REVIEWED; one soundness hole FIXED IN CODE, PENDING HOST VERIFICATION.**
>
> **Item 2 (MHASTYPE) against JDK 25.** Checked against `MethodHandleImpl.valueConversion` and `ValueConversions.unboxWiden` / `unboxExact` / `primitiveConversion`; it matches:
> * The admission matches the `strict`, non-`monobox` `asType` rules.
> * `DirectMethodHandle` is matched exactly, so `$Special`, `$Interface` and `$Constructor` are refused.
> * A null-conversions-only `asType` yields a `viewAsType` copy of the `DirectMethodHandle`, not a species, so it is refused.
> * A later `asType(T2)` that replaces `asTypeCache` cannot make an existing bind wrong: the handle's behaviour does not change.
>
> **Fixed** (`native-builtins/src/lang_invoke/astype_bind.rs`):
> * The hole: `mh_real_resolve_wrappers` clears the bit of each unloaded wrapper. A position whose whole mask was cleared, such as a `Long`-typed parameter with `java/lang/Long` not loaded, then read as mask `0`, which means "pass the word as is". The member would have received the REFERENCE word as its `long`.
> * The fix: the new pure `mh_real_unbox_positions_survive(planned, resolved)` refuses such a handle in `mh_real_converting_compiled_bind`.
> * The case is unreachable on a normal boot, where every wrapper is loaded, so the fix needs no switch and no probe.
> * Test: `cratonvm-native-builtins` `lang_invoke::astype_bind::j27_mh_real_astype_plan_tests::j27_w8cli_an_unbox_position_emptied_by_unloaded_wrappers_refuses`.
>
> **Item 1 (T1LAM2, kind 6).** Read `jit/src/ir_mh_bound_static.rs` `bound_site_build` and `jit_bridge.rs` `mh_constant_bound_kind_admitted` / `mh_constant_bound_static_site_matches`: no defect.
> * The plan's kind and the row's kind must agree.
> * `argL1` is loaded from the handle at run time, so there is no embedded pointer.
> * The guard-free `invokestatic` is right for `Species_LL` (`argL0` = the direct handle, `argL1` = the bound value).
>
> **Verify:** both lanes' recipes below, unchanged, plus `cargo test -j 5 -p cratonvm-native-builtins --lib j27_mh_real_astype_plan_tests`.

> **j27 wave 6 (lane T1LAM2, 2026-10-28): item 1 FIXED IN CODE behind an opt-in switch, PENDING HOST VERIFICATION.** Not built or run here. Item 2 is MHASTYPE's (below); item 3 is left as written.
>
> **What changed: a new compile row kind `6` ("bound static").**
> * **Fill** (`vm/src/jit/helpers/dispatch.rs` `mh_constant_bind_fill`):
>   * A new block after the MHBIND21-1 one records the row. It fires for a real handle the bind admitted with a leaf and the compile memo on, with no conversion of either kind, that is bound, and whose member is static (`invoke_kind 3`, the MH16-7 bind). The pure gate is `mh_bound_static_compile_row_wanted`.
>   * The row carries the species' `argL1` slot (`mh_real_bound_compile_row`, unchanged) and is written through the new `jit_bridge::note_mh_constant_target_bound_static`. That writer stores `invoke_kind 6`, `bound = (slot, 0)`.
>   * The kind-3 compile-row block's `!t.bound` gate is unchanged, so this handle is never recorded as a plain kind 3, which would drop the bound argument.
> * **Door** (`vm/src/runtime/interpreter/jit_bridge.rs`):
>   * `CpResolvers::mh_constant` gains a `6 =>` member arm. The member must be static; its descriptor must be the site's with one leading `L..;` in front (`mh_constant_bound_static_site_matches`, which reuses the jit crate's now-`pub` `mh_constant_virtual_site_matches` with its arguments swapped); and every class it names must be one class from both loaders. The access rules are the existing ones.
>   * `CpResolvers::mh_constant_bound` hands kind-6 rows out too, through the pure `mh_constant_bound_kind_admitted(kind, bound_on, bound_static_on)`.
>   * `mh_constant_kind_word(6)` is `"bound-static"`.
> * **Planner** (`jit/src/lib.rs`):
>   * `ir_mh_constant_plan` admits kind 6 under the static gate when the member is the site plus a leading reference. No guard-trap term is needed: there is no guard.
>   * `IrMhConstantPlan::with_bound_rows` drops a kind-6 site that has no row, as it does for kind 4.
>   * `rewritten_invoke_ops` makes it an `invokestatic` row (`0xb8`).
>   * The new `bound_static_pcs` feeds `IrBuilder::set_mh_constant_bound_static_sites`.
>   * `bound_receiver_class_at` answers kind 4 only, so a static site never seeds a guarded splice's class.
> * **Builder** (`jit/src/ir.rs` `IrBuilder::mh_constant_bound_opcode`, new field `mh_constant_bound_static_sites`):
>   * The new pure decision `jit/src/ir_mh_bound_static.rs` `bound_site_build(invoke_kind, planned_static, num_args, interface)` picks `(0xb8, no guard)` for a static row on a site planned static, and `(0xb6, guard)` for an instance row on a site planned virtual. Any other pairing refuses the method.
>   * The handle's `LoadStatic` is replaced by `Load(Ref)` of `argL1` exactly as in kind 4.
>
> **HotSpot semantics.**
> * **`WrongMethodTypeException`:** only a site at the bound handle's own type is admitted, both by MH16-7's `mh_real_bound_member_kind` and by the door's site check. Any other `invokeExact` type keeps the handle's door and its WMTE.
> * **Null checks:**
>   * The handle is a non-null `static final` of an initialised class; a null field is never planned.
>   * The bound value was a non-null instance of the leading parameter's class at admission (`mh_real_bound_leading_value_ok`). It is `final`, and the handle's identity is guarded by the address/epoch, field-row witness and retire-on-rewrite rules.
>   * A static callee takes any value of its parameter type anyway, so no guard is planted.
> * **Class initialisation:** the member's class is initialised by the `invokestatic` arm's own rule, as the direct handle's `ensureInitialized` form does. By the time a compile row exists the handle has run once, so the class is already initialised.
> * **Exceptions:** an exception thrown by the member propagates from the `invokeExact` with the member's frame on top, as on HotSpot, where the LambdaForm frames are hidden.
>
> **Condy:** lane T1L2's condy path (`mh_condy_answer`) is untouched; a condy receiver is never planned bound.
>
> **Switch:** `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC`, perf, **opt-in** (`=1`). It is read on the cold fill and at compile time only. It is opt-in because MH16-7 (`CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC`, the helper bind this rides on) is not host-verified, and this route was never run.
>
> **Tests:**
> * `cratonvm-jit`:
>   * `ir_mh_bound_static::tests::*` (the build decision);
>   * `j27_t1lam2_bound_static_plan_tests::{a_bound_static_site_is_an_invokestatic_with_its_row, a_bound_static_site_without_its_row_or_gate_is_dropped, a_bound_virtual_site_is_not_a_static_one}` (end of `lib.rs`).
> * `cratonvm-vm`:
>   * `runtime::interpreter::jit_bridge::j27_t1lam2_mh_bound_static_door_tests::*`;
>   * `jit::helpers::j27_t1lam2_bound_static_compile_row_tests::*`.
>
> **Probe** `tools/probes/jit/j27/J27MhRows.java` has new rows. The expected HotSpot 25 lines are derived from JDK 25 and also listed in the probe header:
> ```
> fact bound-static-clinit false true 1 java.lang.String
> fact bound-static-live-sum 51381728 same=true
> fact bound-static-throw-hot 12728964100000 same=true
> fact bound-static-lazy-sum 2400000 same=true
> fact bound-static-box-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found (int)long
> fact bound-static-box-type (int)int java.lang.String
> ```
>
> **Verify:**
> 1. Run `J27MhRows` in `--jdk-only` and `--compatible`, x {unset, `CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC=1`}. Every `fact` must equal HotSpot 25's, including the existing `bound-static-*` lines.
> 2. With `=1` and `CRATONVM_DBG_JIT_MH_BIND=1`, expect:
>    * `[mh-bind] compile row ... bound-static, argL1 slot N` for `BL`, `BS`, `BC`, `BZ`, `BSTR`, `BREAD`, `BBOOMBOX` and `BLAZY`;
>    * `[mh-bind] compile ... -> p.addL(Ljava/lang/Long;I)J bound-static`;
>    * under `CRATONVM_DBG_IR_COMPILES=1`, `[ir] mh CONSTANT BOUND at bytecode pc .. -- built as opcode 0xb8`.
>    * None of these for `BNULL` (a null bound value is refused by MH16-7), nor for `bound-static-pair` / `bind-each` (no `static final` receiver).
> 3. Timing: `-Dj27.time=true`, unset vs `=1`, interleaved, medians. `bound-static-long-sum`, `bound-static-live-sum` and `bound-static-lazy-sum` should drop to roughly `exact-static-sum`'s cost.
> 4. Flip rule: default ON only after MH16-7 is host-verified, the facts are equal in both modes, the timing drops, and the battery is clean.

> **j27 wave 5 (lane MHASTYPE, 2026-10-28): PARTLY (item 2, helper half, OPT-IN), PENDING HOST
> VERIFICATION.** Not built or run here.
>
> **Item 2 done in the dispatch helper.** New module
> `native-builtins/src/lang_invoke/astype_bind.rs` holds the admission
> `mh_real_converting_compiled_bind`. It does not recognise the species' forms. It proves
> provenance instead: the handle must be a `BoundMethodHandle$Species_*` whose `argL0` is a
> plain `DirectMethodHandle` that names the handle as its `asTypeCache` (or as the referent of
> its `asTypeSoftCache`). Such a handle is `argL0.asType(type)`, which is
> `makePairwiseConvert(argL0, type, strict=true)`, so its behaviour is the documented
> conversion. The handle must be called at its own type. The direct handle must be a
> `REF_invokeStatic` or `REF_invokeVirtual` member at its own type, and the member rules are
> the exact bind's.
>
> Conversions served, per position (pure `mh_real_conversion_plan`):
> - the same token, or a reference into `Object`, passes;
> - a reference into `long` unboxes Long, Integer, Short, Byte or Character;
> - a reference into `int` unboxes Integer, Short, Byte or Character;
> - a source type that IS the wrapper takes only that wrapper (`unboxExact`);
> - an `int` or `long` return is boxed into its wrapper, `Number`, `Comparable`, `Serializable`
>   or `Object`;
> - anything else refuses the handle: short, char, boolean, float or double unboxes, argument
>   boxes, casts, primitive conversions, `void` drops.
>
> In `vm/src/jit/helpers/dispatch.rs`:
> - `mh_constant_bind_fill` asks the admission last, for a real handle only;
> - `MhRealConv` / `mh_real_conv_of` resolve each wrapper's `value` slot;
> - the warm path's `mh_real_convert_args` / `mh_real_unbox_word` / `mh_real_widen` replace each
>   marked word. Any other value goes to the door, which runs the JDK adapter and so throws its
>   CCE text, its NPE on null and its WMTE. The unboxers are pure, so declining skips no side
>   effect;
> - `mh_constant_bind_call` boxes a `long` result through `jit_long_value_of_direct_body`, so
>   the `Long` cache identity holds. A `Long.MIN_VALUE` result is boxed. The `i64::MIN` word is
>   treated as the sentinel only when an exception is pending;
> - `Invokers.checkCustomized` is counted as on every real bind.
>
> A converting real handle is kept out of the compile rows: the new `real_conv.is_none()` gate
> on `mh_constant_records_kind`. Without it the compile door would call the member
> unconverted.
>
> Switch `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING`: perf, **opt-in** (`=1`). It is opt-in
> because the `asTypeCache` read and the long-box path were never run here. It is read on the
> cold fill only.
>
> **Item 1 (bound static at compile time) is NOT done.** It needs the jit_bridge
> `CpResolvers::mh_constant` door, the `jit/src/lib.rs` planner (lane X2LCAP) and
> `jit/src/ir.rs`. None of these are this lane's files, and MH16-7 is not host-verified yet.
> **Item 3:** left, as written.
>
> **Verify.** Run `tools/probes/jit/j27/J27MhRows.java` in `--jdk-only` and `--compatible`,
> unset and `=1`; every `fact` line must equal HotSpot 25's. The new lines, derived by reading
> JDK 25; run HotSpot first and treat its output as the reference:
>
> ```
> fact conv-faults 12657328200100 same=true
> fact conv-identity 50016049984 same=true
> fact conv-min-value-count 50000 same=true
> fact conv-number-sum 12696928 same=true
> fact conv-virtual-sum 6648464 same=true
> fact conv-number-Short -8 java.lang.Integer
> fact conv-number-Long java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
> fact conv-number-Double java.lang.ClassCastException: Cannot cast java.lang.Double to java.lang.Integer
> fact conv-number-null java.lang.NullPointerException
> fact conv-virtual-Character 68 java.lang.Integer
> fact conv-virtual-Long java.lang.ClassCastException: Cannot cast java.lang.Long to java.lang.Integer
> fact conv-virtual-null-receiver java.lang.NullPointerException
> fact conv-min-value -9223372036854775808 java.lang.Long
> fact conv-box-cached true java.lang.Boolean
> fact conv-box-fresh false java.lang.Boolean
> fact conv-number-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (Number)Integer but found (Number)Object
> ```
>
> Then, with `=1` and `CRATONVM_DBG_JIT_MH_BIND=1`:
> - `[mh-bind] ... real-converting unbox=[..] box=..` lines must appear for the `TJ`, `TI_NUM`
>   and `TPLUS` sites;
> - none may appear for `TS`, `TC` or `TZ`.
>
> No `real-converting` line at all means the `asTypeCache` provenance read does not resolve on
> this build. That is a refusal, not a wrong bind.
>
> Timing: `-Dj27.time=true`, unset vs `=1`, interleaved, medians. `conv-mixed-sum`,
> `conv-number-sum` and `conv-virtual-sum` must drop with `=1`. Flip the default only after
> that and a clean battery.
>
> Unit tests:
> - `cratonvm-native-builtins`: `lang_invoke::astype_bind::j27_mh_real_astype_plan_tests::*`;
> - `cratonvm-vm`: `jit::helpers::j27_mh_real_converting_tests::*` (plus the unchanged
>   `a_converting_call_unboxes_only_its_marked_words` in `table.rs`).

Status: PROPOSAL (not built)
Area: `native-builtins/src/lang_invoke/adapters.rs` (admission), `vm/src/jit/helpers/dispatch.rs` (`mh_constant_bind_fill`, the converting route), `vm/src/runtime/interpreter/jit_bridge.rs` (`CpResolvers::mh_constant`), `jit/src/lib.rs` `ir_mh_constant_plan`, `jit/src/ir.rs` `IrBuilder::mh_constant_opcode`
Found by: JIT round 27 wave 4, lane MHROWS (re-check of MH6-1, MH7-4, MH16-7 of `j27-r18-backlog-live-rows-20261028.md`)

## Evidence

* The converting bind (MHB-1, `CRATONVM_JIT_MH_CONSTANT_BIND_CONVERTING`, `mh_compiled_bind_converting_target`)
  admits only a CARRIER (`java/lang/invoke/MethodHandle` with `MH_KIND`), and `mh_constant_bind_fill` asks it
  only `if !real`. Under the default-ON `jdk-invoke` row every handle is a real JDK handle, so in both default
  modes an `asType` result (`twiceJ.asType((Object)Object)`, `J27MhRows` `conv-*`) is never bound: each
  compiled call walks the signature-polymorphic link chain (linker, the species' spun `LambdaForm$MH`, the
  direct handle, `linkToStatic`). MH6-1's "unbox more than Integer" was a carrier-only question.
* The compile doors (`mh_constant_records_kind`, `invoke_kind 4` / `5`) record no bound STATIC handle and no
  converting handle. j27 wave 4 bound the static case in the helper only (MH16-7,
  `CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC`); the fill keeps it out of the compile rows
  (`!t.bound`, `t.invoke_kind == 0`), which is load-bearing: recorded as a plain kind 3 the compile door
  would call the member WITHOUT its bound first argument.

## What would fix it

1. **Bound static at compile time** (small, after MH16-7 is host-verified): a new compile row kind
   (say `6`, "bound static") recorded by `mh_constant_bind_fill` when `real && t.bound && t.invoke_kind == 3`,
   with the `argL1` slot (`mh_real_bound_compile_row`, unchanged). `IrBuilder` replaces the handle's
   `LoadStatic` by `Load(Ref)` of `argL1` exactly as kind 4 does, but builds the `0xb8` arm (no null guard:
   the admission refused a null `argL1`, and a static callee takes any value of its parameter type).
2. **Real converting handle** (larger): admit a real `asType` result whose `asTypeCache` chain is one
   `MethodHandleImpl.makePairwiseConvert` hop over a plain `DirectMethodHandle` (`findStatic` / unbound
   `findVirtual` at its own type), and whose per-position conversions are exactly what
   `ValueConversions.unboxWiden(W)` / `box(W)` do: unbox an argument of an exact wrapper class `X` with
   `W.isConvertibleFrom(X)` (Long, Integer, Short, Byte, Character into `long`; Short, Byte into `short`;
   Character into `char`; Boolean into `boolean`), send every other value (`null`, a non-convertible wrapper,
   anything else) to the door, which raises HotSpot's `ClassCastException` / `NullPointerException`; box the
   result through the wrapper's `valueOf`. That generalises `MhBindConv` from one `unbox_int` bit mask to a
   per-word `(wrapper class id, value slot, widening)` and `box_int_return` to a return wrapper. Recognising
   the species shape needs the JDK-private field reads `mh_real_is_bind_to_form` already makes (risk:
   layout drift answers `false`, never a wrong bind).
3. The carrier's converting bind (MH6-1 as written) only matters under `-jdk-invoke`; leave it.

Measure first: `J27MhRows -Dj27.time=true` `conv-mixed-sum` (ns/op) against HotSpot 25, both modes. Build 2
only if the row is above ~200 ns/op and appears in an application census (`CRATONVM_DBG_MH_LINK_CENSUS=1`
on the Spring Boot sample: count of compiled links whose handle is an `asType` species).
