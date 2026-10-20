# ZGC round zt: wave t2 review residuals (lane t3/x2)

> **STATUS (2026-10-19, zt t6/rv3): FIXED IN CODE (items 1-5), PENDING BUILD/PROBE; item 6 waits for the maintainer.** 1 (`75848a0bf1`): `Zt2AlChunkZeroing` checks every 1024th array after publishing it through a `volatile` field and reading it back (stdout gains `sampled=`), so the check is no longer foldable; there is still no probe-level positive control (no switch skips the zeroing), the unit test `zt2al_descending_chunk_zeroing_reads_zero_and_counts` is it. Row: `Zt2AlChunkZeroing 20 32` vs `java -XX:+UseZGC`, SAME, `bad=0`. 2 (`5876a7a94c`): `Arena::commit_range_for_use` commits first and clears the idle stamps / reset bits only after a granted commit; the test hook sits in the commit's place; test `a_refused_commit_keeps_the_idle_stamps` (granted-commit arm is the positive control). 3-5 (`1e74dabbf6`, opt-in vtable switches, collector-independent): an index-moving redefinition drops the old cell (`forget_undeclared_lazy_dispatch`; test `zt6rv3_an_index_moving_redefinition_drops_the_old_cell`); `inherited_template` accepts a deferred declaring slot whose recorded cell holds the wanted snapshot (test `zt6rv3_the_inherit_template_accepts_a_deferred_declaring_slot`, with a foreign-snapshot negative control; the footprint/define-CPU win is still t2/fp's LD rows with both switches on); `[vm] vtable-lazy-dispatch:` reads the owning VM's manager (latch + per-manager counters; `rss_report::print_exit_report(owning_vm)`; test `zt6rv3_the_lazy_dispatch_line_reads_its_manager`). Left: 6 (maintainer acceptance of t2's shared-code changes).

> **STATUS (2026-10-19, zt t5/pg3): OPEN.** ZGC items: 1 (`Zt2AlChunkZeroing`'s check can be folded by an optimizing JIT; sample through a volatile publication), 2 (`Arena::commit_range_for_use`, gc/src/arena.rs:2808, clears idle stamps before `commit_range` can refuse). Items 3-5 are the opt-in lazy vtable dispatch (vm/src/runtime/vtable.rs `defer_dispatch` :668, `inherited_template`, `lazy_dispatch_line`), VM-footprint code; 6 waits for the maintainer's acceptance.

> **STATUS (2026-10-19, zt t3/x2): OPEN.** These are the review findings on `67bc580ab8..bb257f551d` that this lane did not fix because they lie outside the wave-t2 hunks, belong to another t3 lane's area, or need a host row. Each item says what would close it. The fixes this lane made are in `docs/internal/zgc-round-zt-20261018/t3-x2-report.md`.

## 1. Zt2AlChunkZeroing's zero check can be folded by an optimizing JIT (probe that cannot fail)

`tools/probes/Zt2AlChunkZeroing.java:32-37` checks each fresh `long[len]` for zeros right after allocating it, with no store in between. HotSpot C2 folds a load from a just-allocated array that nothing has written to a constant 0 (it finds the allocation's initializing memory, not a store). After warm-up the `bad` counter is dead code there, and probably also in this VM's IR tier with its known-zero facts. So `bad=0` on the compiled part of the run says nothing about whether the zeroing worked, and no setting makes the check fire, so there is no positive control. The t2/al report sells AL-1 as a soundness check as well as the A/B.

**Closes when** the check reads memory the compiler cannot see as fresh. One way: check a SAMPLE of arrays, for example every 1024th, after it has been published through a `volatile` field and read back, so the fence cost stays off the timed loop. Add a positive control: the unit test `zt2al_descending_chunk_zeroing_reads_zero_and_counts` already dirties a recycled chunk. A probe-level one would be a debug switch that skips the zeroing (none exists; say so in the row). Until then, AL-1 is a CPU A/B only, and soundness rests on the unit tests.

## 2. A refused commit clears the span's idle stamps before it fails (now visible: the span stays listed)

`Arena::commit_range_for_use` (`gc/src/arena.rs:2774-2807`) clears `idle_since` and the `reset_idle` bits of the requested granules BEFORE `self.data.commit_range` can refuse. Before t2/al, a refused free-list fit dropped its span, so the stale stamps sat on no-list bytes until a slide. Under `CRATONVM_ZGC_REFUSED_FIT_RELISTS` (default ON, `Arena::hand_out_fit`, `arena.rs:2741`) the span goes back on the list with cleared stamps:
- In decommit mode the uncommitter re-stamps it and gives it back one delay later. That is only a delay.
- The `reset_idle_count` decrement is a counting error only in readable-give-back mode, and a reset granule is still committed there, so its commit cannot be refused.

Low severity, and it needs an OS commit refusal. **Closes when** the stamp and reset-bit clearing moves after a successful `commit_range`. That is the commit / uncommit functions of `arena.rs`, lane `rl`'s area, so it is not done here. The unit hook `refuse_commits_for_test` returns before the clearing, so it cannot show this. The test needs the refusal moved into `commit_range`'s position.

## 3. Lazy dispatch: a redefinition that moves a method's index leaves the old cell

`VtableManager::defer_dispatch` (`vm/src/runtime/vtable.rs:668`) keys a cell by `(declaring class, method index)`. A redefinition that renumbers a method records a new key and keeps the old cell, with its built snapshot and the class-file buffer it views, until the class unloads. The t2 test did exactly that (index 0 -> 5). Dispatch is still right, because the refresh sweep hands subclasses the new block. Opt-in switch only. **Closes when** `install_vtable_descriptors` on a class that already has a table first drops that class's cells (`forget_lazy_dispatch(|d| d == class_id)` before the conversion). Test: after an index-moving redefinition, `lazy.len()` is unchanged.

## 4. Lazy dispatch and the inherit template do not compose

With `CRATONVM_VTABLE_LAZY_DISPATCH=1` and `CRATONVM_DEFINE_VTABLE_INHERIT_TEMPLATE=1`, `inherited_template` never matches. Its bytecode arm needs `inherited_dispatch`, which reads `entry.resolved_method` (`vtable.rs:1252`), and that is `None` for a deferred slot. So every all-inherited class takes the per-slot conversion. The answers are the same, but the template's footprint and define-CPU win is lost. **Closes when** the template's bytecode arm accepts a deferred declaring slot whose recorded cell holds the wanted snapshot (`Arc::ptr_eq` on `lazy.desc.dispatch`), as `defer_dispatch` already does for inherited slots. Deciding row: t2/fp's LD rows with both switches on.

## 5. `[vm] vtable-lazy-dispatch:` reports the environment, not the manager

`lazy_dispatch_line` (`vtable.rs:1608-1611`) prints `on=` from a fresh read of the flag, while each `VtableManager` latches its own choice (`with_lazy_dispatch`). Its `deferred=` / `built=` counters are process statics summed over every VM. Diagnostic only. **Closes when** the line reads the VM's manager (passed in by `rss_report`) and the counters move onto the manager.

## 6. Shared-code changes in t2 that reach G1 / Generational

All of these were reviewed as correct, and they are listed here for the maintainer because the round's rule is "no-op for G1/Gen":
- (a) `service_loader::impl_jars_load_class_inner` now writes the re-read loader back to the function's own `defining_loader` (t2/rv). This is a GC-safety fix on every moving collector.
- (b) `lang_class::listing_component_id` now does the by-name lookup first. The answers are the same on every collector.
- (c) `CRATONVM_LOADER_TABLES_HASHED` and `CRATONVM_VTABLE_LAZY_DISPATCH` are opt-in on every collector.

Nothing to fix. **Closes when** the maintainer accepts (a) for G1/Gen, or asks for it to be gated.
