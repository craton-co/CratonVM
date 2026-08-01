# JIT helper ABI audit

Scope: `jit-api/src/lib.rs` (the `JitRuntimeHelpers` struct and the
`helper_fields!` list) and `jit-api/src/helpers_abi.rs` (the typed ABI
description). Producer: `vm/src/jit/helpers.rs::build_helpers`. Consumers:
`jit/src/x64.rs`, `jit/src/ir_lower.rs`.

Date: 2026-08-01. Table state at audit time: 62 fields, 496 bytes,
`JIT_HELPERS_ABI_VERSION` raised from 2 to 3 by this audit.

## 0. What is actually load-bearing (premise correction)

The pre-existing module doc opened with:

> the JIT bakes each slot's **byte offset** into generated RWX machine code
> (`CALL [helpers + disp32]`, `MOV reg, [helpers + disp32]`)

**That is not what the backend does.** Verified by reading:

- `jit/src/x64.rs:651` — the compiler holds `helpers: JitRuntimeHelpers` **by
  value**. `ir_lower.rs:508` and `jit/src/lib.rs:11218` take `&JitRuntimeHelpers`.
  Every slot is read by Rust field name (`self.helpers.newarray`), never by
  computed offset.
- `jit/src/x64.rs:7929` `emit_call_absolute(addr: usize)` bakes the helper's
  **absolute address** as a `rel32` displacement, falling back to a 12-byte
  `imm64` form beyond ±2 GiB.
- Searching the whole tree for `[helpers + disp32]`-style addressing
  (`helpers_ptr`, `offset_of!(JitRuntimeHelpers`, `HELPERS_OFF`) returns
  **nothing** in `jit/` or `vm/`.

Consequences for this audit:

1. A *reorder* of the struct, on its own, does not mis-target a call inside this
   workspace: producer and consumer are the same Rust type compiled from the
   same file, and `rustc` follows the reorder on both sides. The offsets still
   matter for `as_words`/`word_at` (which reinterpret the struct as
   `[usize; NUM_FIELDS]` and index it) and for any future non-Rust producer, and
   the append-only rule is still worth enforcing — but it is not the sharp edge.
2. The sharp edge is the **signature**. The backend loads N argument registers
   by hand and jumps to a bare address. Nothing connected that hand-written
   setup to the callee's declared arity, argument widths, or whether it returns
   a value. That was the largest unasserted invariant and most of the new work
   targets it.
3. The **runtime validator is not armed**: `validate_with`, `validate_abi`,
   `validate`, `null_pointers`, `HELPER_FIELDS` and `as_words` have **zero**
   callers outside `jit-api`. They are tested and correct and never run in a
   real VM. See §4.

## 1. Invariant table

`CT` = compile-time (`const` assertion — fails the build).
`RT` = runtime (`#[test]` — fails only if someone runs `cargo test -p cratonvm-jit-api`).

| # | Invariant | Asserted where | CT/RT | What edit trips it |
|---|---|---|---|---|
| 1 | `NUM_FIELDS` == `size_of::<H>() / 8` | `lib.rs` `const _` after `helper_fields!` | CT | Add a struct field without a `helper_fields!` row |
| 2 | `NUM_FIELDS` == literal 62 | `lib.rs` `const _`; `helpers_abi.rs` `const _` | CT | Add **or remove** a field |
| 3 | `size_of::<H>()` == literal 496 | `helpers_abi.rs` `const _` | CT | Any size change |
| 4 | `align_of::<H>()` == 8 | `helpers_abi.rs` `const _` | CT | A field with larger alignment |
| 5 | size == `fields * stride` (no padding) | `helpers_abi.rs` `const _` | CT | A non-`usize` field |
| 6 | `HELPER_FIELDS` covers every field | `helpers_abi.rs` `const _` vs `NUM_FIELDS` | CT | Add a field, forget the descriptor row |
| 7 | Descriptor row *i* sits at `i * 8` | `helpers_abi.rs` `const _` | CT | Reorder the struct **without** reordering the descriptor table |
| 8 | **Golden name → literal offset** | `helpers_abi.rs` `GOLDEN_HELPER_OFFSETS` + `const _` | CT | **NEW.** Any rename, insert, delete, or reorder — *including* a reorder that also reorders every derived table |
| 9 | Callable count == alias count | `helpers_abi.rs` `const _` | CT | Add a `Function` row, forget the alias |
| 10 | Callable rows and alias rows match **by name** | `helpers_abi.rs` `const _` | CT | **NEW.** Compensating edits (drop one alias, add another) that keep the count |
| 11 | Required-slot count == 42 | `helpers_abi.rs` `const _` | CT | Promote/demote a slot |
| 12 | Function/Offset/Constant census == 53/4/5 | `helpers_abi.rs` `const _` | CT | **NEW (CT).** Reclassify a slot's kind |
| 13 | Optional-callable count == 11 | `helpers_abi.rs` `const _` | CT | **NEW (CT).** Promote/demote a callable slot |
| 14 | No non-callable slot is `required` | `helpers_abi.rs` `const _` | CT | Mark an `Offset`/`Constant` required |
| 15 | Descriptor kinds agree with crate-root `FieldKind` | `helper_fields_agree_with_crate_root` | RT | Classify a slot differently in the two lists |
| 16 | `ABI_REVISIONS` newest row == this table | `helpers_abi.rs` `const _` | CT | **NEW.** Append a field without bumping `JIT_HELPERS_ABI_VERSION` — the exact hole the monitor-helper wave went through |
| 17 | Ledger is append-only and dense | `helpers_abi.rs` `const _` | CT | **NEW.** Record a removal or a version gap |
| 18 | Accessor name == `field` + `_fn` | `helpers_abi.rs` `const _` (per macro row) | CT | **NEW.** A `helper_fn_slots!` row whose getter and field disagree |
| 19 | Every int/pointer helper argument is 8 bytes wide | `helpers_abi.rs` `const _` (per argument) | CT | **NEW.** Declare a helper argument as `i32`/`u32`/`bool` |
| 20 | Helper arity <= 6 (SysV int reg file) | `helpers_abi.rs` `const _` | CT | **NEW.** A 7-argument helper |
| 21 | No helper mixes integer and float arguments | `helpers_abi.rs` `const _` | CT | **NEW.** A `(i64, f64)` helper — Win64 assigns register files positionally, SysV does not |
| 22 | Float helpers have <= 4 arguments (Win64 XMM0-3) | `helpers_abi.rs` `const _` | CT | **NEW.** A 5-float helper |
| 23 | Exactly 1 helper spills args to the Win64 stack | `helpers_abi.rs` `const _` | CT | **NEW.** A second >4-argument helper, whose args 5+ would be garbage in R8/R9 on Windows unless the call site is hand-written like `invoke_virtual_mic`'s |
| 24 | Argument/return types are from a closed set | `HelperArgAbi` / `HelperRetAbi` have no blanket impl | CT | **NEW.** Any new argument or return type — "trait bound not satisfied" until it is classified |
| 25 | `size_of::<usize>() == 8` | `lib.rs` `const _`; `helpers_abi.rs` `const _` | CT | A 32-bit target |
| 26 | fn pointer / thin raw pointer / `Option<fn>` are one word | `helpers_abi.rs` `const _` | CT | **NEW.** A target where any of those is not a plain machine word |
| 27 | Each `<field>_fn` accessor reads **only** its own slot | `accessor_reads_only_its_own_slot` | RT | **NEW.** Two swapped rows in `helper_fn_slots!` — every count, offset, size and census check still passes, but each helper gets the other's signature |
| 28 | Every accessor is `None` on an unwired slot | `every_accessor_is_none_on_a_zeroed_table` | RT | **NEW.** Was spot-checked for 9 of 53 accessors; now all 53 |
| 29 | `validate_with` rejects each null required slot | `jit_runtime_helpers_validate_rejects_each_required_null` | RT | Drop a slot from the validator |
| 30 | An `Offset` slot holding a pointer is rejected | `validate_with_rejects_a_pointer_stored_in_an_offset_slot` | RT | Store a pointer in a displacement slot |
| 31 | ABI version mismatch is rejected | `validate_with_rejects_a_foreign_abi_version` | RT | — |

## 2. What this audit added

All in `jit-api/src/helpers_abi.rs` unless noted.

1. **`GOLDEN_HELPER_OFFSETS`** — 62 literal `(name, byte offset)` rows plus a
   `const` assertion that each row names `HELPER_FIELDS[i]` and matches its
   `offset_of!` value. This is the only check in the crate a *coordinated*
   reorder cannot satisfy: every other check derives offsets from the struct,
   so moving a field and dutifully moving its row in `helper_field_table!`, in
   `helper_fn_slots!` and in the probe list passed everything.
2. **`ABI_REVISIONS`** — the shape ledger (v1 58/464, v2 60/480, v3 62/496) with
   `const` assertions that the newest row equals `(JIT_HELPERS_ABI_VERSION,
   NUM_HELPER_FIELDS, JIT_HELPERS_ABI_SIZE)`, that versions are dense from 1,
   that `size == fields * 8` in every row, and that field counts strictly
   increase (append-only). **`JIT_HELPERS_ABI_VERSION` bumped 2 → 3**, which the
   monitor-helper append should have done.
3. **`HELPER_FN_SIGS`** — per-slot `{field, accessor, alias, arity, float_args,
   returns_value, returns_float}`, *derived* from the same `helper_fn_slots!`
   rows that declare the `HelperFn*` aliases, so there is no second list to keep
   in step. Plus `HelperFnSig::int_args()` and `win64_stack_args()`.
4. **`HelperArgAbi` / `HelperRetAbi`** — closed classification traits with no
   blanket impl, so a helper using an unclassified argument or return type does
   not compile until someone decides how it is passed.
5. **Signature `const` assertions** — arity bound, the no-mixed-classes rule,
   the Win64 XMM bound, the "exactly one stack-arg helper" pin, per-argument
   width pin, and the by-name callable ↔ alias cross-check.
6. **Accessor naming pin** — `accessor_name_matches_field(getter, field)`
   asserted per macro row, so a row pointing a getter at another field is a
   compile error.
7. **Census `const` assertions** — Function/Offset/Constant/required/optional
   counts moved from runtime-only to compile-time.
8. **Platform `const` assertions** — 8-byte aligned `usize`, one-word function
   pointers, one-word thin raw pointers, niche-optimized `Option<fn>`.
9. **`typed_helper_addr!`** — an exported macro that coerces a function through
   its declared `HelperFn*` alias before taking its address, so the producer's
   signature is checked by the compiler. Not yet used (see §4).
10. **Two new derived tests** — `accessor_reads_only_its_own_slot` (wires one
    callable slot at a time and requires exactly the matching accessor to see
    it, for all 53) and `every_accessor_is_none_on_a_zeroed_table`. Plus
    `golden_offsets_are_the_struct_offsets`,
    `abi_revision_ledger_names_the_current_table`,
    `helper_fn_sigs_record_the_real_c_signatures`,
    `slot_census_matches_the_pinned_counts`,
    `typed_helper_addr_yields_the_declared_functions_address`.
11. **Doc corrections** — the module header no longer claims the backend uses
    `[helpers + disp32]`; the `JIT_HELPERS_ABI_VERSION` doc no longer says
    "60-field, 480-byte"; `helper_field`'s "linear over 58 entries" no longer
    names a stale count.

## 3. Still unasserted, and why

| Invariant | Why not asserted here |
|---|---|
| **Each helper's Rust signature matches the emitter's hand-written argument setup** | The call sites are in `jit/`, which this crate cannot see. `HELPER_FN_SIGS` publishes the facts; the check itself must live in `jit/`. See §4.1. |
| **Each helper's Rust signature matches the function the VM stores** | The functions are in `vm/`, which this crate cannot see. `typed_helper_addr!` exists to make this a compile error at the producer. See §4.2. |
| **A `required` slot really is `CALL`ed unconditionally by the backend** | `required` is a hand-classified claim about emitter behaviour. Cross-checked between the two field lists (invariant 15), so a single-place edit trips, but both lists could be wrong the same way. Only an emitter-side audit can settle it. |
| **Every optional slot is zero-checked before its `CALL` site** | Same reason. The census pin (invariant 13) forces a human to look when the count changes, which is the best this crate can do. |
| **A helper that never returns** | No helper is `-> !` today; `returns_value` distinguishes `-> ()` from a value return, which is the live case (a call site that reserves RAX for a `-> ()` helper reads whatever the callee left). A future `-> !` helper would need a `HelperRetAbi` impl for `!` and a decision about the call site's stack. |
| **Alias name matches the field** (`HelperFnMonitorEnter` ↔ `monitor_enter`) | Not mechanically derivable from `stringify!` — snake → CamelCase needs a proc macro. The getter ↔ field pin (invariant 18) covers the same swap for the accessor, which is the reachable path. |
| **Endianness** | The backend writes immediates with explicit `to_le_bytes`, so a big-endian target would fail elsewhere first; asserting it here would be theatre. |
| **`x86_64` specifically** | `jit/src/aarch64_backend.rs` exists and AAPCS64 has 8 integer argument registers, so pinning the arch would be wrong. Only 64-bit-ness is asserted. Note that invariant 23's Win64 reasoning is x86-64-specific and the aarch64 backend does not use these helper call sites today. |

## 4. Cross-file changes this audit could not make

These are the highest-value remaining items. All are outside `jit-api/`.

### 4.1 Arm the validator (`vm/src/jit/helpers.rs`)

`build_helpers()` returns without ever validating what it built. Nothing in the
workspace calls `validate_abi`, `validate` or `null_pointers`. Add at the end of
`build_helpers`, immediately before the closing brace of the struct literal's
enclosing function:

```rust
    // Fail loudly at startup rather than as a CALL to 0 inside compiled code.
    if let Err(e) = helpers.validate_abi() {
        panic!("JIT helper table is not usable: {e}");
    }
```

This requires binding the struct literal to `let helpers = JitRuntimeHelpers {
… };` and returning `helpers`. It converts "a required slot is 0" from a wild
call much later, in a different subsystem, into a named startup panic.

### 4.2 Type-check the producer's function addresses (`vm/src/jit/helpers.rs`)

Every slot is currently populated as `jit_foo as *const () as usize`, which
type-checks against nothing. Two options, in order of preference:

**(a) One added `const` block, no change to `build_helpers` itself.** Append
near `build_helpers`:

```rust
// Every helper's real signature, checked against the ABI alias that the JIT
// side transmutes the slot to. A mismatch is a compile error here instead of a
// wrong-arity call in generated code.
const _: () = {
    use cratonvm_jit_api::helpers_abi::*;
    let _: HelperFnNewarray = jit_newarray;
    let _: HelperFnNewObject = jit_new_object;
    // … one line per callable slot, 53 total …
    let _: HelperFnMonitorEnter = jit_monitor_enter;
    let _: HelperFnMonitorExit = jit_monitor_exit;
};
```

**(b) Route each slot through the new macro**, which is the same check at the
assignment: `monitor_enter: typed_helper_addr!(HelperFnMonitorEnter,
jit_monitor_enter),`. Stronger (it cannot drift from the assignment) but a
62-line diff in a hot file.

Either way the aliases stop being documentation.

### 4.3 Check the emitter's call sites against `HELPER_FN_SIGS` (`jit/`)

The remaining gap is arity and return-ness at the ~60 hand-written call sites.
A cheap, high-yield form: in `jit/src/x64.rs`, next to `emit_call_absolute`,
add a debug-only helper that takes the slot name and the number of argument
registers the site just loaded, and compares against
`cratonvm_jit_api::helpers_abi::HELPER_FN_SIGS`. Call it from each
`emit_call_absolute(self.helpers.X)` site. A `const` version is possible where
the site is not behind runtime branches:

```rust
const _: () = assert!(helper_sig("invoke_virtual_mic").arity == 6);
```

(`helper_sig` would be a small `const fn` in `helpers_abi.rs` doing a linear
`str_eq` scan; I did not add it because there is no caller yet and an unused
const-fn reads as coverage. Say the word and it is four lines.)

Also worth pinning in `jit/src/ir_lower.rs:162-172`: `CALL_ARG_REGS` is capped
at 4 on **both** platforms with the comment "capped at 4 because the dispatch
helper only ever takes 4". `HELPERS_NEEDING_WIN64_STACK_ARGS == 1` now pins the
fact that makes that safe; a `const _: () = assert!(CALL_ARG_REGS.len() >= …)`
tying the two would close the loop.

### 4.4 `jit/tests` tables use `std::mem::zeroed()`

`jit/src/lib.rs:15691` and ~8 other sites build `JitRuntimeHelpers` with
`unsafe { std::mem::zeroed() }`, i.e. every required slot null. They then wire
only what they exercise. That is legitimate, but `validate_abi()` will reject
such a table, so §4.1's panic must not be added to a path those tests reach —
it belongs in `build_helpers` only, which the tests do not call.

## 5. Procedure for adding a helper field

Follow in order. Each step has a tripwire that fires if you skip it, so working
until the build is green is a valid way to execute this list — but read the
failure messages, because two of them are telling you not to proceed.

1. **Append** `pub <name>: usize,` at the **end** of `JitRuntimeHelpers`
   (`jit-api/src/lib.rs`). Never insert, never reorder, never remove.
2. Add `(<name>, FieldKind::…)` at the **end** of the `helper_fields!`
   invocation (`lib.rs`). *(Skipping this fails invariant 1.)*
3. Bump the two `NUM_FIELDS == 62` literals (`lib.rs`, `helpers_abi.rs`) and the
   `JIT_HELPERS_ABI_SIZE == 496` literal to the new values.
4. Append a row to `helper_field_table!` (`helpers_abi.rs`) with the right
   `HelperKind` and `required` flag. *(Skipping this fails invariant 6.)*
5. If it is callable, append a row to `helper_fn_slots!` giving the real C
   signature. *(Skipping this fails invariants 9 and 10.)* If your argument or
   return type is not already classified, add a `HelperArgAbi`/`HelperRetAbi`
   impl — and think about how the emitter will pass it.
6. Append the `(name, offset)` row to `GOLDEN_HELPER_OFFSETS`, with offset =
   previous + 8. *(Skipping this fails invariant 8.)*
7. Append a row to `ABI_REVISIONS` and set `JIT_HELPERS_ABI_VERSION` to it.
   *(Skipping this fails invariant 16.)*
8. Update the census literals (53/4/5/42/11) in the `const` block, the
   `required == 42` literal, and the counts in
   `helper_table_size_and_align_are_the_literal_abi_numbers`,
   `jit_runtime_helpers_all_fields_classified` and
   `slot_census_matches_the_pinned_counts`. **Before you change the optional
   count, confirm the emitter zero-checks the new slot; before you change the
   required count, confirm the emitter really `CALL`s it unconditionally.**
9. Append probe rows to `helper_fields_offsets_match_offset_of`
   (`helpers_abi.rs`) and `jit_runtime_helpers_repr_c_golden_offsets`
   (`lib.rs`). Update `as_words_matches_the_struct_fields`, which names the last
   slot.
10. If the slot is `RequiredPtr`, add an arm to `zero_field_by_name` in
    `lib.rs`'s tests, or the required-null sweep panics on it.
11. Wire it in `vm/src/jit/helpers.rs::build_helpers` — and, once §4.2 lands,
    through the typed form so the signature is checked.
12. If the new helper takes more than four arguments, its Win64 call site must
    write args 5+ to `[RSP+32]`, `[RSP+40]`, … and the
    `HELPERS_NEEDING_WIN64_STACK_ARGS == 1` pin will stop the build until you
    have done so and updated the literal.
