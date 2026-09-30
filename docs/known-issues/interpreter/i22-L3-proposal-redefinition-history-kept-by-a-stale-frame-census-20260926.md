# Proposal: keep a class's redefinition history exactly as long as a stale frame needs it

**Status: open — filed 2026-09-26 by interpreter round i1 wave 22, lane L3. Stages 1 and 2 landed in wave 23, lane L3 (without the debug line); the per-class floor the wave-23 note proposed and stage 3 (merged-pool compaction) landed in wave 24, lane L3; wave 25, lane L3 re-sized the hard caps (below) and proposes stage 5 (fold steps no frame tells apart); stage 4 open.**

## Wave 25 note — lane L3

**The hard caps were sized in retransforms, and a retransform is two
redefinitions here.** `vm/src/runtime/instrument.rs`,
`native_retransform_classes0`, installs the retransformation base and then
the transformers' output, so a thread parked across 600 renaming
retransforms needs 1,199 steps, past the 1,024-step hard cap: wave 24's
`L3W24ParkedAcrossManyRenames` still woke on the new pool under `--jdk-only`
(details on the i19-L3 untranslatable-frames page's wave-25 section). The hard
budget now counts each step's record and allows 4,096 steps / 512 KiB; the
compacting merge stops re-walking a history in which nothing died since its
last fruitless look.

**Stage 5 (proposed): fold the steps no frame tells apart.** Every cap here
bounds a history whose length follows the number of redefinitions a stale
frame slept through; HotSpot's follows the number of obsolete versions ON A
STACK (`MetadataOnStackMark`). The census already knows enough to get there
for the common shape -- a thread parked since before the first step while
another thread redefines:

* Two adjacent steps `s_i`, `s_{i+1}` (retired `r_i < r_{i+1}`) differ only
  for a frame stamped in `[r_i, r_{i+1})`: an older frame needs both, a newer
  one neither. When no frame can carry such a stamp, replace them by one step
  retired at `r_{i+1}` whose translation is `T_{i+1} o T_i` (runs composed
  through the dense form: `Translation::get` of each old slot through both),
  with `s_i`'s `source_new_len` / `source_moved`. The intermediate version's
  own constants then stop being live, so compaction removes them too, and the
  merged pool stays near one version plus what parked frames hold.
* Which stamps a thread can carry: a running thread (or a blocked one with an
  inexact list) any at or above its published floor; a thread blocked with an
  exact list naming the class only in `[floor, blocked_at]`, where
  `blocked_at` is `class_redefinition_count()` at its blocking deposit (a
  blocked thread pushes no frame) -- one more word in
  `BlockedFrameClasses`, written by `publish_blocked_frame_classes`. A pair is
  foldable when no counted thread's interval meets `[r_i, r_{i+1})`.
* Kept as now: the last `MAX_KEPT_REDEFINITIONS` steps are never folded (the
  census cannot see unregistered threads or unmounted continuations), and a
  step that replaced line tables is not folded (stored backtraces read them by
  capture count).

What limits it, found while sizing it: every registered thread that runs no
Java and never converts -- a GC or finalizer service thread, a native carrier
marked by `mark_native_thread_blocked` (inexact) -- counts for every class
with its CREATION count as floor, so it makes every window "possibly held"
and folds nothing, exactly as it keeps the drop rule from dropping. Stage 5
needs those threads to count for no class: the marker's own doc says its
threads own no interpreter frames while parked, so it can publish an exact
EMPTY list instead of an inexact one, and a service thread's registration can
publish its floor as `u64::MAX` while it runs no Java. Measure first with a
debug line listing, per census, the threads counted for every class and their
floors (the stage-1 debug line never landed).

## Wave 24 note — lane L3

**The floor is per class.** The wave-23 note's next step, as proposed, with
one condition it did not name:

* At its flag-raising blocking deposit (`NativeContextImpl::deposit_root_snapshot`,
  `vm/src/vm/vm_exec.rs`), a thread publishes the classes of its interpreter
  frames in `GcBlockState::blocked_frame_classes` (`vm/src/threading/jvm_thread.rs`;
  `obsolete_frames::publish_blocked_frame_classes`), before it raises
  `in_blocked_region`. The list is **exact only with no compiled activation
  on the stack** (`conservative_roots::current_thread_jit_depth() == 0`): a
  compiled activation's sites translate through its class's history from the
  stamp it was compiled at, and no interpreter frame names that class. It is
  a separate list, not the published frame trace the note suggested: that
  trace is also cleared by the registry's native-block marker
  (`mark_native_thread_blocked`, which raises the flag without a deposit,
  for a thread whose tid may resolve to a mounted virtual thread with Java
  frames), and the registry entry's trace `Arc` is shared by a separate call
  from the `GcBlockState` one. The marker now marks the list inexact.
* The census (`obsolete_frames::prune_histories_by_census`, replacing
  `stale_frame_floor`; `ThreadRegistry::for_each_gc_block_state` replacing
  `min_obsolete_frame_floor`) counts a blocked thread with an exact list only
  for the classes it names, and every other thread for every class; a class
  nobody counts for gets the current count. Sound across a wake racing the
  census: the census holds the class-manager writer, so a frame the woken
  thread pushes is stamped at or above every recorded step, and a later
  deposit's list is complete too. `ClassManager::prune_redefinition_histories`
  takes a floor per class.
* Test: `obsolete_frames::tests::a_thread_blocked_in_other_classes_does_not_hold_a_classs_history`
  (a thread blocked since before twenty redefinitions, in no frame of the
  class: the class keeps only its last eight steps; blocked in a frame of it,
  with an inexact list, or running, it holds the floor).

**Stage 3 landed, in two parts** (the i19-L3 untranslatable-frames page's
wave-24 section has the details):

* Translations are runs (`obsolete_code::Translation`), so the history's
  bytes follow what each redefinition changed, not the merged pool's size.
* `merge_for_obsolete_code_compacting` carries into the next merge only the
  old constants a frame the kept history can translate may name
  (`RedefinitionHistory::live_indices`). The design above said "the
  appended constants that neither the class's current methods nor any kept
  translation can name"; the second half had to be made precise: a kept
  version's frames run either its own code (its pool's prefix, now recorded
  per step as `source_new_len`) or a copy they were moved onto while it was
  current, whose names the move reports (`note_moved_names`, as
  `source_moved`). Taking "anything the oldest kept step's source pool holds"
  instead keeps every constant ever appended, because every merge carries the
  whole old pool into the image. Compaction runs only when the tail reaches
  512 slots and it halves it (it renumbers the tail: a moving redefinition),
  or when the plain merge overflows `u2`.

**Stored backtraces are not frames.** With the per-class floor a class no
frame runs drops its steps past the last eight at once, and a Throwable
captured in a version older than those would have read the CURRENT body's
line (`resolve_lines_as_captured` finds no table and leaves the entry to the
current-class resolver) where wave 23 -- whose one floor rarely moved --
kept the version's own line. So the census drops a step that replaced line
tables only past the soft caps (`RedefinitionHistory::census_may_drop_oldest`;
test `obsolete_code::tests::the_census_keeps_line_tables_within_the_soft_caps`);
steps that only moved constants go at once.

Stage 4 (HotSpot's -1 for a dropped version) is unchanged and still open. It
needs a way to mark an entry's line "resolved, unknown" that the lazy
resolver leaves alone: `LINE_NUMBER_UNKNOWN` (-1) is what it fills in
(`stackwalker::resolve_line_numbers_in_place` touches exactly those entries),
so a census-dropped version cannot simply be answered -1 in place. A probe
row for it needs nine line-changing retransforms of a class no frame runs.

## Wave 23 note — lane L3

Stages 1 and 2 landed, with four changes to the design above, each forced by
something the code showed:

* **A thread with a compiled activation on its stack does not publish.**
  Its compiled sites translate through the history from the stamp the body
  was compiled at (`jit::helpers::stale_cp_site_index`), which no frame
  shows, so `publish_stale_frame_floor` (`obsolete_frames.rs`) keeps the
  floor it published before (`conservative_roots::current_thread_jit_depth()`
  is non-zero). That floor stays a lower bound: a body compiled before a
  redefinition of its class is made not entrant by it, so an activation that
  began after the floor needs no step retired before it.
* **Refused frames are left out, not recorded.** Every `translated_body`
  refusal is final (the history no longer reaches the stamp, or the body
  cannot be decoded); keeping steps for such a frame helps nothing and would
  hold every class at its hard caps for as long as it lives.
* **The last `MAX_KEPT_REDEFINITIONS` steps stay whatever the census says**,
  for what it cannot see: compiled activations of a thread that did publish
  (none, by the rule above, but cheap insurance), and threads not in the
  registry.
* **A needed step is capped, not kept forever**: `HISTORY_HARD_BYTE_BUDGET`
  (128 KiB) / `MAX_HISTORY_STEPS_HARD` (1024) per class. With a thread
  parked since boot the floor never moves (below), and an agent that stamps
  a fresh pool on every retransform grows each translation with the merged
  pool.

Where it runs: `redefine_class_with` (`vm/src/vm/vm_exec.rs`) under the
class-manager writer, before the swap, prunes every class's history with
the least published floor (`ThreadRegistry::min_obsolete_frame_floor`);
`RedefinitionHistory::prune` is constant time unless it drops something.
The redefinition's own step is trimmed with the census in force at its
`record`. Tests: `obsolete_code::tests::the_census_drops_unneeded_steps_and_keeps_needed_ones`,
`obsolete_frames::tests::a_thread_parked_across_hundreds_of_renumberings_stays_translatable`.
Probe: `tools/probes/interp/L3/L3ObsoleteParkedAcrossRenames.java` (150
distinct renames; the 300 of the "Expected win" row would pass the hard
byte cap for this class, whose translations grow with the merged pool --
which is stage 3's point).

The `CRATONVM_DBG_REDEFINE_CENSUS` line was not added: its gate would be one
more process-wide cached flag, and the unit tests above see the floor
directly.

**What limits it in practice, and the next step.** One global floor: a
thread parked since long ago (a JDK service thread blocked since boot) holds
it for every class, so the census then only extends retention and never
trims. A per-class floor is within reach without a new walk: a blocked
thread publishes its frame snapshot at the blocking deposit (the one
`resolve_captured_lines_of_class` rewrites), whose entries name each frame's
class, and a blocked thread pushes no frame while blocked -- so a blocked
thread whose snapshot names no frame of class X needs none of X's steps
retired after its floor. Running threads keep the global rule. Stage 3 (pool
compaction) remains what bounds the merged pool and so the translations.

## Problem, with evidence

A frame running a method body a JVMTI redefinition replaced is translated
into the class's merged pool through the class's `RedefinitionHistory`
(`classloading/src/obsolete_code.rs`). How long that history is kept is a
guess, not a fact about the frames:

* **Too short.** Steps are dropped past `MAX_KEPT_REDEFINITIONS` (8) once
  the history is over `HISTORY_BYTE_BUDGET` (32 KiB) or `MAX_HISTORY_STEPS`
  (256) (wave 22; before, simply past 8). A thread blocked across more
  distinct renumbering redefinitions than that -- it converts its frames only
  when it wakes -- wakes untranslatable and runs its old indices against the
  new pool
  (`docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md`,
  case 2).
* **Too long.** A class nobody runs old code of keeps its last eight steps
  (more, up to 32 KiB) forever, and its merged constant pool keeps
  every constant any earlier version had (`merge_for_obsolete_code` appends
  the whole old pool, which already holds the version before's), until the
  pool no longer fits a `u2` count -- at which point the merge fails, the
  history is cleared, and every older frame becomes untranslatable at once.
  An agent that stamps a fresh constant on each retransform grows the pool
  by one entry per retransform with no bound below 65,535.
* **Line numbers.** HotSpot drops a class version no frame runs at the
  redefinition, so a Throwable captured in it answers line -1 afterwards;
  CratonVM keeps the version's line (`tools/probes/interp/L3/L3ObsoleteTraceLines.java`,
  the `early` row). Knowing which versions are still running is the same
  census.

HotSpot answers all three with `MetadataOnStackMark`: at each redefinition's
safepoint it marks every `Method*` and constant pool some frame references,
and purges previous versions that nothing marked.

## Design

A per-class census of the oldest stamp any frame of the class still carries,
taken where the VM already stops threads:

1. **Every thread publishes a floor.** `JvmThread::redefinitions_seen` is a
   plain field. Mirror it into an `AtomicU64` beside the thread's other
   cross-thread state (`GcBlockState`), written where it is written now
   (`convert_obsolete_frames_if_redefined`). Every frame of that thread was
   translated up to it, except the ones `translated_body` refused -- record
   those separately: the minimum stamp of a refused frame, or `u64::MAX`.
   An unmounted virtual thread's boxed `JvmThread` publishes the same way
   through the virtual-thread manager, which can enumerate them.
2. **The redefining thread reads the floors** after its handshake pause
   (`obsolete_frames::after_redefinition`), from the thread registry and
   the virtual-thread manager: `floor = min(published)`.
3. **The history drops what is below the floor.** `RedefinitionHistory::prune(floor)`
   drops every step with `retired_at <= floor` (no frame is stamped below
   it), whatever the budget says; the byte budget becomes a cap that only a
   census answering "still needed" can exceed up to a hard limit.
4. **The merged pool drops what no kept step reaches.** With the history
   pruned, the appended constants that neither the class's current methods
   nor any kept translation can name are dead; a compaction at the next
   merge (renumber the appended tail, compose the translation) frees them.
   This is the stage that bounds the pool.
5. **Optional: HotSpot's -1.** A step pruned by the census (not by the cap)
   marks its version dropped; `obsolete_frames::resolve_lines_as_captured`
   then answers -1 for a backtrace entry of that version (the resolver
   after it must be told to leave it) instead of the old line, matching
   HotSpot's backtrace answer.

## Expected win and how to measure it

* Correctness: a thread blocked across any number of redefinitions stays
  translatable while it exists (probe: `L3ObsoleteParkedAcrossToggles` with
  300 distinct renames instead of 41 toggles).
* Memory: a class retransformed N times with no stale frame keeps O(1)
  history and a pool of its current size plus what live frames need, not
  O(N). Measure with a unit test that redefines a class 1,000 times with a
  fresh constant each and asserts the pool size and history bytes stay flat
  when no frame is stale.

## Cost and risk

The floors are one relaxed store per conversion pass and one registry walk
per redefinition that moved constants (already a stop-the-world pause).
Risk is in stage 4: renumbering the appended tail must compose with every
translation still kept, and the resolution memos keyed by `(class, cp
index)` must be swept for the moved indices exactly as a redefinition
sweeps them now. Stages 1-3 are bookkeeping; stage 4 needs its own review.

## Staged plan

1. Publish the per-thread floor and the refused-frame minimum; a
   `CRATONVM_DBG_REDEFINE_CENSUS=1` line per redefinition with the floor and
   the steps it would prune (no behaviour change).
2. Prune the history by the floor; keep the byte budget as a cap.
3. Merged-pool compaction.
4. The -1 answer for census-dropped versions (a `--compatible`-visible
   change: needs the probe's `early` row to print HotSpot's -1).

## Wave 24 note — lane L6 (the not-entrant assumption, verified)

The rule "a body compiled before a redefinition of its class is made not
entrant by it" was checked against every kind of compiled body that can hold
constant-pool sites of a class (its own methods, and splices of them, whose
holder word names the callee's class). It holds, after three wave-24 fixes,
for every body the not-entrant patch reaches:

* an optimizing OSR body (memo, never published) was marked withdrawn but
  its running frame was never told to leave (the poll's compile-id lookup
  cannot name an unpublished body); a body the memo had already forgotten
  was not even marked. Fixed: `helpers.rs::running_osr_body_named`,
  `JitCache::body_withdrawn_by_redefinition` (whole-cache barrier), and the
  OSR door refuses to START a withdrawn body (`try_osr`);
* a body a compile of the old bytecode published between the not-entrant scan
  and the eviction was evicted but never patched. Fixed:
  `JitCache::fence_redefinition_publications`, before the scan.

It does NOT hold for a body whose patch is refused (`Protect`, `NoEntryPad`,
...): it stays enterable through baked calls, below any floor.
`docs/internal/fixed-bugs/interpreter-L6-a-refused-not-entrant-patch-leaves-an-old-body-below-the-census-floor-FIXED-20260927.md`
has the table, the scenario (it needs more than `MAX_KEPT_REDEFINITIONS`
redefinitions of the class after the body's stamp) and the clamp that closes
it (landed in wave 25, lane L6: `JitCache::min_unpatched_cp_stamp` lowers the
prune floor in `obsolete_frames::prune_histories_by_census`).

## Wave 26 note — lane L3

Stage 5's motivating number halved without it: a `retransformClasses` is now
ONE redefinition (the swap back to the retransformation base is gone,
`docs/internal/fixed-bugs/interpreter-L3-retransform-redefines-each-class-twice-FIXED-20260928.md`),
so a thread parked across N retransforms that each move a constant needs N
history steps, not 2N-1, and the base's constants no longer enter and leave
the merged pool on every retransform. The hard caps were left as wave 25 sized
them (4,096 steps / 512 KiB), which now covers about 4,000 such retransforms.
Stage 5 (fold adjacent steps no census-visible frame tells apart) is still the
design that makes the history independent of that count; measure it against
`L3W24ParkedAcrossManyRenames` with the rename count raised past 4,096 before
building it.
