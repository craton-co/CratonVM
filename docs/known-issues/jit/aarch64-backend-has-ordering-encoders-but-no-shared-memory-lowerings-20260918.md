# aarch64 backend: ordering encoders exist, shared-memory lowerings do not

Status: PARTIALLY FIXED (round 9 wave 10, aarch10; wired in `lib.rs` by wave 10b, wire10b; wave 11, aarch11) -- step 1 done for `getstatic`, step 3 started (ArithmeticException throw path, `idiv`/`irem`/`ldiv`/`lrem`); see "Status after wave 10", "wave 10b" and "wave 11"
Area: `jit/src/aarch64_backend.rs`, `jit/src/aarch64.rs`
Severity: capability gap (not a miscompile). Default builds are unaffected:
the backend is opt-in (`CRATONVM_JIT_ARM64`) and never runs on x86-64.

## What is true today

* `aarch64_backend::opcode_touches_shared_memory` + `ARM64_CAN_ORDER_MEMORY = false`
  refuse every field access, array access, allocation, invoke, monitor,
  `athrow`, `checkcast`/`instanceof`, before the opcode dispatch runs. So none
  of the x64-side JMM work from rounds 1-9 (volatile `MFENCE`, `putstatic`
  narrowing, `bastore` boolean masking, the `final`-field freeze) has an
  aarch64 counterpart, and none is needed yet: an aarch64 body never touches
  memory another thread can see. Checked in round 9 wave 9: the gate covers
  `0x2e..=0x35`, `0x4f..=0x56`, `0xb2..=0xbe`, `0xbf`, `0xc0..=0xc3`, `0xc5`;
  `ldc*` (`0x12..=0x14`) refuse in their own arms; `wide`, `goto_w`, `jsr*`
  have no arm and refuse through the dispatch's default.
* Round 9 wave 9 added the encoders the gate was waiting on (`aarch64.rs`:
  `ldar`/`ldar_w`/`ldarh`/`ldarb`, `stlr`/`stlr_w`/`stlrh`/`stlrb`,
  `ldaxr`/`ldaxr_w`, `stlxr`/`stlxr_w` (refuses an aliased status register),
  `casal`/`casal_w`), pinned by
  `aarch64::tests::test_acquire_release_and_exclusive_encodings`.
  `the_ordering_constant_agrees_with_the_encoders_that_actually_exist` now
  requires production call sites in `aarch64_backend.rs` as well before the
  constant may flip.
* `idiv`/`ldiv`/`irem`/`lrem` still refuse: there is no exception path
  (`ArithmeticException` needs a runtime helper call and a deopt-free throw).
* Parity fixed in wave 9: `ireturn` now narrows `Z`/`B`/`C`/`S` returns, as
  x64's `emit_narrow_int_return` does
  (`aarch64_backend::tests::r9w9_ireturn_narrows_boolean_byte_char_short_like_x64`).

## Direction (in order)

1. `getstatic`/`getfield` of primitives with the `Arm64Instruction` pseudo-ops
   `Ldar{,W,H,B}` / `Stlr{,W,H,B}` for `volatile` fields (plain `LDR`/`STR`
   otherwise), a trailing `DMB ISH` after a volatile store only where an
   `LDAR` does not follow (ARMv8 `STLR`->`LDAR` is already ordered, RCsc), and
   the same `putstatic`/`putfield` narrowing x64 applies (`Z` -> `& 1`,
   `B`/`S` -> `SXTB`/`SXTH` before the store, `C` -> `& 0xFFFF`). Needs the
   field resolver the x64 tier gets from `build_single_pass_tables`.
2. `bastore` with the boolean-array `& 1` mask and array bounds checks — these
   need a throw path, which is the same prerequisite as `idiv`.
3. A throw/uncommon-trap path (helper `BLR` + frame walk) — unblocks
   `idiv`/`irem`, bounds checks, `checkcast`.
4. Only then open `ARM64_CAN_ORDER_MEMORY` (the guard test enforces this).

All of this is unexecutable on the project's x86-64 hosts; see
`docs/jit/aarch64-running-the-tests.md` for the qemu/container route, and arm
`CRATONVM_DBG_VERIFY_OOP_MAPS` on the first run.

## Status after wave 10 (aarch10)

Done (step 1, the read half, for statics):

* `jit/src/aarch64_backend.rs`: new pseudo-ops `MemLoad { rt, rn, width:
  Arm64MemWidth, acquire }`, `MemStore { rt, rn, width, release }` and
  `DmbIsh`, encoded at exactly `[Xn]` in `emit_machine_code_inner`
  (`LDR{B,H}`/`LDR W`/`LDR X` or `LDAR{B,H}`/`LDAR W`/`LDAR X`; `STR*` or
  `STLR*`; `DMB ISH` = `0xD5033BBF`). These are the first production call
  sites of the wave-9 `ldar*`/`stlr*` encoders.
* `jit/src/aarch64.rs`: plain narrow encoders `ldrh_imm`, `strb_imm`,
  `strh_imm` (the widths `MemLoad`/`MemStore` needed that did not exist).
* `getstatic` lowering (`Arm64Backend::emit_getstatic`, the `0xb2` arm): a
  caller-resolved PRIMITIVE static (`Arm64StaticField { class_id,
  field_index, base_cell, type_tag, is_volatile }`, supplied with
  `Arm64Backend::set_static_field_info`) is read as x64's inline getstatic
  reads it, `[[base_cell] + field_index*SLOT_SIZE + payload]`, 32-bit payload
  for `Z B C S I F`, 64-bit for `J D`; `LDAR` when volatile, plain `LDR`
  otherwise; `SXTW` for the int category, `FMOV` into S/D for float/double.
  No DMB after a volatile read (ARMv8 `STLR`->`LDAR` is RCsc; the StoreLoad
  edge is the writer's, as x64's MFENCE is after the store). Declaring class
  ids go to `Arm64CompileResult::static_init_classes` ->
  `CompiledMethod::static_init_classes`, as x64 records them.
* The gate: `opcode_has_ordered_lowering(0xb2)` lets `getstatic` past
  `opcode_touches_shared_memory`; its arm refuses any site with no resolution,
  a reference static, or a zero base cell. `ARM64_CAN_ORDER_MEMORY` stays
  `false` (the exclusive-access half has no call site), and its guard test
  still agrees.
* `getstatic_sites(bytecode)` (instruction-length walk via
  `bytecode_analysis::insn_len`) gives `lib.rs` the pcs to resolve.
* Tests: `aarch64_backend::tests::r9w10_mem_load_store_pseudo_ops_encode_width_and_ordering`,
  `r9w10_getstatic_int_reads_the_cell_payload_with_ldar_when_volatile`,
  `r9w10_getstatic_long_float_double_widths`,
  `r9w10_getstatic_refuses_unresolved_reference_and_unbased_sites`,
  `r9w10_getstatic_records_declaring_classes_once`,
  `r9w10_getstatic_sites_walks_instructions_not_bytes`;
  `aarch64::tests::r9w10_narrow_unsigned_offset_load_store_encodings`;
  `every_shared_memory_opcode_is_refused_while_the_backend_cannot_order_memory`
  updated (0xb2 must be refused by its own arm when nothing is resolved).

Not wired yet: `jit/src/lib.rs`'s `#[cfg(target_arch = "aarch64")]` block
does not call `set_static_field_info` (cross-lane request in
`docs/internal/jit-review-r9/NOTES-w10-aarch10.md`), so a default aarch64
build still refuses every `getstatic`.

Remaining (unchanged order):

1. `putstatic` -- x64 keeps it on helpers (references owe an SATB pre-barrier;
   a primitive store into a legacy 16-byte cell must keep the tag word right);
   `getfield`/`putfield` need a null check, i.e. a throw path. The
   `MemStore { release }` + `DmbIsh` pseudo-ops are ready for them (volatile
   store = `STLR` + `DMB ISH`, mirroring x64's post-store MFENCE).
2. `bastore` masking and array bounds checks (throw path).
3. The throw/uncommon-trap path.
4. Only then `ARM64_CAN_ORDER_MEMORY`.

Assumption to confirm on the first aarch64 run: the interpreter/runtime's
volatile static WRITE is a release (or SeqCst) store, so the compiled `LDAR`
reader pairs with it. Rust's `SeqCst` store/load lower to `STLR`/`LDAR` on
aarch64, which is the pairing assumed.

## Status after wave 10b (wire10b)

The "Not wired yet" item above is done: `jit/src/lib.rs`'s `#[cfg(target_arch = "aarch64")]`
block in `try_compile_inner` now walks `aarch64_backend::getstatic_sites(code)` right after
`backend.set_method_descriptor(..)`, resolves each site through `cp_static_field_resolver` and
`direct_helpers.resolve_static_base(class_id, field_index)`, and hands the map to
`backend.set_static_field_info(..)` (aarch10's edit verbatim). A resolved primitive static of an
initialized class is now lowered on aarch64 (`CRATONVM_JIT_ARM64` still gates the whole backend);
x86-64 builds do not compile the block. Not tied to x64's `inline_getstatic_enabled()` kill
switch. Unverified: the block only compiles on an aarch64 target, which the integrator's x86-64
build does not check -- a `cargo check --target aarch64-unknown-linux-gnu -p cratonvm-jit` (or
the CI aarch64 job) is the gate. Remaining items 1-4 are unchanged.

## Status after wave 11 (aarch11)

Step 3 (the throw path) is started with its first, self-contained user --
`ArithmeticException` -- which is what `idiv`/`irem`/`ldiv`/`lrem` were
waiting on. It orders no memory, so `ARM64_CAN_ORDER_MEMORY` and the
shared-memory gate are untouched (every opcode the gate refused, it still
refuses; `getstatic` stays the only ordered lowering).

* `jit/src/aarch64_backend.rs`:
  * new pseudo-ops `SDivW` / `MsubW` (encoded by the existing
    `aarch64.rs` `sdiv_w` / `msub_w`), so the `int` pair follows the module's
    "every int producer is a W form + `SXTW`" rule;
  * `Arm64Backend::set_exception_table_empty(bool)` (default `false` =
    unknown = refuse) and `can_throw_arithmetic()` =
    `helpers.throw_arithmetic != 0 && exception_table_empty`;
  * `emit_int_div_rem(kind, rem)`: `CBZ W|X divisor, throw` then `SDIV`
    (+ `MSUB dst, dst, b, a` for a remainder) (+ `SXTW` for `int`). AArch64
    `SDIV` never traps and gives `MIN / -1 = MIN`, remainder 0 -- JVMS
    exactly, so the zero test is the whole guard;
  * `emit_arith_throw_stub()`: ONE out-of-line stub per method, after the
    epilogue's `RET`: `MOV X16, #throw_arithmetic; BLR X16; B epilogue`.
    `jit_throw_arithmetic` sets the pending-arithmetic + deopt signals,
    snapshots the trap frames and returns `i64::MIN`; the interpreter's
    JIT-return drain (`jit_bridge.rs`, `sig.arithmetic`) raises the exception
    without re-running the method -- the same contract as x64's reason-3 deopt
    stub. Not a safepoint (the helper allocates nothing), so no spill and no
    map, like x64;
  * the `0x6c`/`0x6d`/`0x70`/`0x71` arm lowers through the above when
    `can_throw_arithmetic()`, and otherwise refuses with a
    "no exact ArithmeticException path" comment, byte-identical to before.
* Why the empty-table condition: the drain throws with `JitThrowPc::Unknown`,
  which can pick the wrong handler (or skip a `finally`) when this method has
  one. With no handler the only possible outcome is propagation, which is
  exact. x64 accepts the imprecision; this backend does not need to.
* Tests: `aarch64_backend::tests::r9w11_sdiv_msub_w_and_x_forms_encode`,
  `r9w11_div_rem_guard_the_divisor_and_branch_to_the_throw_stub` (all four
  opcodes; checks guard register = divisor, MSUB operands, SXTW only for int,
  stub after RET, stub = MOV X16/BLR X16/B epilogue, and the ENCODED CBZ
  displacement lands on the stub), `r9w11_divisions_share_one_throw_stub_and_none_is_emitted_without_one`,
  `r9w11_div_refuses_without_the_helper_or_the_empty_table_word`. The
  pre-existing refusal tests (`backend_idiv_bails_to_interpreter`, ...) still
  hold: `make_backend_with_method` wires neither half.

Not wired yet: `jit/src/lib.rs`'s aarch64 block must call
`backend.set_exception_table_empty(cached.exception_table.is_empty())`
(cross-lane request in `docs/internal/jit-review-r9/NOTES-w11-aarch11.md`).
`set_helpers(*helpers)` already hands over `throw_arithmetic`. Until then an
aarch64 build still refuses every division.

Remaining (order unchanged):

1. `putstatic` -- still declined, for a reason sharper than wave 10 gave: a
   raw payload store races `set_static_shared`'s `grow_to` (which copies the
   block under the statics write lock and republishes), so a compiled store
   into the old block between the copy and the republish is LOST; x64 keeps
   the helper for this and for the SATB pre-barrier on references.
2. `getfield`/`putfield`/`arraylength` -- need an NPE throw path. The shape is
   the one above, but `jit_npe_with_action` takes a per-site action code and
   the instance-field resolver/layout plumbing has no aarch64 counterpart.
3. `bastore` masking and bounds checks -- need the AIOOBE path
   (`jit_throw_aioobe(index, length, ..)`), again the same stub shape plus
   argument marshalling.
4. Only then `ARM64_CAN_ORDER_MEMORY`.

Assumption to confirm on the first aarch64 run (in addition to wave 10's):
`snapshot_trap_frames(0)` inside `jit_throw_arithmetic` can walk an aarch64
compiled frame (FP chain); if not, the exception's stack trace is shallow, but
the exception itself is still raised correctly.
