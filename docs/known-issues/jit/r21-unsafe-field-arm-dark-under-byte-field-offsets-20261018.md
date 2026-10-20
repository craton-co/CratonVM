# The compiled `Unsafe` instance-field arm has been dark since byte field offsets went default ON

Status: OPEN -- fix built behind an opt-in switch (JIT round 21 wave 1, lane updaters), pending the
orchestrator's build, probe run and A/B. Flip both switches default ON (as a `WARNING:` commit if the
user wants that path) once `R21UpdaterRows` passes and the rows below move.
Area: `vm/src/jit/helpers.rs` (the `DirectHelperTable` fill, `table.unsafe_field_cells`),
`jit/src/direct_helpers.rs` (`unsafe_inline_cell_screen_bytes`, the screen both tiers emit),
`vm/src/vm/vm_exec.rs` (`hw_atomic_addr`, the only cell publisher).
Found by: lane updaters, attributing the r20 handoff rows "field-updater get / set 131-133 ns vs
HotSpot 0-1" and "getAndIncrement / getAndSet / getAndAdd 149-153 ns vs 5".

## What happened

nio21 wave 7 L3 built the instance-field arm of the `Unsafe` prefixes: a per-VM cell table
`(class id, slot << 1 | wide) -> compact displacement`, published by `hw_atomic_addr` and read by a
screen that keys the cell by the raw `Unsafe` offset as a slot index. JIT round 20 wave 11 / 12 gave
it the volatile get / put rows (AT-3, `CRATONVM_JIT_UNSAFE_VOLATILE_FIELD_INLINE`) and the reference
CAS row (AT-4, `CRATONVM_JIT_UNSAFE_QU_REF_CAS_FIELD`).

JIT round 20 wave 8c made `objectFieldOffset` answer HotSpot byte offsets (`16 + slot * 8`) under
`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`, and, because the screen keyed by the raw offset, the VM stopped
baking the table under that switch (`table.unsafe_field_cells = 0`, comment "not under
`CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`"). That was correct while the switch was opt-in. `b0dc8fb375`
(2026-10-12 22:18) flipped it default ON, so from then on **no compile has a field arm**: every
field-updater `get` / `set` / `lazySet` / `compareAndSet` / `getAndAdd` / `getAndSet`, AQS `state`,
`LongAdder` cell and `ConcurrentHashMap` `sizeCtl` access takes its thin call or the funnel again, and
`CRATONVM_JIT_UNSAFE_VOLATILE_FIELD_INLINE` / `_QU_REF_CAS_FIELD` / `_FIELD_RMW_INLINE` are inert by
default. No answer is wrong; nothing logs it.

The w11d A/B that kept AT-3 ON (ifu-get 131 vs 276 ns) ran BEFORE the flip. Wave 17's
`CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE` row (updater get 173 -> 69, getAndInc 347 -> 137 ns) ran
after it, so its 69 / 137 ns already pay the thin `getIntVolatile` call and the compiled
`Unsafe.getAndAddInt` Java body (kept a call by `CRATONVM_JIT_UNSAFE_RMW_BODY_NO_SPLICE`, whose veto
does not ask whether a field arm exists) with its thin get + CAS loop.

A second, smaller gap: a cell is published only from `hw_atomic_addr`, i.e. from a CAS or a fetch-add
on the field. A field that is only read and written through `Unsafe` (an updater used for `get` /
`set` only) never gets a cell even with the table baked, so its get / put arm always declines.

## The fix (built, opt-in)

* `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1`: a byte-offset VM bakes the table with bit 0 set
  (`cratonvm_jit::direct_helpers::UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG`; the table is a boxed `u64`
  slice, so the bit is never address). `unsafe_inline_cell_screen_bytes` then emits `SUB R11, 16 ;
  TEST R11B, 7 ; JNZ decline ; SHR R11, 3` before the unchanged screen (immediates untagged). An
  offset below 16 wraps and fails the `CMP R11, 0x7FFF ; JA`; so do the synthetic band and the
  `Buffer.address` sentinel; `k != 0` (an offset inside a field) declines to the natives, which serve
  the bytes. Both tiers and all three arms (volatile get / put, int/long RMW, reference CAS) share the
  function. No `DirectHelperTable` shape change.
* `CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1`: the volatile get / put thin helpers
  (`jit_unsafe_get_volatile_body`, `jit_unsafe_put_volatile_body`, int / long only) call
  `vm_exec::note_unsafe_volatile_field_cell`, which runs `hw_atomic_addr` for its guards and its
  publication and discards the address. The switch is read once per compile in
  `UnsafeInlineCells::base_addr_for_compile` into a per-VM `AtomicBool`; the helpers pay one relaxed
  load when it is off.

Soundness is the existing atomicity-domain audit (`direct_helpers::unsafe_volatile_field_arm_words`,
updated): the screen admits only `16 + slot * 8`, which every native decodes to `(slot, 0)` and serves
by `get/set_field_volatile(obj, slot)` / `hw_atomic_addr(obj, slot)`, the same bytes the cell names.

Tests: `cargo test -p cratonvm-jit --lib r21_updaters_` (screen bytes with and without the tag, both
halves; the decode against the encode).

## How to verify

Probe `tools/probes/jit/r21/R21UpdaterRows.java` (run lines in its header). Arms, interleaved:

1. default (`--jdk-only`, byte offsets ON, field arm dark);
2. `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1`;
3. `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0` (the arm as w11d had it), control;
4. arm 2 with `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0` (kill: the table is not baked, rows = arm 1).

Expected: every `fact` line equals HotSpot 25 in every arm and in `--compatible` / `--nojit`; `time
ifu-* / lfu-*` rows of arm 2 well below arm 1 and near arm 3; `rfu-cas` moves (reference CAS arm,
Generational / G1 only); `rfu-get` / `rfu-set` do not (no reference get / put arm, UPDATERS21-1).
Engagement: `CRATONVM_DBG_JITC=1` prints `ir-unsafe-rmw-inline ... field_arm=true` and `... volatile
field arm` in arm 2, `field_arm=false` in arm 1. Also run `Nio21CasRace -Dnio21.rows=bench`
(`ufcas ufxadd lfcas ifu lfu`) and `R20PAtomVolatileField` / `R20PAtomRmwNoSplice` if still in the kit.

## If the rows still do not reach HotSpot

What remains per call after the arm hits is the updater's own call structure: the user's
`invokevirtual Atomic*FieldUpdater.get` must splice `…Impl.get` (static-final exact-class proof,
`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`), which nests `accessCheck` (private) whose cold
`throwAccessCheckException(obj)` is a private call that is never run, hence never compiled, hence
possibly unbindable (`ir_splice_body_has_unbindable_call`) -- which would keep `accessCheck` a call.
`CRATONVM_DBG_JITC=1` on `-Dr21.rows=bench` answers it: look for `inline-resolve REFUSED
…AtomicIntegerFieldUpdaterImpl.accessCheck` and for `ir-splice-unbindable-surviving-call`. Proposal
UPDATERS21-3 in `docs/internal/proposals/jit/jit-r21-updaters-proposals.md` has the next step.
