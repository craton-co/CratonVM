# Proposal: a frame carries the pool generation its door validated, not the count at its push

**Status: proposal — filed 2026-10-04 by interpreter round i1 wave 40, lane
L3, from
`docs/internal/fixed-bugs/interpreter-L2-a-frame-pushed-inside-a-constant-only-redefinition-reads-the-new-constant-RETIRED-20261010.md`.
Built in wave 43 (lane L2) for frames whose cached body names its generation,
read at the conversion rather than stamped at the install; see "Progress
(wave 43)".**

## Progress (wave 43) — lane L2: built, with the generation read at the conversion instead of stamped at the install

Commits on `claude/i43-L2`: `89602d260` (the field), `9f0d5c190`
(`CROSS-LANE:` the invoke-cache fills name it), `2baa4281a` (`CROSS-LANE:`
`convert_frames` reads it, switch off) and the last commit of the branch
(the switch on).

**What was built.**

* `CachedBytecodeMethod::pool_generation` (jit-api): the class-redefinition
  count its producer read under the class-manager guard it copied `code`
  under; `POOL_GENERATION_UNKNOWN` (`u64::MAX`) from `from_parts` and every
  hand-built literal (tests), `with_pool_generation` to name it,
  `known_pool_generation` to read it. `CachedMethodParts` is unchanged, so
  the producers' literals were not touched; the ~40 hand-built
  `CachedBytecodeMethod` literals (jit-api, jit, jit tests, two vm tests) say
  unknown.
* Producers that name it, each reading the count under the guard of its
  copy: `populate_static_invoke_cache` (`dispatch_static.rs`),
  `populate_virtual_invoke_cache` (`dispatch_virtual.rs`), the lambda site's
  cached template (`lambda.rs`) and the JIT's handler template
  (`helpers.rs::resolve_callee_uncached`). The vtable snapshots
  (`vtable.rs`, built from loader-captured descriptors, and re-installed by
  the redefining thread inside the swap), the deoptimization rebuilds
  (restamped with their compile stamp) and the trap sinks in `execute` say
  unknown, and keep today's rules.
* `obsolete_frames::body_generation_before(frame, began)`: the generation of
  a CACHED frame's body when it is older than the class's last swap and the
  frame was not moved yet. `convert_frames` uses it where the stamp and the
  bytes cannot tell: a frame stamped inside the swap with the current bytes
  (was: taken for the new body) and a frame stamped after it with them
  (window 3 with the bytes alike) are translated from the body's own
  generation, falling back to today's restamp when the history no longer
  reaches it. `frame_runs_replaced_code` (the OSR door) says such a frame
  runs replaced code. Behind `FRAMES_TRUST_THEIR_BODYS_POOL_GENERATION`,
  switched on in its own last commit. Unit test
  `obsolete_frames::tests::a_frame_pushed_inside_a_constant_only_swap_runs_the_body_its_door_chose`
  (a cached frame read before a constant-only redefinition and stamped
  inside its swap returns 70000 with the switch on, 80000 off; one whose
  entry names no generation returns 80000 either way).

**Why not at the frame install.** Stamping `pool_generation` into
`Frame::redefine_stamp` at the cached installs (the proposal's second
bullet) would make the stamp the time the invoke-cache ENTRY was filled, not
the time the activation began, and two readers need the latter:

* `jit_bridge::osr_body_may_fold_code_the_frame_predates` refuses an OSR
  entry when a class the body copied was redefined after the frame's stamp.
  Entries survive redefinitions of other classes (the per-thread cache
  retires only redefined callers), so every frame pushed from an entry
  filled before an agent redefined some inlined callee would be refused OSR
  until the entry is refilled -- for good, in a Mockito-style process.
* `obsolete_frames::convert_thawed_frames` walks the redefinition ring from
  the oldest frame's stamp at every continuation remount; stamps as old as
  the entries would make that walk (or, past the ring's 256 counts, a full
  conversion pass under the class-manager lock) the common case once any
  class was redefined.

Reading the body's generation where the ambiguity is decided costs the call
path nothing (the install is unchanged) and each fill one atomic load.

**What remains.** Frames without a cached body that names a generation:
owned-metadata frames (`Frame::new_from_arcs`, the launcher's `main`),
frames pushed from a vtable snapshot, and deoptimization rebuilds. Which of
the five pushes of the i39-L2 page build such frames was not traced (see
that page). No probe reaches the window.

## The problem it removes

`Frame::redefine_stamp` is `class_redefinition_count()` read when the frame
is built (`frame::current_redefine_stamp`). It names the constant-pool
generation the frame's code indexes only if the body the door handed out was
current at that moment. A lock-free door (a per-thread invoke-cache entry, a
JIT call door, a vtable snapshot) may push, during a redefinition's swap
(`[latest_began, latest)`) or just after it (window 3 of
`docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md`),
a body older than its stamp says. `obsolete_frames::convert_frames` then
guesses from the bytes (`runs_current_body`), and cannot tell two bodies
apart when their bytes are equal: a constant-only redefinition, where the old
activation should keep the old constant (the i39-L2 page).

## The idea

Stamp a frame with the generation of the body it runs, taken where the body
was chosen:

* `CachedBytecodeMethod` (jit-api) gains `pool_generation: u64`: the
  `class_redefinition_count()` its producer read from the class it copied the
  `Code` from, under whatever made that read consistent (the class-manager
  read lock at resolution; the writer, for the vtable re-install inside the
  swap, which would stamp `latest`). Producers build it through
  `CachedBytecodeMethod::from_parts`, so `CachedMethodParts` gets the field
  and the production producers (about ten `from_parts` calls in `vm`) set it; the test literals (about 75 across
  `jit/tests` and `vm`) need it too, or a `Default` for the memo half.
* A cached frame install (`Frame::reset_cached_value` and the other cached
  constructors) stamps `cached.pool_generation` instead of reading the global
  count -- the same one store, from a line the install already reads, so no
  new cost on the call path. An `Owned` frame keeps today's stamp.
* `convert_frames` then trusts the stamp: a frame whose body was chosen
  before the swap carries `< latest_began` and is translated as the old body,
  bytes equal or not; the window-3 rule and the in-window restamp rule both
  go (the "runs_current_body" guesses become unneeded).

## What to check before building

* Every producer must read the class's `Code` and the count under one
  consistent view; a producer that reads the count after the `Code` without
  the lock recreates the window in the other direction (a NEW body stamped
  old would be translated as old: arbitrary merged-pool reads, worse than
  today). The vtable re-install inside `ClassManager::redefine_class` must
  stamp `latest`, which it cannot read before the second advance: stamp it
  `latest_began` and treat `latest_began` as "new" for a frame whose body
  came from the swap itself, or move the re-install after the second advance
  (lane L5's file).
* `frame_runs_replaced_code` (the OSR door) and the stack walk's
  `frame_predates_its_class_redefinition` read the same stamp; both get
  simpler.
* The per-VM statics ratchet and the jit-api struct-literal count move.

## Positive control

A unit test in `obsolete_frames` that builds a frame from a
`CachedBytecodeMethod` produced before a constant-only redefinition (the
wave-19 `ldc_class` helper, `a_frame_built_during_the_swap_is_translated_only_from_the_old_body`
as the pattern) with the count moved into the window, and asserts the
converted frame reads the OLD constant.
