# Proposal: invalidate what a redefinition touches, not the whole VM's compiled state

**Status: open — filed 2026-09-25 by interpreter round i1 wave 14, lane L3.**

## Where it stands

Every JVMTI `RedefineClasses` / `RetransformClasses` of ONE class
(`vm/src/vm/vm_exec.rs::redefine_class_with`, now through
`JitRealm::note_class_redefinition`) does, for the whole VM:

- `JitCache::clear_all` — every compiled body and OSR body is retired, so
  every hot method recompiles (C1, then the C2 supersede);
- `JitCache::bump_redefine_epoch` — every inline-cache slot flushes once;
- the verdict registries' and the code-state epoch's moves — every OSR denial,
  OSR loop budget, optimizing OSR memo entry, bail-list entry and runtime
  de-speculation verdict of the VM expires, and every queued compile request
  is dropped.

The comment at the call site gives the reason ("JVMTI redefinition can stale
caller-side direct calls and inline dispatch caches, not just compiled bodies
declared by `name`. Full eviction is rare"). It is not rare in the workloads
this VM runs as a test runner: Mockito's inline mock maker retransforms each
mocked type (and its supertypes) when a mock is first created, and coverage and
APM agents (JaCoCo, Byte Buddy agents) retransform in bursts at start-up. Each
of those retransforms costs the whole VM's warm-up again.

HotSpot scopes the same event: it deoptimizes only the compiled methods that
recorded a dependency on the redefined class's methods (`evol_method`
dependencies, `CodeCache::mark_for_evol_deoptimization`) and keeps everything
else.

## Expected benefit

On a Mockito-heavy test suite, no recompile storm per mocked type: compiled
code of unrelated classes survives, so steady-state speed returns after each
mock creation instead of after a full re-warm. The size is unknown until the
census in stage 1 runs; the upper bound is "every redefinition currently
costs one full warm-up of the VM".

## Design

A redefinition of class `C` must retire exactly:

1. bodies declared by `C` (its own methods, including OSR bodies);
2. bodies that inlined a method of `C` or devirtualised against `C`
   (`CompiledMethod::inlined_methods`, which `JitCache::invalidate_for_class_change`
   already walks for class-hierarchy changes);
3. bodies whose code CALLs a body of `C` directly (the direct-call binding the
   call-site comment worries about). Each published body already keeps the
   `RetainedCode` pins of the callees it calls; the reverse question ("who
   pins a body of `C`") is a scan of the published bodies' pins, O(bodies),
   far cheaper than recompiling them all;
4. inline-cache slots: keep the lazy `bump_redefine_epoch` flush (one flush
   per slot, no per-call cost), or scope it later by receiver class.

The per-VM verdicts then move per class, not per VM: `on_class_redefined`
already forgets the class's tiering state, OSR denials and compile verdicts;
the code-state epoch would move only for a full flush (the layout-upgrade
path keeps `flush_code_cache`).

## Staged plan

1. **Census.** Count redefinitions and evicted bodies per run
   (`JIT: fully invalidated N method(s)` is already logged at debug level; add
   the count to the `jit-method-stats` line) on a Mockito suite and a JaCoCo
   run. Decide from the numbers.
2. **Scoped eviction behind a switch** (default off): items 1-3 above, with
   the full flush kept as the switch's other arm, so one binary A/Bs both.
   Probe: redefine a callee that a hot caller inlined, and one it calls
   directly; the caller must run the new body on its next call in both arms.
3. **Per-class verdict expiry**: stop advancing the VM-wide epochs on a scoped
   redefinition; rely on `on_class_redefined` / `forget_for_class`.
4. Default on after the jdk-only suite, the core suite and the hotswap tests
   pass in both arms under all four collectors.

## Risk

Medium to high: a dependent body the scan misses keeps running the old
bytecode, a silent wrong answer. Stage 2's switch and a probe per dependency
kind (inline, devirtualised, direct call, OSR) are the guard.

## Progress (wave 17)

Interpreter round i1 wave 17, lane L5.

**Stage 1 (census) landed.** Every redefinition is counted per VM, with the
bodies its full flush evicted and — when `CRATONVM_DBG=jit-method-stats` is
armed — how many of them a class-scoped eviction would also have had to evict.
The three numbers ride at the end of the `JIT method stats:` line:
`| redefinitions=N redefine-evicted-bodies=M redefine-dependent-bodies=D`.
`M - D` is the recompilation a scoped eviction would save.

* `jit/src/lib.rs`: `JitCache::redefinition_dependents(class_id, class_name)`
  (read-only): the bodies the class declares, the bodies whose
  `inlined_methods` name it, and the transitive reverse closure over
  `_direct_callee_entries` — the set `invalidate_matching_collecting` would
  withdraw for items 1-3 of the design. `JitVerdictRegistry::note_redefinition_census`
  / `redefinition_census`.
* `vm/src/vm/realms/jit_realm.rs`: `JitRealm::note_class_redefinition_of(class_id, name)`
  scans before the flush (only when the stats line is armed) and records;
  `redefine_class_with` (`vm/src/vm/vm_exec.rs`) calls it.
* `jit/src/tiered.rs::verdict_count_fields` prints it. Tests:
  `jit/src/tests.rs::redefinition_dependents_counts_declaring_and_inlining_bodies_only`,
  `jit/src/tests.rs::the_method_stats_line_carries_the_vms_verdict_counts`,
  `jit_realm::tests::the_verdict_registries_share_the_realms_redefine_epoch`.

To read: run a Mockito-inline suite and a JaCoCo run with
`CRATONVM_DBG=jit-method-stats`; the numbers decide stage 2 below.

**Found and fixed on the way: every redefinition flushed EVERY live VM, twice
for its own.** `ClassManager::redefine_class` fires the JIT invalidate hook
for the class (step 8), and the hook's adapter (`vm_init.rs::jit_invalidate_adapter`,
written for layout-changing stub upgrades, with no VM identity) ran
`flush_code_cache` on every live VM — advancing every VM's code-state epoch,
the cross-VM leak waves 13-15 closed everywhere else — and then
`redefine_class_with` flushed its own VM again (so the census above would
have read an empty cache). Now `redefine_class_with` holds
`jit_realm::InPlaceRedefinitionScope` around `redefine_class`, the adapter
leaves the flush to it for that class (it still drops the allocation recipe),
and `note_class_redefinition_of` runs once, still under the class-manager
writer where the hook's flush used to run. The adapter's fan-out for real
layout changes (`upgrade_synthetic_class`, `recompute_subclass_layouts`) is
unchanged. Tests: `jit_realm::tests::the_in_place_redefinition_scope_names_one_class_on_one_thread`,
`vm_init::tests::an_in_place_redefinition_keeps_the_recipe_drop_and_skips_the_flush`.

**Stage 2a (the interpreter's site caches) landed, behind a `const`.** The
per-thread site caches (`vm/src/runtime/interpreter/site_cache.rs`: field,
static-field, fast-field, method, `new`, cast, interface-selection, numeric
constant and `invokedynamic` tables) no longer latch off for the process on
the first redefinition (`any_class_redefined`). A redefinition replaces the
class's constant pool under an unchanged `ClassId`, and what retires the
entries keyed on it is the resolution epoch every entry carries: the
classloading invalidate hook advances it, and `redefine_class_with` now also
advances it under the class-manager writer BEFORE the class is replaced and
again after `redefine_class` has swept the shared resolution memo (the hook
advances before its sweep, so a fill could otherwise snapshot the new epoch
and read an unswept row). An entry filled before the redefinition misses; one
filled after it resolved the new class. `SITE_CACHES_SURVIVE_REDEFINITION`
(`false` restores the latch) is the kill switch; with it `true` the latch
check is gone from every `get`/`put`, one relaxed load fewer per hit.

Why exact: JVMTI redefinition here preserves the superclass, interfaces,
fields and method signatures (`ClassManager::redefine_class` refuses a schema
change), so the values the tables hold for OTHER classes' sites (ids, field
indices and layouts, descriptors, subtype verdicts, selected declaring
classes) cannot change; the redefined class's own sites are retired by the
epoch; and a redefinition does not reset initialization state (the `new`
table's skip). Before, the latch closed only from step 7 of `redefine_class`;
the pre-swap advance now closes the window from the swap itself.

Test: `site_cache::i17_l5_redefinition_tests::a_redefinition_retires_older_entries_and_the_tables_keep_serving`.
Bench: `tools/probes/interp/L7/RedefineSiteCacheBench.java` (needs a
`-javaagent` jar, see its header): after/before under `--nojit` should drop
to ~1 from well above it. Correctness probe:
`tools/probes/interp/L7/RedefineSiteCacheProbe.java` (same agent setup): a
retransform renames what the target's constant-pool entries name; HotSpot
prints `before=111 after=222 again=222`, a stale site would print 111.

**Not landed: stage 2b, the scoped eviction of compiled code.** Still to do,
and still the risky half. The design above holds; what this wave learned for
it:

1. `redefinition_dependents` is the predicate stage 2b needs; the eviction
   is `invalidate_matching_collecting(retire_reason::INVALIDATED, ..)` with
   the same predicate, plus `log_invalidation` of a new
   `InvalidationRecord::RedefinedClass { class_id, class_name }` so an
   in-flight compile that inlined the class cannot publish (today the flush
   barrier does that job, and a scoped eviction must not arm it).
2. Inline caches: keep `bump_redefine_epoch` (lazy, one flush per slot).
3. The epochs: under a scoped eviction `note_class_redefinition` must still
   advance the redefine epoch (verdicts about inlined callees) but NOT the
   code-state epoch — the OSR memo and tiered stamps of unrelated methods
   would survive, which is the point; `on_class_redefined` already resets the
   class's own tiering state.
4. Other interpreter-side latches on `any_class_redefined` remain and are
   other lanes' files (`invoke_fast.rs`, `dispatch_virtual.rs`,
   `field_fast.rs`, `execute_entry.rs`, `vm/src/jit/helpers.rs`); most guard
   native/intrinsic shadowing, which is per-class state
   (`redefine_generations`) and is right to consult after a redefinition.
   A census of which of them are caches (and could move to an epoch like the
   site caches) is the next interpreter-side step.

Guard for 2b: a `const` switch (full flush as its other arm), and a probe per
dependency kind (inlined callee, devirtualised receiver, direct call, OSR
body) that retransforms the callee with a transformer changing its return
value and checks the caller sees it on the next call, in both arms.

## Progress (wave 18)

Interpreter round i1 wave 18, lane L4: the census asked for in item 4 above.
Of the interpreter-side `any_class_redefined` sites, the ones that were
CACHES switched off for the process no longer are, each behind a `const`
kill switch that restores the latch:

* the fast invoke doors (`invoke_fast.rs`, `dispatch_virtual.rs`;
  `DOORS_SURVIVE_REDEFINITION`) and the general path's receiver-first poly
  swap;
* the JIT helpers' compiled `ldc` slot memo, typecheck answer memos and
  bytecode-callee templates (`vm/src/jit/helpers.rs`);
* the selection memo (`resolve/selection.rs`) and the by-name native-callee
  memo (`native_callee_memo.rs`, `vm_exec.rs`).

They rest on a new process-wide cache-validity counter,
`cratonvm_classloading::class_redefinition_count()`, which `redefine_class`
advances before its constant-pool swap and after its resolution sweep and
`redefine_class_with` after the JIT flush. On the way it turned out that the
per-thread invoke cache and the promoted-invoke map never retired a
redefined CALLER's `(class, cp index)` entries at all (the general path
served the old pool's method; only the doors were kept off it, by their
latch) — fixed with the same counter. See
`docs/internal/fixed-bugs/interpreter-L5-fast-invoke-doors-decline-for-the-process-after-one-redefinition-FIXED-20260925.md`
and
`docs/internal/fixed-bugs/interpreter-L5-jit-helper-memos-latch-off-for-the-process-after-one-redefinition-FIXED-20260925.md`.

The remaining `any_class_redefined` reads are triggers, not latches: they
guard per-class native / intrinsic SHADOW decisions (`dispatch_virtual.rs`,
`dispatch_static.rs`, `execute_entry.rs`, `native_override.rs`,
`redefine_state.rs`), which are right to consult the
per-class `redefine_generations` once anything was redefined, plus one
per-thread flush that folds the flag into its state
(`flush_class_identity_dispatch_memos`, one flush for the process).
Stage 2b (scoped eviction of compiled code) is untouched; the analogous
scoping for the invoke cache is
`docs/internal/fixed-bugs/interpreter-L4-proposal-class-scoped-invoke-cache-retirement-FIXED-20260926.md`.

## Progress (wave 20)

Interpreter round i1 wave 20, lane L1. **Stage 2b landed, default on, behind
the `const` kill switch `SCOPED_REDEFINITION_EVICTION_ENABLED`**
(`vm/src/vm/realms/jit_realm.rs`; `false` restores the full flush on every
redefinition). The proposal is not finished: stage 3 and the "record every
copy per body" follow-up below remain, so this page stays open.

**What a redefinition of class `C` now withdraws**
(`JitCache::invalidate_for_redefinition`, `jit/src/lib.rs`), when
`JitRealm::scoped_redefinition_admitted` holds:

1. the bodies `C` declares, method-entry and OSR (`depends_on_redefined_class`:
   the key's declaring class id);
2. the bodies whose `inlined_methods` name `C`: a single-pass splice of a
   `C` method, a guarded (profile) speculation on a `C` receiver and a
   class-hierarchy (unique concrete method) bind whose static type is `C`;
3. `invalidate_matching_collecting`'s closure over baked direct calls,
   lambda adapters, cycle and retire cells;
4. NEW, found on the way: the callers of a SUPERSEDED body of `C`. A tier-up
   replaces a body in the cache without retiring it, and a caller that baked
   the old body keeps calling it; the closure matches callers by the entries
   of published bodies only, so it could not see them (a full flush withdrew
   them with everything else). `JitCache::redefinition_call_dependents`
   walks every body's `_direct_callee_roots` (superseded bodies included) and
   withdraws whatever reaches a body `C` declares or that inlined from `C`.

In flight: an `InvalidationRecord::RedefinedClass` is logged before the scan,
so a compile that began earlier and declares or inlined `C` is refused at
publication (`dependencies_are_current`); a compile that baked a direct call
to a withdrawn body is refused by `prepare_for_publication`'s retired-callee
check; and a compile that began earlier and baked a SUPERSEDED body of `C` (in
no map, so never retired) is refused by the new per-class barrier check
(`JitCache::baked_callee_redefined_since`, in `put` and `put_osr`). No flush
barrier is armed, so every unrelated compile publishes.

**When the full flush is kept** (`scoped_redefinition_admitted`): the switch
is off; `C` is `java/lang/Object` (both tiers elide `Object.<init>` by name);
some method of `C` has a registered native (a redefined class's bytecode wins
over its natives, `redefine_state::native_shadow_suppressed_in`, and a compiled
caller may have bound the native); or some compile of this VM COPIED bytecode
of `C` (`JitCache::bytecode_was_copied`). The last is the dependency kind no
body records: the IR tier records no `inlined_methods` for its splices at
all, the single-pass plan records only its outer sites (not a nested
splice), an elided empty constructor is recorded nowhere, and the by-name
callee door (`try_jit_compile_callee_slow`) publishes an INHERITED method's
body under the RECEIVER class's key, where no scan by the declaring class's
id finds it. The VM marks a class copied (`JitCache::note_bytecode_copied`)
under the class-manager read guard it reads the bytecode under -- in
`resolve_inline_site_from` just before it drops that guard, in
`is_elidable_construction` when it answers `true`, and in
`try_jit_compile_callee_slow` when the declaring class is not the receiver's
(`vm/src/runtime/interpreter/jit_bridge.rs`) -- and the redefinition asks
under the writer that replaced the class, so a copy is either marked in time
or read the new bytecode. Marks are never cleared (a compile published
outside the install witness can copy before a flush and publish after it).

**Also fixed: a flush left every withdrawn method at its tier.** `current_tier`
only advances; nothing lowered it for the bodies `clear_all` withdrew, so a
method at C2 whose body a redefinition (or a layout-upgrade flush) withdrew was
never offered for compilation again by the tiered path (the same defect the
code-cache sweeper fixed for itself with `note_body_withdrawn`, RT-8). Both
arms now demote what they withdrew (`JitCache::clear_all_collecting`,
`JitRealm::demote_withdrawn`, also from `flush_code_cache`).

**Other changes.** `JitCache::compiled_since_redefinition_of(class_id, epoch)`
(per-class redefinition barriers, emptied by `clear_all`) replaces
`compiled_since_last_flush` in the two JVMTI mode-exit questions
(`jvmti_events.rs`: `PollingBody::compiled_since_last_redefinition`,
`granted_exit_of_a_current_body`), which relied on every redefinition arming
the flush barrier. `redefine_class_with` (`vm_exec.rs`) computes
`has_registered_natives` under the writer and passes it to
`note_class_redefinition_of`. The census still runs: scoped, the evicted
bodies ARE the dependents, so `redefine-evicted-bodies` equals
`redefine-dependent-bodies` for those redefinitions.

**Still VM-wide on a scoped redefinition (stage 3):** the redefine epoch
(bail list, OSR entry rejects, runtime de-speculation, gate-pass memo, one
flush per inline-cache slot) and the code-state epoch (queued compile requests
drop; the optimizing OSR memo, whose bodies are in no cache this scan reads,
expires; OSR denials expire).

**Tests.** `jit/src/tests.rs`:
`a_scoped_redefinition_withdraws_each_dependency_kind_and_nothing_else` (own,
OSR, inlined callee, speculated receiver, CHA bind, direct call, transitive
direct call; an unrelated body and a same-named class of another id survive),
`a_scoped_redefinition_withdraws_a_caller_of_a_superseded_body`,
`a_scoped_redefinition_refuses_stale_in_flight_bodies_only`,
`a_compile_that_baked_a_superseded_body_of_the_redefined_class_is_refused`,
`copied_bytecode_marks_are_per_cache_and_survive_a_flush`,
`a_full_flush_hands_back_one_key_per_withdrawn_method`.
`vm/src/vm/realms/jit_realm.rs`:
`a_scoped_redefinition_keeps_unrelated_bodies_and_demotes_what_it_withdrew`,
`a_redefinition_no_body_records_takes_the_full_flush_and_demotes`.

**Probes.** `tools/probes/interp/L7/RedefineScopedEvictionProbe.java` (agent
jar; a hot caller that inlined, devirtualised or direct-called a retransformed
callee sees the new body on its next call; HotSpot prints
`before=1,1,1,other=499500` / `after=2,2,2,...` / `again=2,2,2,...`; run in
both arms of the switch). Bench
`tools/probes/interp/L7/RedefineScopedEvictionBench.java` (agent jar): 40
retransforms of a class nothing copied, each followed at once by a timed
slice of an unrelated hot loop; with the JIT on, after/before should drop
from well above 1 to ~1. `RedefineDoorsBench` may still take the full flush:
its `Victim` has an elidable constructor and `v()` is called from `main`,
whose OSR body can splice it.

**Next.** (a) Record the copies per body instead of per VM:
`interpreter-L1-proposal-per-body-copied-bytecode-dependencies-FIXED-20260930.md`. (b)
Stage 3: stop advancing the redefine and code-state epochs on a scoped
redefinition (the optimizing OSR memo then needs its own per-class check,
`unpublished_body_dependencies_are_current` already reads the new record).
(c) Stage 4: run the jdk-only suite, the core suite and the hotswap tests with
the switch off and on under all four collectors before calling it done.

## Wave 22 note — lane L6

Checked against the code (`jit_realm.rs::redefine_and_flush`,
`JitCache::invalidate_for_redefinition`): stages 1, 2a and 2b are landed as
the wave 17-20 progress sections say; stage 3 (stop advancing the VM-wide
redefine and code-state epochs on a scoped redefinition) and stage 4 (the
two-arm suite run) are the open items.

What wave 22 added beside the eviction: the eviction stops NEW publications
and lookups from reaching a stale body, but not a caller that baked it. Both
arms now also make every body the redefinition makes stale by its own record
NOT ENTRANT (`jit/src/not_entrant.rs`; collected before the eviction,
patched after it), so a running compiled caller's next call into it runs
the new bytecode
(`docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md`).
That also narrows what the scoped eviction's closure over callers has to buy:
a caller that merely CALLS a stale body no longer needs withdrawing for
correctness on x86-64 once that body was patched (its call re-dispatches;
`JitCache::not_entrant_counts` reports the refusals), only for speed (a withdrawn caller
recompiles and re-binds the new callee, a kept one re-dispatches through the
VM on every call). Keeping such callers is therefore a perf choice, and
priced by the re-dispatch cost; see the "re-bind the caller" note on
`i21-L1-proposal-make-withdrawn-bodies-not-entrant-20260925.md`.

Stage 3, concretely. The code-state epoch has two consumers a scoped
redefinition still relies on. (1) Queued compile requests: the queue entry
is a `CompilationTask` (`jit/src/tiered.rs`), i.e. the method KEY plus the
epoch stamp, and the worker fetches the bytecode when it dequeues, so a
request queued before the redefinition and started after it compiles the
new bytecode; `redefine_and_flush`'s comment ("which may carry the old
bytecode") overstates the need. What stage 3 must establish first is the
window between a worker's fetch and the compile-epoch witness it opens: a
fetch that read the old bytecode just before the redefinition, with a
witness opened just after it, would publish old code past the per-class
barrier. If the witness is opened before the fetch (as `try_compile`'s
admission gate is documented to do), the epoch drop is redundant for
queued requests; otherwise stamp each request with its class's redefine
generation (`ClassManager::class_redefine_generation_handle`, already per
class) at fetch and refuse at publication when it moved. (2) The optimizing
OSR memo, whose bodies are in no cache: publish them through the `JitCache`
(the `osr/optimizing.rs` step of
`interpreter-L7-proposal-jit-bridge-decomposition-RETIRED-20261003.md`), which also lets
`redefinition_stale_bodies` find them. With both, the scoped arm can skip
`advance_code_state_epoch`; the redefine epoch (inline-cache flush,
de-speculation verdicts) is cheap and can stay.

## Wave 28 note — lane L3: an identical redefinition should withdraw nothing

**Problem, from the code.** `redefine_class_with` (`vm/src/vm/vm_exec.rs`)
withdraws compiled code for every redefinition it installs, whatever the new
class file is: `JitRealm::note_class_redefinition_of` takes the scoped arm or,
for `java/lang/Object`, a class with a registered native or a class some
compile copied bytecode of (`scoped_redefinition_admitted`), the full flush of
every body in the VM. Mockito's inline mock maker retransforms the mocked
type's whole hierarchy on every `mock()` of a new type, `java/lang/Object`
included (the Object verification refusal in
`docs/internal/fixed-suite-bugs/springboot/twelve-unclustered-residuals-20260905-FIXED.md`),
and its transformer weaves a class the same way each time (a generic advice
dispatcher). So from the second mocked type on, the retransform of `Object`
and of every already-woven supertype installs bytes identical to what the
class runs -- and each of those is a full flush: every hot method of the test
JVM recompiles, once per newly mocked type. The obsolete-method history
already takes no step for such a redefinition
(`obsolete_code::tests::identical_redefinitions_take_no_history_slot`), and
`after_redefinition` takes no handshake when no constant moved; the JIT side
is the one that pays.

**Design.** `ClassManager` keeps the fingerprint of each class's CURRENT
definition (`class_bytes_digest` of the file the define or the last
redefinition installed; the class-bytes cache holds the retransformation
BASE, which a retransform does not replace, so it cannot answer this).
`redefine_class_typed` compares the effective new file's fingerprint with it
before Step 5 and, when equal, reports `Unchanged` instead of swapping: no
method, pool or vtable change, no history step, no `redefine_generations`
bump; `classRedefinedCount` still moves (HotSpot counts the redefinition,
and reflection caches key on it). `redefine_class_with` then skips the
resolution-epoch bumps, `retire_ldc_slots_of_class`,
`note_class_redefinition_of` and the frame conversion. A byte-identical file
is the only case this needs to catch; an equal-modulo-pool (EMCP) body still
needs the pool swap.

**Expected win and how to measure.** One full flush fewer per `mock()` of a
new type after the first, in every Mockito-inline test JVM. Measure the
`jit-method-stats` redefinition census and the compile count on a Spring Boot
module with many mocked types (before/after), and a bench probe: a hot
method timed after each of 200 `retransformClasses(Object.class)` calls with
a retransform-capable transformer that returns `null` (the class then runs
its base throughout, so every retransform after the first is identical).
Expected: recompiles per retransform fall from "every hot method" to zero.

**Cost and risk.** One 16-byte map entry per class and a hash of each
redefinition's file (already computed for the base digest). Risk: a
redefinition some caller relies on for its side effects (a JVMTI agent that
redefines to force compiled code out, or a debugger's HotSwap of an
unchanged class); no observable Java behaviour depends on it, since the code
that would run after the flush is the same bytecode. The JDWP
`RedefineClasses` path goes through the same `redefine_class_typed` and must
still count the redefinition.

**Staged plan.** 1. The fingerprint and the `Unchanged` outcome with a
`ClassManager` test (define, redefine with the same bytes: generation and
methods unchanged, count moved). 2. `redefine_class_with` skipping the JIT
and frame work on `Unchanged`, with the bench probe above.
