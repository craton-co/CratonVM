# Fix note — jit-lentables

Agent id: `jit-lentables`
Report: `docs/reviews/fable-2026-06-10/jit.md` (findings B3, S1)

## Finding

Two non-x64.rs JIT items left by Round 1:

- **B3 (LOW, latent):** `bc_len` in `jit/src/regalloc.rs` has no arm for the
  `wide` prefix opcode `0xc4`. It falls through to `_ => 1`, which would desync
  every PC-stepping consumer (CFG build, branch-target precompute, the neon
  scanner, etc.) after a `wide`. Currently latent because `jit_scan` rejects
  `wide` so no compiled method contains it — but the length table should be
  correct as defense-in-depth.

- **S1 (stub/dead code):** `jit/src/aarch64_backend.rs::detect_neon_patterns`
  pushed `IntArraySum` / `IntDotProduct` patterns with hardcoded placeholder
  locals (`array_local: 0, accum_local: 0, counter_local: 0`; comment said
  "placeholder — full analysis would resolve these"). It was only ever called
  from its own two unit tests and was never wired into any codegen path. If it
  were ever consumed it would vectorize against the wrong locals (a miscompile).
  aarch64 is not the production backend.

## Root cause

- B3: incomplete hand-maintained bytecode length table — the `wide` form (and
  its `wide iinc` special case) was simply never added.
- S1: half-finished analysis committed as dead code; the operand resolution that
  would fill in the real local indices was never written, and the function was
  left disconnected from codegen.

## Exact change

### `jit/src/regalloc.rs` — `bc_len`

Added a `0xc4` arm before the `_ => 1` fallback, per JVMS §6.5 *wide*:

- `wide <load/store/ret> <indexbyte1> <indexbyte2>` = **4 bytes**.
- `wide iinc <indexbyte1> <indexbyte2> <constbyte1> <constbyte2>` = **6 bytes**.

The modified opcode is the byte at `pc + 1`; only `iinc` (`0x84`) takes the
6-byte form. The arm bounds-checks `pc + 1 < code.len()` before reading the
modified opcode and defaults to the 4-byte form otherwise (consistent with the
rest of the table's defensive style, e.g. the switch arms' truncated-header
fall-backs to length 1). A comment records that this is currently latent and
that the x64.rs `bytecode_len_at` twin must be kept in lockstep (see follow-up).

### `jit/src/aarch64_backend.rs` — `detect_neon_patterns` / `NeonVectorizablePattern`

Deleted (per the report's stated preference, since nothing non-test referenced
them):

- the `NeonVectorizablePattern` enum,
- the `detect_neon_patterns` function (with its placeholder-local pattern
  pushes),
- the two unit tests that were its only consumers
  (`p95_neon_pattern_detection_array_sum`,
  `p95_neon_pattern_detection_no_pattern`).

Left a short removal note at each site (the former definition location and the
former test location) explaining why it was removed and pointing at the real
x64 operand-resolution helpers to reuse if NEON auto-vectorization is ever
pursued. The unrelated `p95_neon_machine_code_emission` test and all
`Arm64Instruction::Neon*` emitters are untouched.

Verified by grep that no other code in the repo references the deleted symbols
(only doc/review files and the new comments mention the names).

## Files touched

- `jit/src/regalloc.rs` — added `0xc4` (`wide`) arm to `bc_len`.
- `jit/src/aarch64_backend.rs` — deleted dead `detect_neon_patterns` /
  `NeonVectorizablePattern` and their two tests; left removal notes.
- `docs/reviews/fable-2026-06-10/fixes/jit-lentables.md` — this note.

## Tests added

None. Deletion-only for S1; B3 is a latent-path defense-in-depth correction with
no reachable trigger (no method containing `wide` is JIT-compiled, so a
behavioral test cannot exercise the new arm without first relaxing `jit_scan`,
which is out of scope and owned elsewhere). The report itself recommends a
`jit_scan`-reject lock-in test for `wide` (its Tests item 5), which belongs with
the x64.rs/jit_scan owner.

## Follow-up & risk

- **Lockstep follow-up (out of my owned files):** the x64.rs twin
  `bytecode_len_at` (`jit/src/x64.rs:1601`) is owned by no one this round and
  still lacks a `0xc4` arm. It must get the identical `wide` length logic before
  `wide` is ever added to `jit_scan`, otherwise the two tables drift again.
  `find_modified_locals` and `find_induction_variable` in x64.rs also do not
  decode the `wide` prefix. The report's feature suggestion #2 (centralize the
  three length tables into one shared `pub(crate) fn`) would eliminate this drift
  class entirely.
- **Risk:** very low. The `bc_len` change only adds a new match arm for an opcode
  that is currently unreachable in compiled code, so it cannot change behavior of
  any method that is JIT-compiled today; it only makes the table correct if/when
  `wide` is later accepted. The aarch64 deletion removes dead, test-only code on
  a non-production backend and was confirmed to have no remaining references, so
  the crate still compiles.
