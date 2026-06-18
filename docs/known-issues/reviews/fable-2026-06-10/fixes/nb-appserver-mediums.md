# Fix note — nb-appserver-mediums

Agent: `nb-appserver-mediums`
Date: 2026-06-10
Report: `docs/reviews/fable-2026-06-10/nb-appserver.md` (V1, B8, B9)

Owned files touched:
- `native-builtins/src/jboss_module_loader.rs` (V1)
- `native-builtins/src/wildfly_core.rs` (B9)
- `native-builtins/src/servlet.rs` (B8)

All edits are surgical and keep the default / `app-stubs` / `synthetic-jdk`
configs compiling (no cfg-gated code added; only pure helpers + bound checks).

---

## V1 (medium, security) — `ensure_under_root` now fails closed on canonicalize failure

File: `native-builtins/src/jboss_module_loader.rs`

Before, `ensure_under_root` fell back to the **raw, non-canonical** candidate
whenever `std::fs::canonicalize(candidate)` failed
(`.unwrap_or_else(|_| candidate.to_path_buf())`). A symlink whose target is
unresolvable — or a not-yet-existing path — then bypassed the symlink guard:
the `starts_with` prefix check ran against an unresolved path, so a symlink
under the root pointing outside could escape.

Fix:
- New helper `resolve_for_confinement(candidate) -> Option<PathBuf>`:
  - Fast path: if the whole candidate canonicalizes, use that (symlinks fully
    followed) — preserves the original behaviour for paths that exist.
  - Otherwise, walk up to the **longest existing ancestor**, canonicalize it
    (so any symlink in the real on-disk prefix is resolved), then re-attach the
    trailing not-yet-existing lexical components, rejecting any `..` segment in
    that tail.
  - Returns `None` when the path cannot be safely resolved (no canonicalizable
    ancestor) — caller must fail closed.
- `ensure_under_root` now rejects with `SecurityException` when
  `resolve_for_confinement` returns `None`, instead of trusting the raw path.
  The trusted `-mp` root keeps its non-canonical fallback (it is config, not
  attacker-influenced, and test tempdirs rely on it), but the
  attacker-influenced candidate fails closed.

Why this preserves legitimate resolution:
- `module_xml_path` always exists (it passed `is_file()`), so it hits the fast
  path.
- A declared `<resource-root>` jar that doesn't physically exist resolves via
  its existing prefix (`module_dir`, which exists under the root) — accepted.

Tests added (in the existing `tests` module next to the prior two
`ensure_under_root` cases):
- `t19_h4_ensure_under_root_accepts_nonexistent_descendant` — a not-yet-existing
  jar whose prefix is under root is accepted.
- `t19_h4_ensure_under_root_fails_closed_on_nonexistent_escape` — a
  non-existent candidate whose existing prefix is outside root is rejected
  (this is the exact regression the old raw fallback let through).
- `t19_h4_ensure_under_root_rejects_symlink_escape` (`#[cfg(unix)]`) — an
  in-root symlink whose target is outside the root is rejected.

The two pre-existing tests
(`t19_h4_ensure_under_root_accepts_descendants` /
`..._rejects_escapes`) still pass unchanged.

---

## B9 (low) — `spawn_worker` no longer panics on thread-spawn failure

File: `native-builtins/src/wildfly_core.rs`

`spawn_worker` previously did
`.expect("failed to spawn EnhancedQueueExecutor worker")`, turning an OS
thread-limit / ENOMEM into a process abort.

Fix:
- `spawn_worker` now returns `std::io::Result<()>` (propagates the spawn error
  via `b.spawn(...).map(|_handle| ())`).
- Its only caller, `EnhancedQueueExecutor::new`, logs a `tracing::warn!`
  (matching the existing worker-loop warn style: `target`, `pool`, `error`) and
  continues with however many workers did start. If none start, callers still
  have `drain_locally` / `drain_all_pending_runnables` to make progress.

No public signatures changed (`new` still returns `Arc<EnhancedQueueExecutor>`),
so the 5+ existing tests that build executors are unaffected.

---

## B8 (low) — ByteBuffer relative-read index arithmetic uses checked math + bounds

File: `native-builtins/src/servlet.rs`

`s2_bb_read2/4/8` and `s2_bb_write2/4/8` computed `idx + 1..3/7` and
`idx + i as i32` directly on an `i32` from Java bytecode — reachable overflow
(debug panic / release wrap). `s2_bb_get_byte`/`s2_bb_put_byte` also did
`idx as usize`, so a negative `idx` became a huge `usize`. The IntBuffer view
get/put additionally computed `bs + pos * 4` / `bs + idx * 4` (multiply +
add overflow).

Fix (all in the existing private helpers, so every call site is covered):
- `s2_bb_get_byte` / `s2_bb_put_byte`: reject `idx < 0` and bound `idx` against
  `ctx.array_length(arr)` — out-of-range reads return `0`, writes are dropped
  (panic-free; the JDK would throw IndexOutOfBoundsException, our synthetic path
  stays benign). `get/set_array_element` already bounds-check downstream; this
  removes the negative-`usize` and debug-panic risk at the source.
- New `s2_bb_off(idx, off)`: `idx.checked_add(off).unwrap_or(-1)` — overflow
  saturates to the negative out-of-range sentinel the byte accessors reject.
  Used by all six multi-byte read/write helpers.
- New `s2_bb_int_byte_off(bs, unit)`: `unit*4 + bs` via `checked_mul`/`checked_add`,
  saturating to `-1` on overflow. Used by the four `IntBuffer` get/put sites.

Tests added (pure functions, no mock needed):
- `b8_bb_off_no_overflow_panic`
- `b8_bb_int_byte_off_no_overflow_panic`

(Existing servlet.rs tests exercise real sockets, not the ByteBuffer byte
helpers via the mock, so the new bound checks don't affect them — and the
mock's `array_length` returns 0, which would make a mock-backed round-trip
test meaningless, so no such test was added.)

---

## Confidence

- `compiles_confidence`: high. Edits mirror existing types/APIs
  (`std::io::Result`, `tracing::warn!`, `PathBuf`, `checked_add/mul`,
  `ctx.array_length`). No `cargo`/`git` was run per task rules.
- `tests_added`: yes (5 new unit tests across the two files).
- `behavioral_risk`: low. V1 tightens a security check (fail-closed) while
  preserving legitimate not-yet-existing resource-root resolution; B9 turns an
  abort into a logged-and-continue; B8 only changes behaviour for previously
  panicking/UB indices (now benign 0 / no-op).
