# Fix note — vm-runtime-aioobe (B1)

## Finding
B1 (HIGH-as-latent / MEDIUM live), `vm/src/jit/helpers.rs`: the JIT array
load/store runtime helpers silently swallowed an out-of-bounds index instead of
raising `ArrayIndexOutOfBoundsException`, diverging from JVMS and masking real
bugs:
- Loads (`jit_baload`, `jit_iaload`, `jit_aaload`) returned a fabricated `0` /
  `null` on `index < 0 || index >= length`.
- Void stores (`jit_bastore`, `jit_iastore`, `jit_aastore`) silently dropped the
  write on the same condition.

This is the exact silent-fabrication class the authors already fixed for the
*null* array arm in these same helpers; the OOB arm was an internal
inconsistency.

## Root cause
The bounds-check arm `return 0;` (loads) / `return;` (stores) never set the
pending-AIOOBE channel. The canonical AIOOBE-raising helper in this file,
`jit_throw_aioobe` (helpers.rs:1943), sets `JIT_PENDING_AIOOBE` to
`Some((index, length))` and returns the `i64::MIN` deopt sentinel; the
interpreter drains that flag and constructs the real
`ArrayIndexOutOfBoundsException`. The array helpers' OOB arms never participated
in that protocol.

## Exact change (file:line)
All in `vm/src/jit/helpers.rs`. Each OOB arm now mirrors `jit_throw_aioobe`:

- `jit_baload` (~1138) — OOB arm: `JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length)))); return i64::MIN;`
- `jit_iaload` (~1216) — same load protocol.
- `jit_aaload` (~1262) — same load protocol.
- `jit_bastore` (~1196) — void store: set the pending-AIOOBE flag, then `return;`.
- `jit_iastore` (~1248) — same void-store protocol.
- `jit_aastore` (~1295) — same void-store protocol; the flag is set and we
  `return` BEFORE the SATB pre-write barrier read and the element write (no
  element pointer is dereferenced on the OOB path).

The fast in-bounds path is byte-for-byte unchanged in every helper.

I used the inline `JIT_PENDING_AIOOBE.with(|e| e.set(Some((index, length))))`
form (identical to `jit_throw_aioobe` at line 1944) rather than the
`stash_jit_pending_aioobe` helper, because that helper's doc-comment scopes it
to the OSR re-stash path; the inline form is the canonical raise.

### Tests added (#[cfg(test)] in the same module)
- `alloc_test_array(et, len)` — small private helper: builds a 4 MB
  `SharedVm` (default `GcAlgorithm::Generational`, matching the existing
  `jit_newarray_under_pressure_drives_real_stw_gc` test) and `alloc_array`s a
  single array, returning the owning `Box<SharedVm>` + raw array pointer.
- `jit_iaload_oob_sets_pending_aioobe` — OOB-high and OOB-low(negative) both
  return `i64::MIN` and set `Some((index, length))`.
- `jit_iastore_oob_sets_pending_aioobe`, `jit_baload_oob_sets_pending_aioobe`,
  `jit_bastore_oob_sets_pending_aioobe`, `jit_aaload_oob_sets_pending_aioobe`,
  `jit_aastore_oob_sets_pending_aioobe` — one OOB assertion each.
- `jit_int_array_in_bounds_roundtrip_unchanged` — proves the fast path is intact:
  in-bounds `jit_iastore` then `jit_iaload` round-trips the value and leaves NO
  pending AIOOBE flag.

These mirror the existing `jit_*_null_sets_pending_npe` test style and the heap
construction in `jit_newarray_under_pressure_drives_real_stw_gc`.

## Files touched
- `vm/src/jit/helpers.rs` (6 helper OOB arms + test module additions).
- `docs/reviews/fable-2026-06-10/fixes/vm-runtime-aioobe.md` (this note).

No other files edited.

## Follow-up & risk

### IMPORTANT cross-file dependency (interpreter — NOT my file)
The void-store fix is only fully correct once the interpreter drains
`JIT_PENDING_AIOOBE` UNCONDITIONALLY on the main JIT return path, the same way
it already drains `JIT_PENDING_NPE`.

Current interpreter behavior (`vm/src/runtime/interpreter.rs`, owned by another
agent):
- `take_jit_pending_npe()` is drained **unconditionally** at ~15135 (before the
  `i64::MIN` branch), so void-store NPEs surface correctly.
- `take_jit_pending_aioobe()` in the main path (`invoke_jit_cached`) is drained
  **only inside the `if result == i64::MIN` branch** (~15152).

Consequence:
- **Loads** are fully correct now: they return `i64::MIN`, so the existing
  in-`i64::MIN`-branch drain at 15152 fires. ✅
- **Void stores** set the AIOOBE flag but return no sentinel; the main path has
  no unconditional AIOOBE drain, so a void-store AIOOBE would currently NOT be
  surfaced at that method and the flag would leak to the next JIT helper call.
  (The OSR path at ~13677 already drains/re-stashes AIOOBE unconditionally, so
  OSR-routed stores are fine; only the main `invoke_jit_cached` path is missing
  the symmetric drain.)

Recommended interpreter change (for whoever owns interpreter.rs): add an
unconditional AIOOBE drain immediately after the NPE drain at ~15148, symmetric
to the NPE block — drain `take_jit_pending_aioobe()` and route an
`ArrayIndexOutOfBoundsException` through `route_jit_exception_through_method`
(the NPE block at 15135-15148 is the exact template). I could not make this edit
because the file is owned by another agent.

### Reachability / risk
- The load helpers are currently a latent/dead fallback (x64 codegen uses an
  inline `emit_bounds_check`); `jit_bastore` is reachable via the null-check
  stub path, so its OOB arm is one codegen change from live. This fix removes
  the silent-OOB hazard for any future/non-x64 codegen path that routes through
  these helpers, with no behavior change on the in-bounds fast path.
- Low risk: edits are confined to the already-erroring OOB arms; the in-bounds
  path and the SATB write barrier in `jit_aastore` are untouched (OOB returns
  before them).
