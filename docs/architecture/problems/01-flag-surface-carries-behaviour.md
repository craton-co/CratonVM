# Problem 1: 6,126 switches carry the VM's behaviour

Filed 2026-11-11 from code analysis only. Measured on the tree at `39e1145b06` (`dev` of 2026-11-01). Counts are regex counts over `*.rs`
and are approximate.

## Problem

Behaviour is selected by environment switches, not by code. A fix lands as a
new switch plus a new arm, and the old arm stays. The VM that ships is one
point in a space nobody can enumerate or test.

## Evidence

- `types/src/flag_groups.rs` `INVENTORY` holds **6,126 rows**: JIT 2,809,
  GC 1,221, COMPAT 929, DBG 707, LOADER 246, THREADS 127, IO 34, REAL 30,
  SECURITY 14, TEST 9.
- The header of that file says it was written to replace a surface of 692
  identifiers. It is now nine times that.
- Growth by the rows' own `since` field: 326 in 2026-08, 1,408 in 2026-09,
  3,341 in 2026-10. 204 rows are dated after today (2026-12, 2027-01), so the
  field is not reliable as an audit trail.
- Reads are string-keyed and scattered: **5,237** `runtime_flag_on` /
  `runtime_flag_default_on*` call sites and 1,107 `runtime_var*` call sites,
  against 177 typed `VmFlags` fields read at 185 `flags().` sites.
- Each string read is a set lookup plus a map lookup
  (`types/src/flags.rs:7075`, `runtime_var_os`). Hot sites therefore add their
  own `OnceLock` memo, and `vm/src/runtime/env_cache.rs` memoises about 90
  more. `flags.rs:6695` (`overrides_active`) documents that these memo layers
  cannot see a test override unless each one cooperates.
- The snapshot is a process global (`FLAGS`, `PROCESS_OVERRIDE` in
  `flags.rs`). Two VMs in one process cannot differ in any switch.
- The `DBG` group is documented as "cannot change a program's result" and the
  same comment lists three members that do (`flag_groups.rs`, `Group::DBG`).
- 929 `COMPAT` rows are per-application workarounds (see problem 4).

## Why it is architectural

Every other problem in this folder is harder because of this one. A
default-on flip leaves the off arm in the tree, so code size, review cost and
the number of reachable states only grow. No test matrix covers 2,809 JIT
switches, so "green" means "green at the defaults".

## Plan

1. **Ratchet.** In `types/tests/flag_declaration_guard.rs`, fail when the row
   count exceeds a number checked into the test. Adding a row requires
   lowering another. One assert.
2. **Classify.** Add a `kind` to `E`: `Diag` (prints only), `Rollback`
   (default-on opt-out kept for a revert window), `Tunable` (a value a user
   may set). Fill it mechanically from `on_key` / `off_key` / `off_word`.
3. **Expire rollbacks.** A `Rollback` row older than 30 days fails the guard
   test. The fix is to delete the row and the off arm it guards. Work the
   backlog group by group, JIT first, oldest first.
4. **Typed reads.** Generate a `FlagId` enum from `INVENTORY` and make the
   read `flags().on(FlagId::X)`, an array index. This removes the per-site
   `OnceLock` memos and `env_cache.rs`, and makes an unread flag a compile
   error once its last reader is deleted.
5. **Per-VM.** Hang the snapshot off `VmConfig` (problem 2) so an embedder can
   run two VMs with different settings.

## Done when

- Non-`Diag` rows are under 300 and the ratchet is at that number.
- No `runtime_flag_*("CRATONVM_...")` string reads remain outside `types`.
- `env_cache.rs` is deleted.
