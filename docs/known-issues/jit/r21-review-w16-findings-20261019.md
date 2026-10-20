# JIT round 21 wave 16: adversarial review of waves 13-15 (lane review-fuzz5)

Status: OPEN — 7 items: 1 (medium, fail-open), 2-5 and 7 (low), 6 (low, perf); 5 FIXED in wave 17 (pending build). Nothing was fixed in place: this
lane may not edit `.rs` files. Wave 17: 6 and 7 FIXED (pending orchestrator build; see the table). Wave 17
(lane finalize-reinit): 1, 3 and 4 FIXED, 2 fixed in its fail-open direction (pending orchestrator build).

Scope: `git diff e1aff0645e fd8cb74135 -- '*.rs'`, read at HEAD 52cec37b7b. The areas were:

- the megamorphic stub service identity (`jit/src/runtime_lowering.rs`);
- the finalizer compile bar: `vm/src/runtime/interpreter/exception_dispatch.rs`, the
  `ClassStore::finalizable_ancestors` set in `classloading/src/class.rs`, the define listener
  `SharedVm::note_finalizable_define` (`vm/src/vm/vm_init.rs`), `JitRealm::finalizer_bar_refusals` and its
  prune in `vm/src/memory/gc.rs`;
- the hidden-class paths: the verifier's `hidden_self_named_pool`, `jit_hidden_self_owner` in
  `vm/src/jit/helpers.rs`, and `HiddenClassBits` in `classloading/src/class_manager.rs`;
- the spoofed `InnerClasses` check (`native-builtins/src/lang_class.rs`), `splice_names_declaring_class` and
  `inlined_callee_dependency_class`.

This was a review by reading only. Nothing was built or run. Line numbers are those of the working tree at the
time of writing; other wave-16 lanes are editing some of these files, so the lines may move.

## Item status

| Item | Severity | Verdict |
|---|---|---|
| 1 a finalizable class loaded as a superclass is never listened to | Medium (fail-open, HotSpot divergence) | FIXED (wave 17, lane finalize-reinit; pending orchestrator build): `vm/src/runtime/interpreter/exception_dispatch.rs:861` `finalizable_define_ancestors` walks past a finalizable superclass (`CRATONVM_FINALIZER_ANCESTOR_WALK_PAST_FINALIZABLE`, default ON, `SharedVm` field `vm/src/vm/vm_init.rs:2100`); test `exception_dispatch.rs:6440` |
| 2 the ancestor set is fixed at define; a redefined ancestor is not re-evaluated | Low (fail-open, needs RedefineClasses) | FIXED, fail-open direction (wave 17, lane finalize-reinit; pending build): the listener `vm_init.rs:11465` records every reached ancestor and withdraws only the stale ones (`CRATONVM_FINALIZER_ANCESTOR_RECORD_REACHABLE`, default ON, `vm_init.rs:2107`); test `exception_dispatch.rs:6440`. The perf direction (`JitRealm::finalizer_bar_refusals` never re-evaluated after a redefinition): FIXED, opt-in `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH=1` (wave 18, lane finalize-reinit; pending orchestrator build). Each row is stamped with `JitCache::redefine_epoch` (`vm/src/vm/realms/jit_realm.rs` `finalizer_bar_refusals`), and `exception_dispatch.rs` `finalizer_bars_compiling_memoized` re-asks a row of another epoch; `SharedVm` field `finalizer_bar_refusal_epoch`; test `exception_dispatch.rs:6785` `the_gate_memoizes_a_refusal_per_class_id_and_rereads_the_record` (tail) |
| 3 the legacy JNI `DefineClass` skips the finalizable-define listener | Low (fail-open, kill-switch path) | FIXED (wave 17, lane finalize-reinit; pending build): `vm/src/native/jni.rs:13499`; source pin `vm_init.rs:27717` |
| 4 an empty `finalize()` makes a class finalizable (HotSpot: not finalizable) | Low (pre-existing divergence, both modes) | FIXED (wave 17, lane finalize-reinit; pending build): `classloading/src/class_manager.rs:5743` `class_has_finalizer_by_hotspot_rule`, applied at the define (`:11836`) and at the stub upgrade (`:17781`, `:17858`); switch `CRATONVM_FINALIZER_EMPTY_IS_NONE`, default ON (`ClassManager` field `:6155`); test `:31375`. The rule (an empty override is no finalizer and cancels an inherited one) is from memory of JDK 25's `classFileParser.cpp`, because no HotSpot sources are on this machine: confirm it with a HotSpot run (a subclass with `protected void finalize() {}` under a finalizable parent is never finalized). Wave 22 (lane finalize): the HotSpot half is confirmed. The header of `tools/probes/jit/r21/R21FinalizeEmptyOverride.java` records the run on 25.0.3 (Serial and G1): an empty override is not registered, an empty own `finalize()` is not registered, and an override of an empty `finalize()` is registered. This lane did not re-run the probe's CratonVM arms (no runs allowed) |
| 5 the spoof-verdict memo relies on `InnerClasses` never changing | Low (latent; pre-existing redefinition divergence) | FIXED (wave 17, lane innerclasses; pending orchestrator build): `classloading/src/class_manager.rs:14115` `redefinition_inner_tables`, installed at :15228; `native-builtins/src/lang_class.rs:38471` `forget_redefined_inner_class_answers`, called at `vm/src/vm/vm_exec.rs:18237` (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`, default ON) |
| 6 the finalizable-subtype withdrawal matches constructors by name across loaders | Low (perf) | FIXED, opt-in `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID=1` (wave 17, lane jit-selfcopy; pending orchestrator build): `jit/src/lib.rs:39896`, test `:40166` |
| 7 name-keyed withdrawals miss inherited constant-pool splices since wave 15 | Low (rare path) | FIXED, default ON `CRATONVM_JIT_SPLICE_DECLARING_CLASS_REPLACE_DEP` (`=0` off; wave 17, lane jit-selfcopy; pending orchestrator build): `jit/src/lib.rs:21454`, `:21741`, `:39968`; tests `:23025`, `:40067` |

## 1. A finalizable class that is loaded only as a superclass is never listened to (Medium, fail-open)

- **Where:** `vm/src/runtime/interpreter/exception_dispatch.rs:842`, in `finalizable_define_stale_ancestors`:
  `if ancestor.has_finalizer || ancestor.superclass.is_none() { break; }`. The listeners are
  `vm/src/vm/vm_init.rs:11833` (the `load_class_concurrent_for` listener, which runs for the REQUESTED
  `class_id` only) and `vm/src/vm/vm_exec.rs:22935` (`define_class_full`).
- **The assumption that fails:** the walk stops at the first finalizable superclass. Its comment says that
  superclass's "define already settled every class above it". That holds only when that superclass's own define
  reached a listener. The class manager loads superclasses recursively, inside the load the listener reports:
  `resolve_supertype` (`classloading/src/class_manager.rs:11242`) and `self.load_class(super_name)`
  (`:17379`). No listener ever sees the id of a superclass loaded that way.
- **Failing scenario:**
  - The classes are `class TBase { TBase(String s) { this(Integer.parseInt(s)); } TBase(int v) {..} }`,
    `class TFin extends TBase { protected void finalize() {..} }` and `class TLeaf extends TFin`.
  - The program warms `new TBase("x1")` until `TBase.<init>(String)` is compiled. Then it names `TLeaf` for the
    first time, and the load of `TLeaf` loads `TFin` as its superclass.
  - The listener runs for `TLeaf` only. `TLeaf.has_finalizer` is inherited (`class_manager.rs:11795`), so the
    walk starts at `TFin`, finds `has_finalizer`, and breaks. `TBase` is never recorded, so
    `finalizer_bars_compiling` keeps answering `false` for `TBase.<init>(String)`, and the body compiled before
    `TLeaf`'s define is never withdrawn.
  - `new TLeaf("bad")` then throws a NumberFormatException from the COMPILED `TBase(String)`, before
    `Object.<init>`. The unwinder has no interpreted `TBase` frame to prove the object unconstructed, so the
    registration made at allocation stays, and `finalize` runs on an object that HotSpot never registered
    (RegisterFinalizersAtInit).
- **Which modes:** every VM-native load, which is `--compatible` application classes and every bootstrap or
  platform class. Under `--jdk-only` an application class loader that delegates through
  `ClassLoader.loadClass` defines `TFin` through its own `define_class_full`, which runs the listener. So the
  defect shows mainly in `--compatible`, and in `--jdk-only` for JDK-internal finalizable hierarchies.
- **Probe:** `tools/probes/jit/r21/fuzz/R21Fz5FinTransitive1.java`. HotSpot prints `uncon=0` on every line. The
  `transitive` rows are the defect, and the `control` rows (the finalizable class is named directly) are the
  control.
- **Fix sketch** (either one; behind a new correctness switch, for example
  `CRATONVM_FINALIZER_ANCESTOR_WALK_PAST_FINALIZABLE`, default ON):
  - (a) Do not break at a finalizable ancestor. Skip recording it, because its own rule already covers its
    constructors, and keep walking to `java/lang/Object`. `mark_finalizable_ancestor` is idempotent, and the
    cost is one hierarchy walk per finalizable define.
  - (b) Run `note_finalizable_define` for every id the load defined, which is the range from
    `class_store.next_id()` before the load to `next_id()` after it, as `define_withdraws_by_name` already
    measures.
  - (a) is simpler and also covers the hidden and `Unsafe` define paths without auditing them.
  - Unit test: a hand-built `Base` / `Fin` / `Leaf` hierarchy in which only `Leaf` is passed to
    `note_finalizable_define`. Assert that `finalizer_bars_compiling(Base, "<init>", "(Ljava/lang/String;)V")`
    is `true`.

## 2. The ancestor set is decided once, at define; a redefined ancestor is not re-evaluated (Low, fail-open)

- **Where:** `vm/src/runtime/interpreter/exception_dispatch.rs:816` (`finalizable_define_stale_ancestors`
  records an ancestor only when it already has a constructor that can throw before its initializing call), and
  `:745` (`finalizer_bars_compiling_in` refuses only recorded ancestors).
- **Failing scenario:** `F extends B` is finalizable, and `B`'s constructors cannot throw before `super()`, so
  `B` is not recorded. An agent's `RedefineClasses` then replaces `B.<init>(String)` with a body that throws
  before `super()`. JVMTI allows any change to a method body, and `redefine_class_typed` replaces `methods`. The
  redefinition withdraws `B`'s old bodies, but the recompile asks the bar. The bar answers `false` because `B`
  is not in the set, so the new throwing body compiles, and an `F` object whose new `B` constructor throws keeps
  its registration. HotSpot never registers it.
- **The other direction (perf only, fail-closed):** `JitRealm::finalizer_bar_refusals` is never re-evaluated
  after a redefinition. A finalizable class whose redefined constructor no longer throws before `super()` stays
  interpreted for the life of the VM.
- **Fix sketch:**
  - Record REACHABILITY rather than the current bytes. Every non-finalizable ancestor below the walk's end goes
    into `finalizable_ancestors`, whatever its constructors do. The bar already reads the current bytes through
    `class_ctor_may_throw_before_initialization`, and the `jit_gate_pass` memo is redefine-epoch guarded. Keep
    the "can throw" filter for the withdrawal list only.
  - Cost: one `finalizer_bars_compiling_cold` call per constructor compile of a recorded ancestor, and none
    for any other class.
  - For the refusal memo, stamp each row with `jit_cache.redefine_epoch()`, as `jit_gate_pass` does, and
    recompute on a mismatch.

## 3. The legacy JNI `DefineClass` skips the finalizable-define listener (Low, fail-open)

- **Where:** `vm/src/native/jni.rs:13487` (`cm.define_class(..)` in `jni_define_class`'s legacy arm). After
  `drop(cm)` it runs `define_withdraws_by_name` and `invalidate_for_class`, but not
  `shared.note_finalizable_define(cid)`.
- **Scenario:** `CRATONVM_JNI_DEFINECLASS_LOADER=0` (either mode), or a JNI caller with no attached
  `JvmThread`, which falls through to the legacy define. A native agent defines a finalizable subclass of an
  already-compiled base whose constructor throws before `super()`, and then the same failure as item 1
  follows.
- **Fix sketch:** call `shared.note_finalizable_define(cid)` right after `drop(cm)`. Extend the source-pin test
  `define_class_full_runs_the_finalizable_listener` (`vm/src/vm/vm_init.rs`) to `jni_define_class`.

## 4. An empty `finalize()` makes a class finalizable; HotSpot treats it as no finalizer (Low, pre-existing, both modes)

- **Where:** `classloading/src/class_manager.rs:11791-11795` (`has_finalizer = self_declares || parent_has`)
  and `classloading/src/class.rs` `declares_finalize`, which checks only the name and descriptor.
- **HotSpot:** `ClassFileParser` sets `_has_empty_finalizer` for a `finalize()V` whose code is a lone `return`
  (`Method::is_empty_method`). `set_precomputed_flags` then sets `has_finalizer` only when
  `!_has_empty_finalizer && (_has_finalizer || super->has_finalizer())`. So an empty override is not
  finalizable, and it also CANCELS a finalizer it would otherwise inherit (this is from memory of
  `classFileParser.cpp`; check it against the JDK 25 sources before you implement it).
- **What differs:**
  - CratonVM registers every instance and runs the empty body. That is invisible to the object itself, but it
    keeps the object reachable for one more GC cycle, which delays the enqueue of a `PhantomReference` to it.
  - Since wave 11 the finalizer compile bar also keeps such a class's throw-before-init constructors
    interpreted, and since wave 13 its ancestors' constructors too. That is perf only.
  - Common real shape: a subclass that overrides a library class's `finalize()` with `{}` to opt out.
- **Fix sketch:** compute "declares a non-empty `finalize()V`" and "declares an empty one" (the code is exactly
  `[0xb1]`). Then `has_finalizer = !self_empty && (self_nonempty || parent_has)`. Put it behind a correctness
  switch, default ON. Run the finalizer probes (`R21Fz3Fin*`, `R21Fz5FinTransitive1`) and the
  `vm/src/vm/tests.rs` finalize tests on both arms.

## 5. The spoof-verdict memo relies on `InnerClasses` never changing for a class id (Low, latent)

- **Where:** `native-builtins/src/lang_class.rs:38510` ("both tables are the classes' parsed attributes, fixed
  for the life of the class ids"). The verdicts are kept in `ReflectDoorCaches::spoof_verdicts`.
- **Why it holds today:** `redefine_class_typed` PRESERVES `inner_classes` and `enclosing_method`
  (`classloading/src/class_manager.rs:14851`).
- **Why that is itself a HotSpot divergence:** HotSpot's `VM_RedefineClasses::redefine_single_class` swaps the
  scratch class's `inner_classes` array into the redefined class. After a redefinition that changes
  `InnerClasses`, HotSpot's `getDeclaringClass`, `getSimpleName`, `isMemberClass` and its
  `check_for_inner_class` ICCE answer from the NEW table, and CratonVM answers from the old one.
- **The trap:** whoever fixes that divergence must also drop the `spoof_verdicts` rows keyed by the redefined
  class, as the inner half of the key and as the outer half. Otherwise a verdict computed from the old table
  answers forever.
- **Fix sketch:**
  - For now, a comment at the memo naming the dependency, so that the redefinition fix finds it.
  - Later, a redefinition listener that retains only the rows whose key does not name the class (`key >> 32`
    or `key & 0xFFFF_FFFF`).
  - A probe needs an agent (`Instrumentation.redefineClasses`) with two versions of the outer class's bytes.
- **FIXED in JIT round 21 wave 17 (lane innerclasses), pending orchestrator build.** Switch
  `CRATONVM_REDEFINE_SWAPS_INNER_CLASSES` (correctness, both modes, default ON, `=0` keeps the old tables).
  - HotSpot behaviour followed:
    - `redefine_single_class` swaps the scratch class's `inner_classes` array, which also holds the
      `EnclosingMethod` indices.
    - A changed `InnerClasses` is never refused. `compare_and_normalize_class_versions` checks only `NestHost`,
      `NestMembers`, `Record` and `PermittedSubclasses`, which is the set the JDK's own
      `ClassAttributeChanged` message names.
    - The scratch class's parser applies the define path's `InnerClasses` verdict under `_need_verify` (every
      loader but the boot loader). `Class is both outer and inner class` and `Duplicate entry in InnerClasses
      attribute` refuse with `INVALID_CLASS_FORMAT` (`ClassFormatError`). An ignored (circular) table installs
      as empty.
    - These points come from memory of `jvmtiRedefineClasses.cpp`: no HotSpot C++ source is on this machine.
  - `class_manager.rs:14115` `redefinition_inner_tables`:
    - computes the new tables;
    - is asked by `check_redefinition` too, so a batch refuses before any class of it changes;
    - is installed after verification (:15228), so the rollback never has to restore the tables.
    - Hidden classes keep their table. The invariant snapshot no longer pins `inner_classes` /
      `enclosing_method`.
  - `lang_class.rs:38471` `forget_redefined_inner_class_answers`, called by the VM after every redefinition
    (`vm_exec.rs:18237`), clears:
    - the whole spoof memo, because a `SPOOF_BY_INNER` row depends on a class its key does not name;
    - the VM's simple and canonical name caches, because a member's canonical name embeds its outer's.
      HotSpot discards `reflectionData` the same way.
    - the redefined class's modifiers row.
  - Tests:
    - `classloading/tests/redefine_inner_classes.rs`: the swap, and the format refusal by both the check and
      the redefinition;
    - `native-builtins` `lang_class::tests::r21w17_*`: the cache clear per VM, and a source pin on the VM call.

## 6. The finalizable-subtype withdrawal matches constructors by name across loaders (Low, perf)

- **Where:** `jit/src/lib.rs:39809`, `withdraws_for_finalizable_subtype`: `&*key.class_name == class_name`
  (OR-ed with the id test), and the `inlined_methods` arm, which matches by name only.
- **Scenario:** two loaders each define `p/Base`, for example a plugin host or a servlet container with
  several web apps. A finalizable subclass in one loader withdraws, and records an invalidation that refuses
  in-flight compiles for, every compiled or spliced `p/Base.<init>` of every loader. The bar itself is
  id-keyed, so the other copies recompile normally and only churn.
- **Fix sketch:** when `class_id != 0`, match `key.declaring_class_id == class_id` alone, and match the
  spliced arm through `copied_bytecode_of(class_id)`, which is already OR-ed in. Keep the name arm for
  `class_id == 0`.
- **FIXED (JIT round 21 wave 17, lane jit-selfcopy; pending orchestrator build), opt-in
  `CRATONVM_JIT_FINALIZER_WITHDRAW_BY_ID=1`.** It is a perf fix that narrows a withdrawal, so it ships off.
  `jit/src/lib.rs:39896`, `withdraws_for_finalizable_subtype`: with a non-zero `class_id`, the own-constructor
  arm matches `key.declaring_class_id == class_id`. The name is used only for a key minted with id `0`. The
  spliced arm is `copied_bytecode_of(class_id)`. The `inlined_methods` name match is kept only for a body
  whose `copied_classes` is empty: it was built with no compile scope, so its record cannot speak for its
  splices. Elsewhere the VM resolver marks every spliced body's declaring class (`jit_bridge.rs`
  `note_bytecode_copied(declaring_id)`). The log record uses the same function. Test:
  `r21_finalize_subtype_invalidation_tests::by_id_a_same_named_class_of_another_loader_keeps_its_constructors`
  (`:40166`). Still owed: the finalizer probes A/B with the switch.

## 7. Name-keyed withdrawals miss inherited constant-pool splices since wave 15 (Low, rare path)

- **Where:**
  - `jit/src/lib.rs:21412`, `inlined_callee_dependency_class`. Since wave 15 a constant-pool splice of an
    inherited body records `InlinedCallee` under the CONSTANT-POOL class (`K3`), not the declaring class
    (`A`).
  - `jit/src/lib.rs:42316`, `JitCache::invalidate_for_class`, which matches `inlined_methods` by name only and
    does not consult `copied_classes`.
- **What still works:** the wave-15 note says the declaring-class dependency is kept through
  `copied_classes`. That is true for the id-keyed scans: redefinition (`depends_on_redefined_class`,
  `redefinition_makes_stale`) and unload.
- **What no longer works:** the by-name define withdrawals call `invalidate_for_class(name)` when a define
  REPLACES a class of that name. That happens on an in-place stub upgrade or an `allow_redefine` re-key
  (`vm/src/vm/vm_exec.rs:22357`, `:22418`, `:22459`, `:22937`; `vm/src/native/jni.rs:13503`). They no longer
  see a caller that spliced `A.d1` through `K3`. Before wave 15 the triple named `A` and matched.
- **Severity:** low. A stub has no bytecode to splice, so the window is an `allow_redefine` re-key of a class
  whose inherited method was spliced through a subclass constant pool.
- **Fix sketch:** in `invalidate_for_class`, also withdraw the bodies whose `copied_classes` contain the id
  that the name had before the define. The callers have it: `next_id_before` and the replaced id. Or record a
  second, non-subtype-sensitive marker triple for the declaring class, which
  `names_subtype_sensitive_member` already skips for `<init>` and `<clinit>` only, so it would need its own
  tag.
- **FIXED (JIT round 21 wave 17, lane jit-selfcopy; pending orchestrator build), default ON
  `CRATONVM_JIT_SPLICE_DECLARING_CLASS_REPLACE_DEP` (`=0` records the wave-15 triple alone).** This takes
  the second route, because the by-name callers in `vm_exec.rs` and `jni.rs` are outside the lane's file.
  - When `inlined_callee_dependency_class` names the site's class, `plan_inline` (`jit/src/lib.rs:21741`)
    also records `(declaring class, DECLARING_CLASS_REPLACE_MARKER, descriptor)` (`:21454`, marker
    `"<declaring-class>"`). JVMS 4.2.2 forbids `<` in other method names, so the marker never names a
    real member.
  - `invalidate_for_class(A)` and `invalidate_for_class_change(A)` match the marker by name.
  - `names_subtype_sensitive_member` skips it (`:39968`), so a new subtype of `A` still does not withdraw
    the caller.
  - The finalizable match looks for `<init>` only.
  - All three planners (single-pass, IR, OSR) pass `site_class_name`, so all get the marker.
  - Tests: `a_constant_pool_splice_depends_on_the_class_the_site_names` (`:23025`, updated for the
    marker) and `i88_l7_new_subtype_invalidation_tests::the_declaring_class_marker_answers_replacement_but_not_a_new_subtype`
    (`:40067`).

## Checked and found sound

- **`HiddenClassBits`.** The bit is set before `class_store.add` (`class_manager.rs:12221`) and by
  `mark_class_hidden`. The native `set_class_hidden` door now goes through it (`vm_exec.rs`). The other
  `hidden = true` writes are test fixtures. The bit uses `Release`/`Acquire` ordering, ids are never reused
  (tombstones), and an id past the capacity answers `None`, which means "ask under the lock". The bitmap is
  per class manager, so there is no process global.
- **`jit_hidden_self_owner` on spliced sites.** A spliced call's `JitInvokeInfo::declaring_class_id` is the
  inlined callee's declaring class (`InlineInvokeTarget.declaring_class_id: declaring_id`,
  `vm/src/runtime/interpreter/jit_bridge.rs:33198`), so a hidden body spliced into an ordinary caller still
  answers its own self-references by id. A `[L<self>;` constant-pool entry is not treated as self, which
  matches HotSpot (only `this_class` is re-pointed).
- **The megamorphic stub service identity.** The identity store sits on the cold side, after `JNO`, and no
  flag is live there. The lean path and the service path are mutually exclusive (`service_identity` is filtered
  on `lean_identity.is_none()`). The id is the caller's own, and the caller's buffer holds the stub.
- **The gate memo race.** The listener records under the class-manager write lock and THEN drops the
  `jit_gate_pass` rows. The gate inserts its row and then re-reads `finalizer_ancestor_recorded` (`interpreter.rs`
  ~2691). One of the two always sees the other.
- **`finalizer_bar_refusals`.** It is keyed by class id, pruned by the unload sweep (`gc.rs`), and takes no
  lock across the bar computation.
- **`frame_locals_hold` and `entry_is_jni_upcall`.** A handler starts with an empty operand stack, so only a
  local can carry the uninitialized reference to a retry. The JNI row comparison also includes the JIT entry
  depth.
- **AGENTS.md.** No new native registrations, no new process statics, and no new class-name allow-lists in
  non-test code in the range. The finalizer switches moved to `VmConfig`, which is per VM.
