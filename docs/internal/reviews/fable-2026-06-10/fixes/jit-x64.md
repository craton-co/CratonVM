# Fix note — jit-x64

All changes in `jit/src/x64.rs`. No changes were needed in `jit/src/regalloc.rs`
or `jit/src/aarch64_backend.rs` (their findings — B3, S1, P2–P4 — were not in this
fixer's task list and are left as follow-ups).

## Finding B1 / V1 (HIGH, memory safety) — BCE off-by-one for inclusive-comparison loops

`analyze_loop_bound` accepted the inclusive comparators `if_icmpgt` (0xa3, exit)
and `if_icmple` (0xa4, continue) but `LoopBoundsInfo` recorded only `bound_local`,
not inclusivity. The speculative header guard (`CMP array.length, bound; JB deopt`)
proves only `array.length >= bound`, while an inclusive loop reaches `index == bound`
(max index `bound`, needing `array.length >= bound + 1`). The per-element check was
then elided in `emit_bounds_check`, producing a one-past-the-end heap read/write
(an OOB store for `iastore`/`aastore`) — reachable from ordinary javac output for
`for (i = 0; i <= n; i++) a[i] = …`.

### Root cause
Loss of the comparator's inclusivity between `analyze_loop_bound` and the
elision/guard logic; the single header range guard is sound only for the exclusive
(`<` / `>=`) forms.

### Exact change
- Added an `inclusive: bool` field to `struct LoopBoundsInfo` (`x64.rs:3896`).
- `analyze_loop_bound`: set `inclusive: cmp_op == 0xa3` for Pattern A (exit) and
  `inclusive: cmp_op == 0xa4` for Pattern B (continue) at both `LoopBoundsInfo`
  literals (`x64.rs:4138`, `x64.rs:4152`).
- `find_safe_array_accesses` (static path): early-return an empty set when
  `bounds.inclusive` is true (`x64.rs:4380`) — the access keeps its per-element
  check.
- `analyze_bounds_elimination` (speculative path): `bound_invariant` now begins
  with `!bounds.inclusive && …` (`x64.rs:4503`), so no speculative guard is
  installed and no PC is added to `safe_pcs` for inclusive loops.

Chosen remediation = the report's option (b) "refuse to mark such accesses safe":
provably sound, no new guard-encoding/overflow logic, and zero behavior change for
the already-tested exclusive `if_icmpge`/`if_icmplt` cases (`inclusive == false`).

## Finding B2 (MEDIUM) — `find_modified_locals` unclamped `1u64 << local`

Wide `istore..astore` (0x36..0x3a) and `iinc` (0x84) carry a raw local index 0..255;
`1u64 << n` panics in debug for `n >= 64` and masks to `n % 64` in release (wrong
modified-bit, which could defeat the BCE invariance check and re-open B1).

### Root cause
Two shift sites in `find_modified_locals` lacked the `.min(63)` clamp every other
shift-by-local site already uses.

### Exact change
- `find_modified_locals`: both `modified |= 1 << code[pc + 1]` replaced with
  `modified |= 1u64 << (code[pc + 1] as usize).min(63)` (`x64.rs:3384`, `x64.rs:3391`).
  A high local now saturates to bit 63, which is conservatively treated as "some
  high local modified" — so the existing `bl < 64` / `al < 64` guards in
  `find_safe_array_accesses` / `find_speculative_array_accesses` keep refusing BCE
  for any access whose bound/array local is ≥ 64, closing the B1 re-open path.

## Finding B4 (LOW) — `assert!` panics in `emit_safe_idiv`

Three rel8-range `assert!`s panicked on an out-of-range displacement, violating the
crate's "never panic, bail to interpreter" contract.

### Root cause
`emit_safe_idiv` returns `()` and could not previously signal failure, so it asserted.

### Exact change
- Replaced the two JNE asserts with a combined range check that calls
  `self.buf.mark_overflowed(); return;` (`x64.rs:11257`).
- Replaced the JMP assert with the same `mark_overflowed(); return;` pattern
  (`x64.rs:11301`). This mirrors the existing no-panic bail used in
  `patch_branches`/`patch_self_calls` (`x64.rs:19190` etc.); the driver's
  `if buf.overflowed() { return None; }` discards the half-emitted method.

## Performance (P5 + P1)

- P5: cached `CRATONVM_JIT_NO_SPEC_BCE` in a new `OnceLock<bool>` helper
  `jit_no_spec_bce()` (`x64.rs:3879`), modeled on `precise_jit_maps_enabled`;
  `analyze_bounds_elimination` now calls it instead of `std::env::var_os` per loop
  (`x64.rs:4512`).
- P1: added `speculative_bce_guards_by_header: FxHashMap<usize, Vec<SpeculativeBCEGuard>>`
  to the compiler struct (`x64.rs:4773`), built once in `compile` right after the
  `bounds_safe_pcs` assignment (`x64.rs:19672`). The per-loop-header emit loop now
  does an O(1) `get(&pc).cloned().unwrap_or_default()` (`x64.rs:12160` region)
  instead of `iter().filter(|g| g.loop_header == pc).cloned().collect()`. The flat
  `speculative_bce_guards` vec is retained unchanged (still used elsewhere).

## Files touched
- `jit/src/x64.rs`

## Tests added (inline `#[cfg(test)]`, modeled on `test_bounds_elimination_analysis`)
- `test_bounds_elimination_inclusive_not_safe`: a `for (i=0;i<=n;i++) arr[i]` loop
  (`if_icmpgt` exit). Asserts `analyze_loop_bound(...).inclusive == true`, the
  `iaload` PC is NOT in `safe_pcs`, and no speculative guard is produced. Directly
  covers B1/V1.
- `test_find_modified_locals_high_local_no_panic`: a wide `istore 200` + `iinc 200, 1`
  body; asserts `find_modified_locals` returns exactly `1u64 << 63` (no panic, no
  wrong low bit). Covers B2.

## Follow-up & risk
- Risk is low: the inclusive fix only *removes* elisions (conservative — keeps a
  check that was being wrongly dropped). Exclusive `if_icmpge`/`if_icmplt` paths are
  byte-identical (`inclusive == false`). The two existing BCE tests
  (`test_bounds_elimination_analysis`, `test_bounds_elimination_javac_pattern`)
  remain valid because their `LoopBoundsInfo` is now constructed with
  `inclusive: false`.
- Possible widening (deferred, not done here): instead of refusing inclusive loops,
  emit the speculative guard against `bound + 1` with an overflow check — would
  restore BCE for `<=` loops. Left out to keep the change minimal and avoid
  untested guard-encoding logic.
- Not addressed (outside this fixer's owned-file fix list): B3 (`wide`/0xc4 missing
  from `bytecode_len_at` in x64.rs and `bc_len` in regalloc.rs — latent, `jit_scan`
  rejects `wide`), S1 (dead `detect_neon_patterns` stub in aarch64_backend.rs),
  P2/P3/P4 (linear `dup2` metadata scans, O(n²) `canonicalize_stack`, `Vec<bool>`
  branch-target map).
