# Wiring the bytecode loop transform into `compile_with_param_slots`

> **SUPERSEDED — historical.** Everything this file lists as "not wired" has
> been wired since, and its census of what would need translating is wrong in
> one place (`OopMapEntry::bytecode_pc` must *not* be translated). Read
> [`loop-rewriter-wiring.md`](loop-rewriter-wiring.md) instead; it records the
> state of the wiring as built, including guarded versioning. This file is kept
> because it is where the acceptance criterion was set out, and because two
> other docs cite it.

Companion to [`loop-transforms.md`](loop-transforms.md), which describes the
transform itself. This file records what was wired **at the time it was
written**, and what a future change had to do before the transform was allowed
to rewrite the bytecode the emitter compiles.

Status as of this change:

| Piece | State |
|---|---|
| `plan_loop_unroll` as the native unroller's **admission oracle** | wired (`x64.rs`, `plan_native_unroll`) |
| `bypassable_headers` consulted by the native unroller | wired |
| Mutual exclusion of the two unrollers | wired (`bytecode_loop_xform_rewrites_bytecode` / `native_unroller_enabled`) |
| Compiling `xform.code` instead of `code` | **not wired** |
| `bci_at` at deopt / oop-map record sites | **not wired** (vacuous today — see below) |
| `osr_entry_pc` at the OSR entry site | **not wired** (vacuous today — see below) |

`loop-transforms.md`'s "Wiring (not done)" section describes rows 4–6 only; the
first three rows are now done and that section is stale to that extent.

## What is wired: the transform as an admission oracle

`x64.rs::plan_native_unroll` gates every entry that reaches
`compiler.unroll_loops`, which is what the `0xa7` arm of `compile_bytecode`
treats as proof that duplicating a body's *machine code* is sound.

It refuses when

* the header is in `bypassable_headers` (an external branch reaches the header,
  or a handler range entering the body escapes the loop) — the same filter the
  aaload / arith / FP LICM hoists, matrix-dot and the bulk-byte loops all
  apply, and which the unroller previously ignored even though it sits on the
  same pre-header: `pc_to_native[header]` points *past* it, so `body_start`
  excludes it and none of the copies runs it; **or**
* `plan_loop_unroll` refuses, for any of its 17 reasons.

The rewritten bytes are discarded. Only the verdict is used, so the emitter
still compiles the caller's bytecode and nothing is re-keyed. That is why rows
5 and 6 above are *vacuous* rather than *missing*: every pc the emitter handles
is already an interpreter bci, so `bci_at` would be the identity and
`osr_entry_pc` would be the identity.

The old gate was `code[back_edge] == 0xa7` plus a body-byte-size band. It had
no reducibility test, no single-entry test, no inner-cycle test, no
branch-to-back-edge test, no handler-containment test, no `bypassable_headers`
consult and no poll-free-span budget.

Cost: one `MethodCfg::build` + dominator fixpoint per *candidate* loop (only
loops that already passed the profitability band), plus one `Vec<u8>` the size
of the rewritten method, which is discarded. Bounded by
`LOOP_XFORM_MAX_BODY_BYTES` (256) and `LOOP_XFORM_MAX_COPIES` (7).

## What is *not* wired, and why it is not half-wired

Compiling `xform.code` is not a local change, because
`compile_with_param_slots` receives ~15 caller-owned side tables keyed by
**bytecode pc**, and duplicating a body duplicates every site inside it:

```
multianewarray_info  field_info        typecheck_info     static_field_info
new_info             new_deferred_info anewarray_info     anewarray_deferred_info
invoke_info          direct_calls      mic_slots          pic_slots
ldc_info             ldc_string_info   ldc2w_info         indy_info
branch_hints         loop_unroll_hints non_escaping_new   inline_sites
compact_field_info   protected_ranges  exception_ranges
```

Re-keying them needs, per table: entries at `pc >= back_edge_end` shifted by
`copies * body_len`, and entries inside `[header, back_edge_end)` **replicated**
once per copy. Three of them (`invoke_info`, `mic_slots`, `pic_slots`) carry raw
pointers to per-call-site slots that `jit/src/lib.rs::try_compile` allocates and
owns; a replicated site either shares one inline-cache slot across copies (the
per-iteration collision the native unroller mints fresh slots to avoid) or needs
new slots minted by code that lives outside this file. `inline_sites`
additionally feeds `inline_stack_reserve`, so replication changes the frame size.

A table missed in that sweep is silent: the copy's site simply has no metadata,
and the emitter takes whatever fallback that table's absence selects. That is
the "half-wired is worse than unwired" case, so none of it was attempted here.

## The acceptance criterion, if the rewrite is ever wired

`bci_of` is total over the output and is **not** the identity — output PCs run
past the end of the interpreter's method — so anything that records a pc as a
bci records something the interpreter cannot resume at. Every one of these must
go through `LoopXform::bci_at`:

* `Compiler::deopt_stubs` — 24 `push` sites in `x64.rs`, tuple
  `(patch_offset, bci, reason)`. The bci is consumed by `emit_deopt_stubs` and
  passed to `jit_uncommon_trap(vm, reason, bci)`.
* `OopMapEntry::bytecode_pc` — one `oop_maps.push` site. The GC root walker
  matches the running safepoint back to its precise map by this value.
* `Compiler::bounds_check_stubs`, `exception_check_stubs`,
  `null_check_store_stubs` — each entry pairs a native patch offset with a bci.
* `deopt_box_ptr_by_bci`, `exc_frame_box_ptr_by_bci`, `osr_exit_box_ptr_by_bci`
  — `FxHashMap` keyed by bci.
* The deopt frame snapshot's `bci: bci as u32` fields.
* `deopt_eager_bci`, `osr_exit_test_trigger_bci`, and the
  `DespecRegistry::contains` consult on `LoopHoist::loop_header` — all
  bci-valued.
* `pc_to_native` and `osr_entry_native` are **indexed** by bci and published as
  `CompiledMethod::osr_pc_to_native`, which the runtime indexes with an
  interpreter bci. These cannot merely be translated on read: they must be
  built in original-bci space, with `osr_entry_native[bci]` set from
  `LoopXform::osr_entry_pc(bci)`'s native offset.

`osr_entry_pc` is load-bearing and not an optimisation: entering a **peeled**
copy re-runs the peeled iterations, so the loop executes `k` times too many.
It always answers with the steady-state copy.

The mutual-exclusion switch is `x64.rs::bytecode_loop_xform_rewrites_bytecode`.
`native_unroller_enabled` is its complement, so flipping it turns the native
byte-copy unroller off in the same motion. If both ever ran, `k+1` bytecode
copies would be machine-code-duplicated `k+1` more times behind one back-edge
poll — `(k+1)^2` bodies per poll, a time-to-safepoint neither unroller's budget
check ever saw.

## Known gap in `LoopXform::osr_entry_pc` (owner: `jit/src/x64/licm.rs`)

`jit/src/x64/licm.rs:3336` `osr_entry_pc` answers `Some(bci + steady *
body_len)` for every bci in `[header, back_edge_end)`. For `LoopXformKind::Unroll`
`steady` is `0`, so the **back-edge bci itself** answers `Some(back_edge)` — but
the back edge exists in the *last* copy only, and output pc `back_edge` is the
first byte of copy 1, i.e. the header. So

```
unroll.osr_entry_pc(back_edge) == Some(back_edge)
unroll.bci_at(back_edge)       == Some(header)     // not back_edge
```

An OSR entry there would resume with the interpreter's frame for `goto` at the
top of a fresh body copy. `Peel` has no such gap: its steady-state copy carries
the back edge, and the round trip holds.

Required edit (out of scope for this change, `licm.rs` is owned elsewhere):
`osr_entry_pc` should return `None` for
`bci in back_edge..orig_back_edge_end` when `kind == Unroll` — there is no
steady-state image of those bytes. The x64 test
`loop_unroll_admission::osr_entry_lands_on_the_steady_state_copy_not_a_peeled_prefix`
asserts the current behaviour explicitly so the gap stays visible; that
assertion must be updated when the refusal lands.
