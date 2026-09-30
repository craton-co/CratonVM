# Proposal: a shared, per-class resolved constant pool for `CONSTANT_Class` and `ldc` entries

**Status: open (stage 1's identity-neutral half landed on the per-thread tables, wave 24; stage 2 landed in the shared table, wave 2; wired into `multianewarray` and the JIT's CP-class helper, wave 3; numeric `ldc`/`ldc2_w` lock-free, wave 4; stage 1a's validity counter specified, wave 5, and landed for the `new` and numeric-constant site tables, wave 6, and the cast-site table, wave 7; the shared per-VM table remains) — filed 2026-09-23 by interpreter round i1, lane L2.**

## Wave 26 note — lane L5

* **A second consumer: the owner-class access verdict.** Since wave 26 every
  field / method resolution MISS asks JVMS §5.4.4 for the class its reference
  names (`field_access::member_owner_access_refusal`, the class-constant
  verdict) — once per member reference, although HotSpot decides it once per
  CLASS entry (`klass_at_impl`), which every `Fieldref` / `Methodref` naming
  that entry then shares. The per-entry slot of (b) is exactly where that
  verdict belongs: a published class id in the slot means "resolved AND
  accessible from this class", so a member reference whose class entry is
  published skips both the owner lookup and the verdict. Today's cost is one
  class-constant verdict per member-reference miss (cold; it never runs on a
  cached answer).
* **What the slot must NOT be used for.** The compiled tier: HotSpot's own C1
  / C2 bind a call whose class entry holds a resolution error once the class
  is loadable through the loader (`ciEnv` looks the name up in the loader's
  dictionary; measured with
  `tools/probes/interp/L5/L5W26CompiledOwnerFailureRecord.java`, 303 to 6939
  of 50000 calls served by compiled code). CratonVM's `CpResolvers::invoke`
  now declines such a site instead (JVMS §5.4.3); a slot read by the compiler
  must keep answering "failed" for it, not the class the loader has since
  initiated.

## Wave 25 note — lane L5

* **Symptom 2 ("loader-namespaced classes get no cache") closed for FIELD
  references.** Wave 24 admitted namespaced `new` / cast sites whose answer is
  the loader's own record; the field-site table had the same admission behind
  the opt-in `field-site-cache-loader` arm (fill only when the owner came from
  the loader-LOCAL lookup), with its correctness argument written down in wave
  16 (`interpreter-L4-proposal-static-field-quickening-FIXED-20260930.md`). Wave 25 made the
  arm default-on (`vm/src/runtime/interpreter/field_access.rs`
  `field_site_cache_loader_enabled`; kill switch
  `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER=0`), which also lets `field_fast`
  quicken those sites. Measure with
  `tools/probes/interp/L5/L5W25LoaderFieldSiteBench.java` (`loader/app` under
  `--nojit`, A/B that variable) and `CRATONVM_DBG=field-site`'s field
  `reject_loader`.
* **Why this and not (b), the shared per-VM table, this wave.** (b) is a new
  lock-free structure on every `CONSTANT_Class` consumer of both tiers and
  changes which thread's answer wins; nothing measured says the
  once-per-thread warm-up is the cost, and its acceptance set is not runnable
  from a lane. The field arm had a finished argument, a measured per-opcode
  cost (a getfield 550-720 ns vs 250 ns cached) and a kill switch. The
  i13-L5 by-name memo was the other candidate; its stage 3 waits on the
  stage-1 arm tally, which has not been taken.
* **(b) has a correctness reason now, not only a cost one:**
  `interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md` (fixed-bugs; the remainder is carried below, "Progress (wave 41)").
  A success in a loader-namespaced class is published per thread only, so a
  racing failure is recorded for everyone else; a `ResolvedCp` slot written
  by CAS from 0 with either a class id or an error index is exactly HotSpot's
  single-winner tag protocol and closes it.

## Progress (wave 24) — lane L5

* **Stage 1, in its identity-neutral form, landed on the existing per-thread
  tables.** `vm/src/runtime/interpreter/opcodes.rs` `site_fill_admitted`
  replaces the flat `referencing_class_has_loader_namespace` refusal at the
  three fill sites (`op_new`, `anewarray_component`, `resolve_cast_target`).
  A loader-namespaced referencing class's site is now filled when its answer
  is (a) a JDK-global name (`is_global_resolution_namespace`: the resolver
  answers it from the global mapping whatever the loader), or (b) exactly the
  answer the loader has itself recorded — the lookup `resolve_class_loader_aware`
  consults first (`lookup_loader_initiated`, or `lookup_loader_defined_exact`
  for an isolated URL loader). That is the wave-2 note's admission rule
  ("fill only from answers the defining loader produced"). The entries keep
  their `NameEpochs` tag, which covers every change of that lookup: the loader
  defining the name itself is a second definition of a mapped name (the name
  generation moves), and the initiating memo's writer
  (`cache_loader_initiated`, which also evicts) and its unload sweep bump the
  VM's resolution epoch. An answer from the global fallback after the loader
  declined has no record and is still refused; array names from a namespaced
  class are still refused; the catch-row and trivial-target fills are
  unchanged. Kill switches: the existing `CRATONVM_JIT_NO_NEW_SITE_CACHE` /
  `CRATONVM_JIT_NO_CAST_SITE_CACHE`.
* **Measure:** `tools/probes/interp/L5/L5W24LoaderSiteBench.java` (the same
  `Work` class defined by the app loader and by a platform-parented
  URLClassLoader; `timing loader/app` should fall from several to about 1
  under `--nojit`), and `CRATONVM_DBG=field-site`'s `new` / `cast`
  `reject_loader` counters on a webapp or H2-under-a-loader run, which count
  exactly the executions still refused.
* **Acceptance still owed** (not runnable from a lane): the forked-loader
  Spring suites and difftest's interp modes, as the risk section says. If
  one of them regresses, the two kill switches bisect it to this change.
* **Remains:** a loader-namespaced site whose answer came from the global
  fallback (no loader record) — admissible too by the same epoch argument
  (a later record moves the resolution epoch), not done to keep this stage
  exactly "the loader's own answer"; the shared per-VM table (b); `ldc` onto
  `resolved_references` (stage 3).

## Progress (wave 7)

* **Remains (a) landed: the cast-site table is on `NameEpochs`.**
  `CastSiteCache = SiteCache<CastSite, NameEpochs>`; its three verdict memos
  carry the definition epoch themselves (`CastSite::memo_epoch`, read
  through `CastSite::observed` / `observed_at`). `anewarray`'s component
  and the `checkcast` / `instanceof` / catch-row resolutions therefore
  survive a class-loading burst, as the `new` and numeric-constant sites do
  since wave 6. Details and tests:
  `interpreter-L2-proposal-negative-cast-memo-FIXED-20260930.md`, wave 7. Remains: (b), the
  shared per-VM table.

## Progress (wave 6)

* **Stage 1a's counter landed** — `classloading/src/class_manager.rs`:
  `CLASS_NAME_GENERATION` / `class_name_generation()` (exported from
  `cratonvm_classloading`). It moves only on the events that can change an
  EXISTING name answer, exactly the wave-5 list:
  1. a `loaded_classes_insert` whose name, in either separator spelling,
     already maps to a DIFFERENT id, or that displaces a different id
     (`name_insert_changes_an_existing_answer`, read before
     `name_definitions` is updated); an insert of a brand-new name, or a
     re-mapping of a name to the id it already has (an initiating-loader
     alias of a unique class), does not move it;
  2. `loaded_classes_remove` and the unload path's bulk `retain` (which also
     covers `ClassId` reuse after unloading);
  3. the resolution-invalidate hook stays on `resolution_epoch`, which every
     table pairs with it.
* **Validity split, per table.** `vm/src/runtime/interpreter/site_cache.rs`:
  `SiteCache<T, E: SiteEpochs = DefinitionEpochs>`. `DefinitionEpochs`
  (`class_definition_epoch`, `resolution_epoch`) stays the default and is
  the only admissible tag for a table that memoises a subtype verdict or
  another hierarchy fact (cast sites, fields, methods, interface selection,
  indy). `NameEpochs` (`class_name_generation`, `resolution_epoch`) now tags
  `ClassSiteCache` (`new`: a resolved id, `num_total_fields` — moved in place
  only with the resolution epoch — and a monotonic init state) and
  `PrimitiveConstantSiteCache` (numeric `ldc`/`ldc2_w`). Both now keep their
  entries through a class-loading burst, and a `new` site whose own
  resolution LOADED its class is filled on that first execution (the
  definition epoch moved under that resolution and dropped the fill before).
* **Identity-neutral by construction**: a table entry holds the answer for a
  name that was mapped when it was filled; the counter moves on every event
  that can change such an answer (list above), and on nothing else only
  because nothing else can. The hidden-class self reference, the transform
  hook and the array-component pre-pass of `resolve_class_loader_aware` are
  not reached for a no-namespace referencing class's cached site differently
  than before: the admission test (`referencing_class_has_loader_namespace`)
  is unchanged, and the fabricated-stub-then-real-class sequence retires
  through `upgrade_synthetic_class` (resolution epoch) or through a second
  definition of the name (this counter).
* Tests: `classloading/src/class_manager.rs`
  `name_generation_predicate_separates_new_names_from_replacements`,
  `a_second_definition_of_a_name_moves_the_name_generation`;
  `vm/src/runtime/interpreter/site_cache.rs`
  `i6_l2_name_epoch_tests::a_new_class_name_retires_definition_tagged_entries_only`.
* Measure: `CRATONVM_DBG_FIELD_SITE=1`'s `new: hit= miss= fill=` and
  `ldc: hit= miss=` on a boot-heavy run (Spring Boot start, the regression
  vectors), before and after; the miss count should fall by roughly the
  number of class definitions that interleave with warm `new` sites.
* **Remains.** (a) `CastSiteCache`: its `target` (and `anewarray`'s
  component) is a resolution fact, but the same entry carries the positive,
  negative and catch-row verdict memos, which must stay on the definition
  epoch. Splitting needs a per-entry `memo_epoch` in `CastSite` checked by
  `cast_site_hit_admits`, `negative_memo_answers`, `fill_negative_cast_memo`
  and `exception_dispatch::catch_row_verdict` (43 references across
  `opcodes.rs`, `typecheck.rs`, `exception_dispatch.rs`), then the table
  moves to `NameEpochs`. (b) The shared per-VM `Box<[AtomicU64]>` table of
  the wave-4 design, now that its tag exists.

## Progress (wave 5)

* **Stage 1a not landed; what blocks it is now narrower and written down.**
  Its validity counter must move on exactly the events that can change a
  NO-namespace referencing class's answer, and on nothing else:
  1. `loaded_classes_insert` of a name that ALREADY maps under some loader
     (a second loader defining `p.X` can turn `find_unique_class_by_name`'s
     unique answer into an ambiguous one — the `class_definition_epoch` doc
     names this case), but NOT the insert of a brand-new name;
  2. `loaded_classes_remove` and the unload `retain`;
  3. the resolution-invalidate hook (`resolution_epoch`: the initiating-loader
     memo, `upgrade_synthetic_class`, `recompute_subclass_layouts`).
  (1) needs a "did this name exist" probe inside `loaded_classes_insert`
  (`classloading/src/class_manager.rs`, the name-mapping code, outside this
  lane's scope) and a per-VM counter beside `class_definition_epoch`. With it,
  the shared table stores only the RESOLUTION `(id, generation)`; the
  receiver verdict memos stay per-thread on `class_definition_epoch`.
* **Why it matters more after wave 5:** `class_definition_epoch` now also
  moves on every superclass / interface edge change (so that subtype-verdict
  memos stay exact — see `interpreter-L2-proposal-negative-cast-memo-FIXED-20260930.md`),
  which makes the per-thread resolution entries miss through one more kind of
  event that cannot change a resolution. Splitting resolution validity (this
  stage) from verdict validity is the fix for that too.
* **The identity-neutrality argument still to be checked line by line** for
  the no-namespace path of `resolve_class_loader_aware`
  (`vm/src/runtime/interpreter/constants.rs`): the hidden-class self
  reference (a function of the referencing class alone), the transform hook
  (stages bytes only for a class not yet defined), the array-component
  pre-pass (gated on a defining loader, so skipped), and the global
  `load_class_concurrent*` fallback, whose compatible-mode FABRICATION of a
  stub for a missing name is an insert of a new name followed, when the real
  class arrives, by `upgrade_synthetic_class` (event 3) — so a fabricated
  answer is retired exactly when it would change.

## Progress (wave 4)

* **Numeric constants are off every lock.** `ldc2_w` of a `CONSTANT_Long` /
  `CONSTANT_Double` took the `class_manager` read lock and `get_class` on EVERY
  execution (it never probed the record), and `ldc` of a `CONSTANT_Integer` /
  `CONSTANT_Float` took the `resolution_cache` read lock and a hash probe.
  Both now answer a warm site from a per-thread
  `site_cache::PrimitiveConstantSiteCache` (`JvmThread::prim_const_sites`),
  under the same validity condition as every other `SiteCache` and the same
  kill switch as the `ldc` record (`CRATONVM_JIT_NO_LDC_CONST_CACHE=1`).
  Numeric values only: they are pure functions of the constant pool, so this
  changes no resolution identity, and they hold no heap reference, so the
  per-thread table needs no GC root. `ldc` fills it from its own resolution and
  from a record hit another thread produced.
  (`vm/src/runtime/interpreter/constants.rs`, `execute_ldc`, `execute_ldc2w`;
  test `numeric_ldc_sites_are_served_from_the_thread_table`; measure with
  `tools/probes/interp/L2/L2Ldc2wBench.java`.)
* **Stage 1 still not landed — and the "cache only bootstrap-loader classes"
  variant does not buy what it seems to.** For a referencing class with NO
  loader namespace (bootstrap/platform/app-defined, no defining-loader side
  entry) `resolve_class_loader_aware` reduces to the global name → id mapping,
  which the per-thread `ClassSiteCache` / `CastSiteCache` already cache under
  `class_definition_epoch` + `resolution_epoch`. A shared table restricted to
  those classes would therefore be identity-neutral, but it would also have to
  carry the same two epochs per entry (a synthetic stub can still be replaced
  under a name, and `upgrade_synthetic_class` rewrites in place), so its only
  gain over the per-thread tables is warming once per VM instead of once per
  thread — worth having for 200-thread pools, but not the lock-free `ldc` or
  the loader-namespaced caching this proposal is for. The identity-changing
  part (loader-namespaced referencing classes) still needs the forked-loader
  Spring suites as its acceptance set.
* **Refined next stage (1a), identity-neutral:** a per-VM,
  `ClassId`-indexed side table of `Box<[AtomicU64]>` (allocated on first
  resolution of a class, like the class-init-state side table), admitted with
  exactly the per-thread tables' `referencing_class_has_loader_namespace`
  test, each slot `(resolved id: u32, generation: u32)` where the generation
  is a new per-VM counter bumped by the two things that can change a
  no-namespace answer: a `loaded_classes` REPLACEMENT or removal of a name
  (not every insert — an insert of a new name cannot change an existing
  answer; that is the difference from `class_definition_epoch`, which moves on
  every definition and makes the per-thread tables miss through every loading
  burst) and the resolution-invalidate hook. The per-thread tables then become
  a thin layer above it (or go). Stage 1 proper later extends admission to
  loader-namespaced classes filled only from the defining loader's own answer.

## Progress (wave 3)

* **Refinement 1 below is resolved without this proposal.** The condy and
  `CONSTANT_MethodHandle` records moved to a never-evicted, insert-if-absent
  map in `ResolutionCache` (`put_permanent_constant`), the failure record lost
  its cap, and unloading now purges every record of the unloaded classes
  (`ResolutionCache::forget_classes`, which also closed a dangling-root hole) —
  see `docs/internal/fixed-bugs/interpreter-L2-condy-record-eviction-reruns-bootstrap-FIXED-20260923.md`.
  Stage 3 is therefore no longer urgent for correctness; it stays the way to
  take the `RwLock` off every `ldc`.
* **Stage 1 still not landed**, for the reason recorded in wave 2: filling a
  per-class entry for loader-namespaced referencing classes changes which
  class a `CONSTANT_Class` resolves to under `compatible` mode whenever
  `resolve_class_loader_aware` falls back to global resolution after a user
  loader declines, and its acceptance set (the forked-loader Spring suites) is
  not runnable from a lane. The JIT's CP-class helper (`jit_resolve_cp_class`)
  and `multianewarray_alloc` now consult and fill the stage-2 failure record,
  so the opcode-level `CONSTANT_Class` consumers of both tiers share it.

## Progress (wave 2)

* **Stage 2, failures, landed — in `ResolutionCache`, not yet in a per-class
  table.** `classloading::resolution::ResolutionCache` now holds a
  `ResolutionFailure` record per `(class, cp index)`, reached through
  `MemberResolver::{probe,record}_resolution_failure`, and `ldc`, `new`,
  `checkcast`, `instanceof`, `anewarray` and condy rethrow it. It is the
  semantic half of this proposal; the representation (a hash map behind the
  `resolution_cache` `RwLock`, probed only after a process-wide "anything
  recorded" bit) is not the lock-free array proposed below. When `ResolvedCp`
  exists, the failure encoding in its slot replaces that map.
* **Refinement for stage 1.** Two facts found this wave constrain the design:
  1. The condy/ldc record is FIFO-capped at 65,536 entries per VM and shares
     the cap with every recorded `ldc` constant (Integer/Float/String/Class
     since 2026-08). Eviction is harmless for a string literal but re-runs a
     condy bootstrap — a JVMS §5.4.3.6 identity violation
     (`docs/internal/fixed-bugs/interpreter-L2-condy-record-eviction-reruns-bootstrap-FIXED-20260923.md`,
     fixed in wave 3). A per-class
     table has no global cap and removes the problem; stage 3 (moving `ldc`
     onto `resolved_references`) should therefore come BEFORE stage 4, not
     after.
  2. `resolve_class_loader_aware` declines a user loader's failing
     `loadClass` and falls back to GLOBAL resolution (`drive_defining_loader_load`
     returns `None`), so a class the user loader refuses can resolve to the
     application loader's copy. A per-class resolved entry would make that
     wrong answer permanent. Stage 1 must fill only from answers the defining
     loader produced (or from the global namespaces `is_global_resolution_namespace`
     names), which is the same admission the site caches' `reject_loader`
     counters already measure.
* **Stage 1 not landed**: it changes resolution identity for loader-namespaced
  classes (a behaviour change for compatible mode) and needs the forked-loader
  Spring suites as its acceptance set, which this lane cannot run.

## The problem, in three symptoms

1. **Per-thread, per-opcode caches of one fact.** A `CONSTANT_Class` entry is
   resolved once per class by JVMS §5.4.3, yet CratonVM re-derives it in four
   places with three caches: `ClassSiteCache` (`new`), `CastSiteCache`
   (`checkcast`/`instanceof`, and since this round `anewarray`), the `ldc`
   record in `resolution_cache` (`execute_ldc`), and nothing at all for
   `multianewarray`'s levels. The per-thread tables warm separately on every
   thread, so a pool of 200 worker threads resolves each site 200 times.
2. **Loader-namespaced classes get no cache.** The per-thread tables refuse a
   referencing class with a user loader (`referencing_class_has_loader_namespace`)
   because their validity epochs only cover the global name → id mapping. But
   JVMS §5.4.3 makes a *resolved entry* permanent regardless of loader — the
   `ldc` record already relies on exactly that and records without the guard.
   Tomcat webapps, Spring forked-loader tests, Groovy scripts and every app
   server run all of their application code under user loaders, so the caches
   do nothing precisely where resolution (a Java `loadClass` upcall) is most
   expensive.
3. **Failures are not recorded** — see
   `docs/internal/fixed-bugs/interpreter-L2-class-resolution-failures-are-not-recorded-FIXED-20260923.md`.

And for `ldc` specifically: every execution, hit or miss, takes the
`resolution_cache` `RwLock` and a hash probe (`cached_cp_constant`), where
HotSpot's `fast_aldc` is one indexed load from the class's
`resolved_references` array.

## Design

Give each `Class` a lazily allocated, append-only side table indexed by CP
index:

```rust
struct ResolvedCp {
    // one slot per CP index; written once, read lock-free
    slots: Box<[AtomicU64]>,   // encoded: 0 = unresolved, tag bits + ClassId / error id
}
```

* Class entries store the resolved `ClassId` (or an index into a small
  per-class error table holding `(error class, message)`).
* `ldc` String/Class/MethodType/MethodHandle/condy results store an index into
  a per-class `resolved_references: Vec<ObjectRef>` that the collector scans
  and remaps as a root set (it already scans the condy map; this replaces that
  map's lock with an array).
* Writes are a compare-and-swap from 0, so racing resolvers agree on ONE answer
  — which is also what JVMS requires and what per-thread caches cannot give.
* Invalidation: redefinition (drop the table; the existing latch already
  disables site caches), `upgrade_synthetic_class` /
  `recompute_subclass_layouts` (bump a per-class generation; readers compare).
  Class unloading drops the table with the class.

Then `new`'s extra facts (initialized, `num_fields`, access check passed) stay
in the per-thread `ClassSiteCache` as today, keyed by the now-cheap class
resolution; `checkcast`'s receiver memo stays per-thread.

## Staged plan

1. Build `ResolvedCp` for class entries only; have `resolve_class_loader_aware`'s
   callers in `opcodes.rs` (`op_new`, `op_checkcast`, `op_instanceof`,
   `anewarray_component`) probe it first and fill it on success, for ALL
   referencing classes (loader-namespaced included). Keep the per-thread caches
   as a layer above it.
2. Record failures (LinkageError class + message) and rethrow them.
3. Move `ldc` onto `resolved_references`; quicken the decoded instruction
   stream's `Ldc` to an index into it (the quickened-instruction table in
   `interpreter.rs` already exists).
4. Retire `CastSiteCache`'s target half and the `ldc` record in
   `resolution_cache` once the table carries both.

## Expected benefit

One resolution per (class, CP index) per VM instead of per thread; caching for
user-loader code (today: a `loadClass` upcall per `new`/`checkcast` on a miss
path that never warms); a lock-free `ldc`. Measure with a webapp-shaped
workload (classes defined by a user `URLClassLoader`) and
`CRATONVM_DBG_FIELD_SITE=1`'s `reject_loader` counters, which count exactly the
executions this would serve.

## Risk

Medium–high: it moves resolution identity from "whatever this thread computed"
to "the first answer, for everyone", which is the JVMS rule but a behaviour
change for code that depended on two threads resolving differently (a bug on
HotSpot too). Stage 1 behind a gate, with difftest's interp modes and the
forked-loader Spring suites as the acceptance set.

## Progress (wave 10)

* **One more reason for stage 1, measured by reading, not timed.** Since
  wave 10 the JVMS §5.4.4 class-access check runs in `--compatible` too
  (`constants::check_class_constant_access`,
  `docs/internal/fixed-bugs/interpreter-L2-class-access-checked-only-by-new-FIXED-20260925.md`).
  A site the cast / `new` tables admit pays it once, at the fill. A site
  they refuse — every `checkcast` / `instanceof` / `anewarray` in a class
  whose loader namespace makes `referencing_class_has_loader_namespace`
  true — now pays it on every execution: one more `class_manager` read lock
  and the verdict, on top of the `resolve_class_loader_aware` those sites
  already repeat. Before wave 10 that was one config read under
  `--compatible`. A per-class resolved pool that records the access verdict
  with the resolution removes both.

## Progress (wave 41) — lane L5: carries the i25-L5 racing-entry remainder

`i25-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing`
was closed by wave 41 (lane L5) into
`docs/internal/fixed-bugs/interpreter-L5-a-racing-resolution-of-one-class-entry-can-leave-threads-disagreeing-FIXED-20261005.md`:
fixes 1–3 are in under `--jdk-only` (the loader lock, the success that meets
a recorded failure throws it, the refusal that finds a published winner takes
it; `L5W37RacingEntryOutcome`, `L5W39RacingOwnerOutcome`), and the one window
left is this proposal's (b). The evidence, as that page's "Progress (wave
40)" traced it, from the code:

1. T_lose's drive fails; the checked door re-reads the loader's own
   definition and initiating record: nothing yet.
2. T_win's drive succeeds and publishes the loader's definition (or its
   initiating record).
3. T_win's success site re-checks for a recorded failure (fix 1): none yet,
   so it uses and caches the class.
4. T_lose's opcode records its error.

From then on T_win's site cache says one thing and every other thread the
other. Closing it needs the two publications ordered against each other
(a store-load race on two locations: a common lock or a `SeqCst` pair on
EVERY slow-path class-resolution success), which a `ResolvedCp` slot written
by one CAS from 0 with either a class id or an error index gives for free.
Reachable only by a loader answering two concurrent requests differently,
inside that four-step interleaving; no probe forces it (the two existing
probes force the orders fixes 1 and 2 cover). An acceptance probe for the
implementer: `L5W37RacingEntryOutcome`'s latches with T_win's `loadClass`
released between T_lose's re-read and its record (a hook, since Java cannot
reach that point). `--compatible` has none of fixes 1–2 (its failed
`loadClass` falls back to the global store); applying them there is the
owner's call, as for the other `--compatible` loader items.
