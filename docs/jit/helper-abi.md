# JIT helper ABI contract

Scope: `jit-api/src/lib.rs` (the `JitRuntimeHelpers` struct and the
`helper_fields!` list) and `jit-api/src/helpers_abi.rs` (the typed ABI
description). Producer: `vm/src/jit/helpers.rs::build_helpers`. Consumers: the
single-pass x64 emitter (`jit/src/x64.rs`, `jit/src/x64/*`), the IR lowerer
(`jit/src/ir_lower.rs`) and the shared stubs in `jit/src/runtime_lowering.rs`.

Table shape: **75 fields, 600 bytes, `JIT_HELPERS_ABI_VERSION` 12.** Verified
against the source on 2026-09-12.

## 0. What is actually load-bearing

The JIT does **not** bake each slot's byte offset into generated code
(`CALL [helpers + disp32]`):

- The single-pass `Compiler` holds `helpers: JitRuntimeHelpers` **by value**
  (`jit/src/x64.rs`), passed in by `x64/driver.rs`. `Lowerer::new`
  (`jit/src/ir_lower.rs`), `try_compile`, `try_compile_with_invokespecial_resolver`
  and `try_compile_inner` (`jit/src/lib.rs`) take `&JitRuntimeHelpers`. The
  `Lowerer` copies individual slots into its own `usize` fields. Every slot is
  read by Rust field name, never by computed offset.
- Every helper call bakes the helper's **absolute address**. In the single-pass
  emitter, `emit_call_absolute` (`jit/src/x64/emit.rs`) emits `E8 rel32` when the
  target is within ±2 GiB, and otherwise `MOV RAX, imm64 ; CALL RAX`
  (`emit_call_imm64_via_rax`). The IR lowerer and `runtime_lowering.rs` emit
  `MOV RAX, imm64 ; CALL RAX` by hand.
- No code in `jit/` or `vm/` addresses the table by offset (`helpers_ptr`,
  `offset_of!(JitRuntimeHelpers`, `HELPERS_OFF` find nothing outside
  `jit-api`'s own tests). Two comments in `jit-api/src/lib.rs` still describe
  `[helpers_ptr + disp32]` loads; they are stale.

Consequences:

1. A *reorder* of the struct does not, on its own, mis-target a call inside this
   workspace: producer and consumer are the same Rust type. The offsets still
   matter for `as_words` / `word_at` and for any future non-Rust producer, so
   the append-only rule is enforced anyway.
2. The sharp edge is the **signature**. Each call site loads argument registers
   by hand and calls a bare address. §3 lists what now checks that, and what
   still does not.

## 1. Invariant table

`CT` = compile-time (`const` assertion; fails the build).
`RT` = runtime (`#[test]`, or the startup check in `build_helpers`).

| # | Invariant | Asserted where | CT/RT | What edit trips it |
|---|---|---|---|---|
| 1 | `NUM_FIELDS` == `size_of::<H>() / 8` | `lib.rs` `const _` after `helper_fields!` | CT | Add a struct field without a `helper_fields!` row |
| 2 | `NUM_FIELDS` == literal 75 | `lib.rs` `const _`; `helpers_abi.rs` `const _` (`NUM_HELPER_FIELDS == 75`) | CT | Add **or remove** a field |
| 3 | `size_of::<H>()` == literal 600 | `helpers_abi.rs` `const _` (`JIT_HELPERS_ABI_SIZE == 600`) | CT | Any size change |
| 4 | `align_of::<H>()` == 8 | `helpers_abi.rs` `const _` | CT | A field with larger alignment |
| 5 | size == `fields * stride` (no padding) | `helpers_abi.rs` `const _` | CT | A non-`usize` field |
| 6 | `HELPER_FIELDS` covers every field | `helpers_abi.rs` `const _` | CT | Add a field, forget the descriptor row |
| 7 | Descriptor row *i* sits at `i * 8` | `helpers_abi.rs` `const _` | CT | Reorder the struct without reordering the descriptor table |
| 8 | **Golden name → literal offset**, and the golden offsets are dense (`i * HELPER_FIELD_STRIDE`) | `helpers_abi.rs` `GOLDEN_HELPER_OFFSETS` + `const _` | CT | Any rename, insert, delete, or reorder, including a coordinated one |
| 9 | Callable count == alias count | `helpers_abi.rs` `const _` | CT | Add a `Function` row, forget the alias |
| 10 | Callable rows and alias rows match **by name** | `helpers_abi.rs` `const _` | CT | Compensating edits that keep the count |
| 11 | Required-slot count == 43 | `helpers_abi.rs` `const _` (twice) | CT | Promote/demote a slot |
| 12 | Function / Offset / Constant census == 60 / 4 / 11, and they sum to `NUM_HELPER_FIELDS` | `helpers_abi.rs` `const _` | CT | Reclassify a slot's kind |
| 13 | Optional-callable count == 17, and `required + optional_fns == functions` | `helpers_abi.rs` `const _` | CT | Promote/demote a callable slot |
| 14 | No non-callable slot is `required` | `helpers_abi.rs` `const _` | CT | Mark an `Offset`/`Constant` required |
| 15 | Descriptor kinds agree with crate-root `FieldKind` | `helper_fields_agree_with_crate_root` | RT | Classify a slot differently in the two lists |
| 16 | `ABI_REVISIONS` newest row == this table | `helpers_abi.rs` `const _` | CT | Append a field without bumping `JIT_HELPERS_ABI_VERSION` |
| 17 | Ledger is append-only and dense | `helpers_abi.rs` `const _` | CT | Record a removal or a version gap |
| 18 | Accessor name == `field` + `_fn` | `helpers_abi.rs` `const _` (per macro row) | CT | A `helper_fn_slots!` row whose getter and field disagree |
| 19 | Every int/pointer helper argument is 8 bytes wide | `helpers_abi.rs` `const _` | CT | Declare a helper argument as `i32`/`u32`/`bool` |
| 20 | Helper arity <= 6 (`SYSV_INT_ARG_REGS`) | `helpers_abi.rs` `const _` | CT | A 7-argument helper |
| 21 | No helper mixes integer and float arguments (`float_args == 0 \|\| float_args == arity`) | `helpers_abi.rs` `const _` | CT | A `(i64, f64)` helper — Win64 assigns register files positionally, SysV does not |
| 22 | Float helpers have <= 4 arguments (`WIN64_INT_ARG_REGS`) | `helpers_abi.rs` `const _` | CT | A 5-float helper |
| 23 | Exactly 1 helper spills args to the Win64 stack (`HELPERS_NEEDING_WIN64_STACK_ARGS == 1`) | `helpers_abi.rs` `const _` | CT | A second >4-argument helper |
| 24 | Argument/return types are from a closed set | `HelperArgAbi` / `HelperRetAbi` have no blanket impl | CT | Any new argument or return type |
| 25 | `size_of::<usize>() == 8` | `lib.rs` `const _`; `helpers_abi.rs` `const _` | CT | A 32-bit target |
| 26 | fn pointer / thin raw pointer / `Option<fn>` are one word | `helpers_abi.rs` `const _` | CT | A target where any of those is not a plain machine word |
| 27 | Each `<field>_fn` accessor reads **only** its own slot | `accessor_reads_only_its_own_slot` | RT | Two swapped rows in `helper_fn_slots!` |
| 28 | Every accessor is `None` on an unwired slot | `every_accessor_is_none_on_a_zeroed_table` | RT | Covers all 60 accessors |
| 29 | `validate_with` rejects each null required slot | `jit_runtime_helpers_validate_rejects_each_required_null` | RT | Drop a slot from the validator |
| 30 | An `Offset` slot holding a pointer is rejected | `validate_with_rejects_a_pointer_stored_in_an_offset_slot` | RT | Store a pointer in a displacement slot |
| 31 | ABI version mismatch is rejected | `validate_with_rejects_a_foreign_abi_version` | RT | — |
| 32 | **The production table validates at startup** | `build_helpers_opt` (`vm/src/jit/helpers.rs`): `if let Err(e) = helpers.validate_abi() { panic!("JIT helper table is not usable: {e}"); }` | RT (every VM start) | A required slot left 0, a pointer in an offset slot |
| 33 | **The producer's functions match the `HelperFn*` aliases** | the `const _` block after `build_helpers` in `vm/src/jit/helpers.rs` (`let _: HelperFnNewarray = jit_newarray;` …) | CT | Changing a `jit_*` helper's Rust signature without changing its alias |

Invariant 33 does not name every alias. `HelperFnGetCurrentThread` is absent on
purpose: the alias returns `*mut c_void` and `jit_get_current_thread` returns
`*mut JvmThread`, which fn-pointer coercion cannot express, and the block's
comment says so. `HelperFnFfmSegmentGet`, `HelperFnFfmSegmentSet` and
`HelperFnLocalHandlerLookup` are also not named there, and no comment says why.

## 2. The machinery that enforces the table

All in `jit-api/src/helpers_abi.rs` unless noted.

1. **`GOLDEN_HELPER_OFFSETS`** — 75 literal `(name, byte offset)` rows, with
   `const` assertions that each row names `HELPER_FIELDS[i]`, matches its
   `offset_of!` value, and sits at `i * 8`. This is the check a *coordinated*
   reorder cannot satisfy.
2. **`ABI_REVISIONS`** — the shape ledger, with `const` assertions that the
   newest row equals `(JIT_HELPERS_ABI_VERSION, NUM_HELPER_FIELDS,
   JIT_HELPERS_ABI_SIZE)`, that versions are dense from 1, that
   `size == fields * 8`, and that field counts strictly increase:

   | v | fields | bytes | Appended |
   |---|---|---|---|
   | 1 | 58 | 464 | the table before the constant-pool-indexed allocation helpers |
   | 2 | 60 | 480 | `new_object_cp`, `anewarray_object_cp` |
   | 3 | 62 | 496 | `monitor_enter`, `monitor_exit` (first shipped under v2 by mistake) |
   | 4 | 63 | 504 | `ldc_class_cp` |
   | 5 | 64 | 512 | `aastore_type_check` |
   | 6 | 65 | 520 | `read_bounds_addr` |
   | 7 | 66 | 528 | `local_handler_lookup` |
   | 8 | 67 | 536 | `ldc_string_cp` (the old `ldc_string` stays, no longer emitted) |
   | 9 | 69 | 552 | `ffm_segment_get`, `ffm_segment_set` |
   | 10 | 72 | 576 | `ref_store_pre_gate`, `ref_store_post_gate`, `ref_store_post_young_floor` |
   | 11 | 74 | 592 | `g1_barrier_addr`, `g1_post_write_barrier` |
   | 12 | 75 | 600 | `ref_store_post_skip_mask` |

3. **`HELPER_FN_SIGS`** — per callable slot `{field, accessor, alias, arity,
   float_args, returns_value, returns_float}`, derived from the same
   `helper_fn_slots!` rows that declare the `HelperFn*` aliases. Plus
   `HelperFnSig::int_args()` and `win64_stack_args()`.
4. **`HelperArgAbi` / `HelperRetAbi`** — closed classification traits with no
   blanket impl.
5. **Signature `const` assertions** — invariants 19–23 and the by-name
   callable ↔ alias cross-check.
6. **Accessor naming pin** — `accessor_name_matches_field(getter, field)`.
7. **Census `const` assertions** — invariants 11–14.
8. **Platform `const` assertions** — invariants 25–26.
9. **`typed_helper_addr!`** — an exported macro that coerces a function through
   its `HelperFn*` alias before taking its address. It still has **no caller**
   outside `jit-api` (its doctest and one test). The producer checks signatures
   with the separate `const _` block instead (invariant 33).
10. **Derived tests** — `accessor_reads_only_its_own_slot`,
    `every_accessor_is_none_on_a_zeroed_table`,
    `golden_offsets_are_the_struct_offsets`,
    `abi_revision_ledger_names_the_current_table`,
    `helper_fn_sigs_record_the_real_c_signatures` (which pins
    `invoke_virtual_mic` at arity 6 with two Win64 stack arguments),
    `slot_census_matches_the_pinned_counts`,
    `typed_helper_addr_yields_the_declared_functions_address`; in
    `jit-api/src/lib.rs`, `jit_runtime_helpers_all_fields_classified` and
    `jit_runtime_helpers_repr_c_golden_offsets`.

## 3. Still unasserted, and why

| Invariant | Status |
|---|---|
| **Each call site's hand-written argument setup matches the helper's arity and return-ness** | Unasserted. `HELPER_FN_SIGS` is not referenced outside `jit-api`, and no `helper_sig` lookup exists. The check would have to live in `jit/`. |
| **`CALL_ARG_REGS` is wide enough** | `jit/src/ir_lower.rs` caps `CALL_ARG_REGS` at 4 on both platforms ("capped at 4 because the dispatch helper only ever takes 4"), and no `const` assertion ties it to `HELPERS_NEEDING_WIN64_STACK_ARGS`. The 6-argument `invoke_virtual_mic` site loads its fifth and sixth arguments by hand (§4). |
| **A `required` slot really is `CALL`ed unconditionally** | A hand classification, cross-checked between the two field lists (invariant 15) only. |
| **Every optional slot is zero-checked before its `CALL` site** | Same; the census pin forces a human to look when the count changes. |
| **A helper that never returns** | No helper is `-> !` today. |
| **Alias name matches the field** | Not derivable without a proc macro; the getter ↔ field pin covers the reachable path. |
| **`x86_64` specifically** | Only 64-bit-ness is asserted. The Win64 reasoning in invariants 21–23 is x86-64-specific. |

## 4. Calling convention at a helper call site (x86-64)

### Argument registers

| Constant | Where | Windows | System V |
|---|---|---|---|
| `ARG_REGS` | `jit/src/x64/reg_encoding.rs` (single-pass) | RCX, RDX, R8, R9 | RDI, RSI, RDX, RCX, R8, R9 |
| `CALL_ARG_REGS` | `jit/src/ir_lower.rs` (helper calls) | RCX, RDX, R8, R9 | RDI, RSI, RDX, RCX (4 only) |
| `ENTRY_ABI_REGS` | `jit/src/ir_lower.rs`, `jit/src/runtime_lowering.rs` (JIT method entry) | RCX, RDX, R8, R9 | RDI, RSI, RDX, RCX, R8, R9 |

Every helper argument is an 8-byte integer or pointer, or every argument is a
float (invariant 21). So register assignment is positional on both ABIs.

### Stack arguments, shadow space, alignment

* **Win64** requires 32 bytes of shadow space above the return address. Both the
  single-pass `Compiler::new` and `Lowerer::new` budget `shadow = 32` plus a
  16-byte `stack_arg_reserve` directly above it, sized for the one 6-argument
  helper. Stack arguments 5–6 go at `[RSP+32]` / `[RSP+40]`.
* **`invoke_virtual_mic`** is that helper (invariant 23). On Windows the call
  site writes `mic` and `pic` with `MOV [RSP+32], RAX` / `MOV [RSP+40], RAX`; on
  System V it loads them into R8 / R9. The sites are in `ir_lower.rs` and
  `x64/bytecode_walk.rs`.
* Both ABIs require `RSP ≡ 0 (mod 16)` immediately before the `CALL`
  (`x64/frames.rs`). `stack_arg_block_size` rounds the shadow space plus stack
  arguments up to 16, and `emit_stack_arg_setup` / `emit_stack_arg_cleanup`
  bracket a call that needs a block.

### What survives a helper call

* **GPRs.** Only callee-saved registers survive: RBX, RBP, R12–R15 on both ABIs,
  plus RSI and RDI on Win64. `x64::LOCAL_REGS` (the registers the single-pass
  colourer gives locals) and the IR tier's `IR_GP_LINEAR_SCAN` are drawn from
  exactly that set, so a register-held local survives by ABI. RAX, RCX, RDX,
  R8–R11 are clobbered, and the emitters use R10 / R11 as scratch around calls.
* **XMM.** Win64 preserves XMM6–XMM15, all 128 bits (`XMM_SAVE_SLOT_BYTES = 16`
  in `jit/src/x64.rs`). System V preserves none. The single-pass scratch XMM pool
  is restricted to the volatile subset on Windows.
* **RAX** carries the return value. The post-call sequence below is written not
  to disturb it.

## 5. Obligations around a GC-capable helper call

A helper that can allocate, park or throw is a GC safepoint. The collector may
move objects while it runs, and it finds this frame's references through the
shadow stack and the oop map. The emitter therefore owes three things.

### Before the call: publish

* **Single-pass.** `emit_pre_safepoint_spill` (`jit/src/x64/safepoint.rs`) spills
  the register file into the frame's `reg_spill` region (a blind image a
  conservative scan can read; `OopMapEntry::reg_oop_mask` narrows it where the
  compiler can prove which registers hold references). It then pushes the live
  references onto the shadow stack with `emit_shadow_push`. Only after that are
  `ARG_REGS` loaded and the helper called.
* **IR tier.** `Lowerer::emit_safepoint_map` (`jit/src/ir_lower.rs`) runs
  **before** the call and before the result slot is allocated. It stores the
  safepoint id into `[rbp - sp_id_slot_off]`, pushes every live `Ref` home slot
  (plus the reference-parameter homes) with `emit_shadow_push`, and records the
  `OopMapEntry`. It is emitted before `CALL_ARG_REGS` are loaded, because the
  push clobbers RCX (`CALL_ARG_REGS[0]` on Win64); the test
  `the_aastore_arm_maps_before_it_marshals` pins that order.
* The thread pointer is not kept in a register. The prologue caches it in the
  frame slot `[rbp - shadow_thread_slot_off]`; the push reloads it into R10.

### After the call: reload, then republish, then check

* **IR tier.** `emit_call_return_check` is the choke point every dispatch route
  shares, and it runs, in order:
  1. `emit_post_call_frame_record` — republish this frame's RBP (and compile id)
     in the TLS frame record, because a compiled callee overwrote it with its
     own;
  2. `emit_shadow_reload` — copy the possibly relocated values back from the
     shadow stack into their frame slots and retract `top` to the saved base.
     It uses R10, R11 and RCX, never RAX, and runs before the result is stored;
  3. the `i64::MIN` sentinel check (below);
  4. the result store.

  Leaf helpers such as `getfield` use `emit_helper_sentinel_check` and skip the
  reload. Push and reload counts must match: `lower_inner` refuses the body when
  `shadow_pushes != shadow_reloads`.
* **Single-pass.** `emit_oop_map_for_safepoint` runs right after the `CALL`. It
  records the map at `native_pc_offset = buf.pos()` (the return address) and
  calls `emit_shadow_reload` first. After any call that can reach compiled code,
  `emit_post_call_rbp_republish` restores this frame's RBP / compile-id pair in
  the TLS frame record without touching RAX.
* **Both.** The epilogue restores the shadow-stack watermark it saved in the
  prologue.

### On the VM side

`vm/src/jit/helpers.rs` helpers call
`crate::jit::conservative_roots::note_jit_boundary()` on entry, which invalidates
the JIT-frame scan cache so the next root scan re-walks the frames.
`jit_frame_record(rbp)` is what the prologue calls when the inline TLS store is
unavailable; helpers do not call it themselves.

### The exception / deopt sentinel

A helper or callee signals "exception or deopt pending" by returning `i64::MIN`.
The emitter compares with `MOV R10, i64::MIN ; CMP RAX, R10`, which clobbers R10:

* for `int`, reference and `void` results a match bails straight to the
  exception path;
* for `long`, `double` and `float` results `i64::MIN` can be a real value, so on
  a match the emitter calls `dispatch_threw` (`jit_dispatch_threw`, which peeks
  the thread's signal block without clearing it and returns `1` when something
  is pending), restores `RAX = i64::MIN`, and bails only on `1`.

Single-pass: `emit_post_invoke_exception_check` and the bail stub
`emit_exception_check_stub` (`jit/src/x64/deopt_stubs.rs`), which calls
`set_throw_bci`, loads `i64::MIN` and runs the epilogue. IR tier:
`emit_call_return_sentinel_tail` and `emit_helper_sentinel_check`.

Some helpers have their own convention instead:

* `ldc_class_cp` / `ldc_string_cp` return 0 when an exception is pending;
* `aastore_type_check` returns `i64::MIN` to refuse the store;
* `local_handler_lookup` returns -1 to propagate;
* `ffm_segment_get` / `ffm_segment_set` return 1 when handled and 0 when
  declined.

## 6. The helper list

Offset = index × 8. Kind: F = `Function`, O = `Offset` (a displacement baked as
`disp32`), C = `Constant` (loaded as data, never called). R = required,
o = optional (zero means "not wired" and the emitter must not call it).
Source: `helper_field_table!` in `jit-api/src/helpers_abi.rs`.

| # | field | kind | | # | field | kind |
|---|---|---|---|---|---|---|
| 0 | `newarray` | F R | | 38 | `get_current_thread` | F o |
| 1 | `new_object` | F R | | 39 | `tlab_post_init` | F o |
| 2 | `anewarray_object` | F R | | 40 | `frame_record` | F o |
| 3 | `baload` | F R | | 41 | `shadow_stack_offset_in_thread` | O |
| 4 | `bastore` | F R | | 42 | `throw_exception` | F R |
| 5 | `iaload` | F R | | 43 | `jit_npe_with_action` | F R |
| 6 | `iastore` | F R | | 44 | `dispatch_threw` | F R |
| 7 | `aaload` | F R | | 45 | `jit_frem` | F R |
| 8 | `aastore` | F R | | 46 | `jit_drem` | F R |
| 9 | `multianewarray_2d` | F R | | 47 | `self_call_stack_guard` | F o |
| 10 | `arraylength` | F R | | 48 | `region_bounds_addr` | C |
| 11 | `getfield` | F R | | 49 | `native_stack_floor_fn` | F o |
| 12 | `putfield_int` | F R | | 50 | `ldc_string` | F R |
| 13 | `putfield_long` | F R | | 51 | `safepoint_flag_addr` | C |
| 14 | `putfield_float` | F R | | 52 | `safepoint_slow_path` | F o |
| 15 | `putfield_double` | F R | | 53 | `jit_card_table_addr` | C |
| 16 | `putfield_object` | F R | | 54 | `jit_card_old_base` | C |
| 17 | `getstatic` | F R | | 55 | `jit_card_old_end` | C |
| 18 | `putstatic_int` | F R | | 56 | `set_throw_bci` | F R |
| 19 | `putstatic_long` | F R | | 57 | `service_callee_deopt` | F o |
| 20 | `putstatic_float` | F R | | 58 | `new_object_cp` | F o |
| 21 | `putstatic_double` | F R | | 59 | `anewarray_object_cp` | F o |
| 22 | `putstatic_object` | F R | | 60 | `monitor_enter` | F o |
| 23 | `checkcast` | F R | | 61 | `monitor_exit` | F o |
| 24 | `instanceof_check` | F R | | 62 | `ldc_class_cp` | F o |
| 25 | `throw_aioobe` | F R | | 63 | `aastore_type_check` | F R |
| 26 | `throw_arithmetic` | F R | | 64 | `read_bounds_addr` | C |
| 27 | `invoke_dispatch` | F R | | 65 | `local_handler_lookup` | F o |
| 28 | `invoke_virtual_mic` | F R | | 66 | `ldc_string_cp` | F o |
| 29 | `lambda_int_to_double` | F R | | 67 | `ffm_segment_get` | F o |
| 30 | `write_barrier` | F R | | 68 | `ffm_segment_set` | F o |
| 31 | `satb_pre_write_barrier` | F R | | 69 | `ref_store_pre_gate` | C |
| 32 | `uncommon_trap` | F R | | 70 | `ref_store_post_gate` | C |
| 33 | `math_fma_double` | F R | | 71 | `ref_store_post_young_floor` | C |
| 34 | `math_fma_float` | F R | | 72 | `g1_barrier_addr` | C |
| 35 | `tlab_cursor_offset_in_thread` | O | | 73 | `g1_post_write_barrier` | F o |
| 36 | `tlab_end_offset_in_thread` | O | | 74 | `ref_store_post_skip_mask` | C |
| 37 | `class_id_offset_in_obj` | O | | | | |

Signature notes, from `helper_fn_slots!`:

* `invoke_virtual_mic` is the only 6-argument helper;
* the all-float helpers are `math_fma_double` / `math_fma_float` (3 arguments)
  and `jit_frem` / `jit_drem` (2 arguments);
* `aaload` takes `(i64, i64, i64) -> i64`, including `vm_ptr`;
* `ref_store_post_skip_mask` is a value, not an address.

## 7. Procedure for adding a helper field

Follow in order. Each step has a tripwire that fires if you skip it.

1. **Append** `pub <name>: usize,` at the **end** of `JitRuntimeHelpers`
   (`jit-api/src/lib.rs`). Never insert, never reorder, never remove.
2. Add `(<name>, FieldKind::…)` at the **end** of `helper_fields!` (`lib.rs`).
   *(Invariant 1.)*
3. Bump the `NUM_FIELDS == 75` literals (`lib.rs`, `helpers_abi.rs`) and the
   `JIT_HELPERS_ABI_SIZE == 600` literal.
4. Append a row to `helper_field_table!` (`helpers_abi.rs`) with the right
   `HelperKind` and `required` flag. *(Invariant 6.)*
5. If it is callable, append a row to `helper_fn_slots!` giving the real C
   signature. *(Invariants 9 and 10.)* If the argument or return type is not
   already classified, add a `HelperArgAbi` / `HelperRetAbi` impl.
6. Append the `(name, offset)` row to `GOLDEN_HELPER_OFFSETS`, offset = previous
   + 8. *(Invariant 8.)*
7. Append a row to `ABI_REVISIONS` and set `JIT_HELPERS_ABI_VERSION` to it.
   *(Invariant 16.)*
8. Update the census literals (60 / 4 / 11, required 43, optional 17) and the
   counts in `slot_census_matches_the_pinned_counts` and
   `jit_runtime_helpers_all_fields_classified`. **Before you change the optional
   count, confirm the emitter zero-checks the new slot; before you change the
   required count, confirm the emitter really `CALL`s it unconditionally** — a
   required slot is what `validate_abi` refuses to start without (invariant 32).
9. Append probe rows to `helper_fields_offsets_match_offset_of`
   (`helpers_abi.rs`) and `jit_runtime_helpers_repr_c_golden_offsets` (`lib.rs`),
   and update `as_words_matches_the_struct_fields`, which names the last slot.
10. If the slot is required, add an arm to `zero_field_by_name` in `lib.rs`'s
    tests, or the required-null sweep panics on it.
11. Wire it in `vm/src/jit/helpers.rs::build_helpers`, and add a
    `let _: HelperFn<Name> = jit_<name>;` line to the `const _` signature block
    beside it (invariant 33).
12. If the new helper takes more than four arguments, its Win64 call site must
    write arguments 5+ to `[RSP+32]`, `[RSP+40]`, …, the 16-byte
    `stack_arg_reserve` must cover them, and the
    `HELPERS_NEEDING_WIN64_STACK_ARGS == 1` pin stops the build until you have
    done so and updated the literal.
13. If the call can reach a GC, give its site the §5 obligations: publish
    before the call, reload and sentinel-check after it.

## 8. Known stale comments in the source

These contradict the code as of this update. They are outside this document's
edit scope, so they are listed here rather than changed:

* `jit-api/src/helpers_abi.rs` module doc: "**The validator is not armed.**" —
  `build_helpers_opt` now calls `validate_abi()` and panics on failure.
* `jit-api/src/helpers_abi.rs`, the doc on `JIT_HELPERS_ABI_VERSION`: "`6` is the
  revision of the 65-field, 520-byte table shipped today" — the constant is 12.
* `jit-api/src/lib.rs`: two comments describing `[helpers_ptr + disp32]` loads
  (§0).

## 9. Panics in helpers

The workspace builds with `panic = "unwind"`. Since Rust 1.81, a panic that
unwinds out of an `extern "C"` function aborts the process, with no
Java-visible error and no metric. `vm/src/jit/helper_guard.rs` contains those
panics. Its module doc is the authoritative list; this section states the
contract.

### Shape

A guarded helper keeps its `extern "C"` signature, so the table, the `HelperFn*`
aliases and invariant 33 are untouched. Its original body moves, textually
unchanged, into a private `<name>_body` function, and the shell becomes:

```rust
pub unsafe extern "C" fn jit_monitor_exit(vm_ptr: i64, obj_ptr: i64) -> i64 {
    let f = || jit_monitor_exit_body(vm_ptr, obj_ptr);
    contain("jit_monitor_exit", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}
```

`contain(name, on_panic, sentinel, body)` costs a `catch_unwind` plus one
thread-local scope depth on the normal path. It takes no lock and allocates
nothing. The sentinel is an argument, not a trait, because helpers with the
same return type fail differently.

### On a panic

1. Increment the process-wide counter `jit_helper_panic_count()`. Record the
   helper name for `jit_helper_last_panic()` and `jit_helper_recent_panics()`
   (the last eight).
2. The first time a given helper panics, print one `[jit-helper-panic]` line to
   stderr with the helper name and the payload.
3. Apply the wrap site's policy:

   | Policy | Used for | Effect |
   |---|---|---|
   | `Throw { vm_ptr }` | Helpers whose normal path already allocates, runs Java or parks, so the call site is already a GC safepoint | Stash `java.lang.InternalError: JIT runtime helper <name> panicked: <payload>` with `set_jit_pending_exception` (`vm_ptr == 0` uses the process VM). If that cannot be built, only the sentinel is returned. |
   | `Deopt` | `uncommon_trap`, whose answer is already to reinterpret | Allocate nothing. Call `set_jit_deopt_pending()`, so an `i64::MIN` return reads as a sentinel. |
   | `Record` | Fast paths whose failure answer is "declined" | Count and report only. |

4. Return the sentinel.

### Sentinels

Each guarded helper returns the failure answer its call site already handles
(§5):

| Sentinel | Helpers |
|---|---|
| `i64::MIN`, `Throw` | `invoke_dispatch`, `invoke_virtual_mic`, `service_callee_deopt`, `indy_bridge`, `lambda_int_to_double`, `varhandle_read_direct`, `varhandle_cas_direct`, the `integer_*`, `long_*`, `dbb_*`, `md_update_byte`, `preconditions_check_index`, `buffer_session`, `thread_current_thread`, `concurrent_hashmap_get`, `hashmap_get`, `hashmap_put` and `string_latin1_to_lower` direct intrinsics, `monitor_enter`, `monitor_exit`, `checkcast`, `aastore_type_check`, `getstatic`, `putstatic_*`, `self_call_stack_guard` |
| `0` / null, `Throw` | `newarray`, `new_object`, `new_object_cp`, `anewarray_object`, `anewarray_object_cp`, `multianewarray_2d`, `ldc_class_cp`, `ldc_string_cp`, `ldc_string`, `instanceof` |
| `0`, `Record` | `ffm_segment_get`, `ffm_segment_set` (declined; the native path runs) |
| `-1`, `Throw` | `local_handler_lookup` (propagate) |
| `DEOPT_ACTION_REINTERPRET`, `Deopt` | `uncommon_trap` |
| `i64::MIN`, `Deopt` | `baload`, `iaload`, `aaload`, `arraylength`, `getfield`, `throw_aioobe`, `throw_arithmetic`, `throw_exception`, `npe_with_action` |
| `0`, `Deopt` | `tlab_post_init` |
| `()`, `Deopt` | `bastore`, `iastore`, `putfield_int`, `putfield_long`, `putfield_float`, `putfield_double` |
| `()`, `Throw` | `aastore`, `varhandle_write_direct`, `safepoint_slow_path` |

A void helper has no failure channel. A `Throw` stash is delivered at the
thread's next pending-exception drain, not at the faulting instruction, and a
`Record` panic is invisible to Java.

### Unguarded helpers must stay panic-free

These are not wrapped:

* `set_deopt_pending`, `dispatch_threw`, `set_throw_bci`, `get_current_thread`
* `native_stack_floor`, `frame_record`, `verify_inline_frame_record`
* `math_fma_double`, `math_fma_float`, `jit_frem`, `jit_drem`
* `reachability_fence_direct`, `resolve_static_base`
* the savebase watch: `arm_savebase_watch` is `#[naked]`, and its inner half
  belongs to the crash handler

`write_barrier`, `g1_post_write_barrier`, `satb_pre_write_barrier` and
`putfield_object` are left unguarded on purpose. A contained panic there would
silently drop a card mark, remembered-set entry or SATB record, which is a
latent use-after-free, so the abort is preferred.

Any change to an unguarded helper must keep it free of panics. A new helper
that cannot be shown panic-free must be guarded, and it uses `Throw`, which
needs a GC-safepoint call site. `Deopt` is reserved for sites whose answer is
already a reinterpretation.

### Crash reporting and limits

* `contain` runs the body inside `cratonvm_jit::tiered::contain_compile_panic`'s
  scope. The hook installed by `crash_handler::install_crash_handler` already
  honours that scope, so a contained helper panic writes no
  `hs_err_pid<pid>.log`. Hooks still run first. That hook chains to the default
  one, and `vm-cli`'s own hook prints every panic.
* Unwinding releases every guard, lock and `RefCell` borrow the body held.
  State undone by an explicit call rather than by `Drop` is not restored: a
  `set_jit_thread` scope, a thread-state transition, a half-initialised object.
* A panic raised while already unwinding still aborts.
