# JIT round 21 wave 21: adversarial review of waves 18-20 (lane review-w21)

Status: OPEN. There are 10 items. Items 1, 6 and 7 are low-medium. Items 2-5 and 8 are low; item 5 is mostly
performance. Items 9 and 10 list what was reviewed and found clean. Nothing was fixed, because this lane may not edit
`.rs` files.

Scope: `git diff 30635cad5e 57a8eae8cd -- '*.rs'` (waves 18-20), read at HEAD 57a8eae8cd. The review was by
reading only: nothing was built or run. Line numbers are HEAD's.

## Item status

| Item | Severity | Verdict |
|---|---|---|
| 1 deopt resume of an unhinted scope still matches a hidden class's own name by exact string and by name | Low-medium (wrong-class resume once a same-named class is loaded) | FIXED (wave 21, lane deopt-resume; pending orchestrator build): `vm/src/runtime/interpreter/deopt_resume.rs:877` step 1 and the switch-off arm, `:968` CP scan, `:835` supertype walk, `:688` hinted arm, all through `hidden_self_cp_answer` / `find_cp_class_for_class` (`CRATONVM_HIDDEN_SELF_CP_BY_ID`, per-VM latch); test `:18624` |
| 2 by-name owner lookups on constant-pool names that wave 20 did not convert | Low | FIXED, pending orchestrator build (wave 21, lane hidden-self-resolve): `jvmti_events.rs` `:2990`, `:3147`, `:3323` use `find_cp_class_for_class` |
| 3 hidden-class mint sites that break the `<class-file name>/0x<hex>` rule wave 20 relies on | Low (unreachable on JDK 25, or on a parse-failure path) | FIXED, pending orchestrator build: the class manager records each hidden class's `this_class` at define (`ClassManager::hidden_class_file_name`, which since wave 22 reads `ClassStore::recorded_file_names`, `classloading/src/class.rs:1607`; the wave-21 map `hidden_class_file_names` is gone, w22 item 2), and the rule reads it first, so no mint spelling matters for a defined-hidden class; `unsafe_natives.rs` `defineAnonymousClass` mints from the parsed `this_class`. Residual: a class marked hidden after its define (`mark_class_hidden`) and the pure `is_self_class_reference` in `dispatch_static.rs` still read the stored suffix. Residual FIXED (wave 22, lane hidden-resid; pending orchestrator build): `classloading/src/class_manager.rs` `mark_class_hidden` records the `this_class` an ordinary renamed define kept (in `ClassStore::recorded_file_names`, read through `class_store.recorded_file_name(id)`; the separate `renamed_class_file_names` map was merged there in wave 22, w22 item 2; name fix wave 24, item 9 of `r21-review-w24-findings-20261019.md`), and `vm/src/runtime/interpreter/dispatch_static.rs` `self_class_id` asks `is_self_class_reference_in` (`CRATONVM_HIDDEN_SELF_RECORDED_NAME`, default ON, per VM). A class defined under its own `this_class` and marked later keeps the suffix fallback (its exact-name self test already matches). Tests: `class_manager::tests::a_class_marked_hidden_after_its_define_records_its_class_file_name`, `invoke::hidden_self_cp_tests::dispatch_static_self_test_reads_the_recorded_class_file_name` |
| 4 three copies of the hidden self-name rule, and they disagree | Low (maintainability; today's disagreement fails closed) | FIXED, pending orchestrator build: one rule, `cratonvm_classloading::hidden_self_name_rule` (recorded name, else `hidden_stored_name_is_class_file_name`); `constants::hidden_stored_name_is_self` and `exception_dispatch.rs` `ctor_target_may_initialize::names` (pure, no class manager) call its suffix half, the only copy; the self doors with a guard (`hidden_self_reference[_in]`, `is_self_class_reference_in`, `hidden_self_cp_answer`) ask `ClassManager::is_hidden_self_name` |
| 5 the switch's reach, the eager `invokespecial` probe, and ids outside the bitmap | Low (perf, semantics of `=0`) | FIXED, pending orchestrator build: `=0` reach documented on `HiddenClassBits::self_cp_by_id` and the wave-20 page; the `invokespecial` probe is lazy, in its arm; ids past the bitmap answer known-ordinary (lock-free `past_capacity` latch) until a hidden class lives there |
| 6 the sticky outer-resolution memo: unload and id reuse, a descriptor caller, and the lost cause | Low-medium | FIXED (wave 21, lane innerclasses; pending orchestrator build): (a) `lang_class.rs:39370` `forget_unloaded_reflect_door_rows`, called from `vm/src/memory/gc.rs:671`; (b) `:39351` `class_seen_by_referencing_class_unrecorded` for the record-component descriptor; (c) `:16921` `RecordedResolutionFailure` keeps the cause, set on the rethrow |
| 7 the `--compatible` `getSimpleName` / `getCanonicalName` bridges ask the declaring-class check before the nesting kind | Low-medium (`--compatible`, inconsistent `InnerClasses`, which obfuscators produce) | FIXED (wave 21, lane innerclasses; pending orchestrator build): `lang_class.rs:38965` `name_bridge_asks_declaring_class` gates the ICCE, outer-error and agreement checks of both bridges (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`); probe `R21Fz7InnerRowsEm1` |
| 8 the `<init>`-return check refuses more than local-0 writes, and allocates per return | Low (fail-closed; JEP 513 prologues) | FIXED (wave 21, lane finalize-reinit; pending orchestrator build). `vm/src/runtime/interpreter/exception_dispatch.rs:447` `ctor_body_keeps_receiver_and_calls_before` is a dedicated allocation-free linear decode, called from `:431`. It refuses only local-0 writes in every form and subroutines (`jsr`, `jsr_w`, `ret`, `wide ret`), and answers whether an `invokespecial` precedes `return_pc`. It does not examine handlers or branches, and the handler-pc `Vec` is gone. The withdraw side's `ctor_prefix_scan` is unchanged. Tests `:6592` (a `try`/`catch` and a loop before `super(..)`, a `tableswitch`, a truncated body) and `:6547` (subroutine cases). Not done: memoizing the answer per method needs a cell on `CachedBytecodeMethod` or `Frame`, both outside the lane, so the decode still runs once per finalizable constructor return. FIXED (wave 22, lane finalize; pending orchestrator build), opt-in `CRATONVM_FINALIZER_INIT_RETURN_SCAN_MEMO=1` (`VmConfig::finalizer_init_return_scan_memo`): `exception_dispatch.rs` `ctor_return_names_its_initialized_receiver_in` decodes once per method body (`ctor_body_receiver_facts`: refused, no call, or the first `invokespecial` pc; `ctor_facts_answer` gives the per-return answer) and keeps it in `reader/src/quickened.rs` `QuickenedCode::ctor_receiver_facts`. That is the stream the frame's `CachedBytecodeMethod::quickened` already built, used only when it pins the frame's own code allocation (`Arc::ptr_eq`). A new field on `CachedBytecodeMethod` would have touched ~49 struct literals across four crates, most of them in files other lanes are editing. Redefine-safe. A redefinition that changes the body gets a new code allocation (`frame.rs` `padded_bytecode_for_method` checks the bytes and the exception table before it reuses an `Arc`), so a new `CachedBytecodeMethod` and a new stream. One that keeps the bytes shares them, and the answer depends on the bytes alone. A frame on a replaced body fails the identity check and decodes. Test `the_init_return_scan_memo_agrees_with_the_decode` |
| 9 inner-tables generation guard | Info | clean |
| 10 hidden-self matching, lambda proxies, hidden twins, per-VM latch, JIT doors | Info | clean, with notes |

Probes, written without being run. They follow `r21-fuzz5-probes-20261019.md`: default package, ASCII, deterministic
output, and the HotSpot expectation (by reading) in each header.

| Probe | Targets | Verdict |
|---|---|---|
| `tools/probes/jit/r21/fuzz/R21Fz7InnerRowsEm1.java` | item 7: local and anonymous classes whose self row names an outer class that does not list them, or does not exist; a spoofed member as the control | OPEN (run pending; expected to differ on `--compatible`) |
| `tools/probes/jit/r21/fuzz/R21Fz7HiddenSelfLambda1.java` | wave 20 (`CRATONVM_HIDDEN_SELF_CP_BY_ID`) reached through a lambda body and a method reference in a hidden class, hot, before and after the ordinary class loads; two hidden classes from one set of bytes | OPEN (run pending) |

## 1. Deopt resume of an unhinted scope still matches a hidden class's own name by exact string and by name (Low-medium)

- **Where:** `vm/src/runtime/interpreter/deopt_resume.rs:880-886` (`resolve_unhinted_scope_class` step 1: `&*class.name
  == class_name`, then `find_class_by_name_for_class`); `:942-946` (`scope_class_through_member_refs`: `cp_name ==
  &*enclosing.name`, then `find_class_by_name_for_class(cp_name, ..)`); `:874` (the switch-off arm).
- **Scenario:** a hidden class `P/0x..` inlines its own private or static method. The deopt chain row carries no class
  id, and its key spells the constant-pool name `P` (an inlined site keyed by the site's `class_name`). Step 1 compares
  `P` with the stored `P/0x..` and does not match, so it asks the loader for `P`. Before the class path's `P` is loaded,
  the scope is refused (fail closed). After it is loaded, step 1 answers the ordinary `P`. Step 3 reads the hidden
  class's own `Methodref P.step`, makes the same comparison, asks the loader again, and gets the same ordinary `P`. The
  two answers agree, so the frame resumes in the ORDINARY class's body, with its statics and privates. This is the
  `R21Fz5HiddenOsr1` family, reached through the deopt door. The wave-16 test
  `a_scope_naming_the_enclosing_hidden_class_resolves_by_id` (`:18337`) keys by the STORED name only, so it does not
  cover this case.
- **Fix sketch:** in both places, use `is_self_class_reference(&class.name, class.is_hidden(), name)` for the self
  test, and `cm.find_cp_class_for_class` for the constant-pool name. Add a test keyed by the class-file name with an
  ordinary same-named class loaded. Put it behind `CRATONVM_HIDDEN_SELF_CP_BY_ID`, which the class manager already
  latches.

**Verdict (wave 21, lane deopt-resume): FIXED (pending orchestrator build).** It is gated by
`CRATONVM_HIDDEN_SELF_CP_BY_ID`, read through the class manager's per-VM latch. No new switch.
- In `vm/src/runtime/interpreter/deopt_resume.rs`, every spot that matched the class name by exact string now also
  goes through `ClassManager::hidden_self_cp_answer` or `ClassManager::find_cp_class_for_class`:
  - step 1 of `resolve_unhinted_scope_class` (`:877`)
  - the `CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER=0` arm
  - the CP scan's class lookup (`:968`)
  - the start of the `supertype_named` walk (`:835`)
  - the hinted `Exact` arm's name check (`:688`)
- The effect: a key or pool entry spelled with the class-file name binds to the hidden class by id.
- Test: `:18624` `a_hidden_class_keyed_by_its_class_file_name_beats_a_same_named_ordinary_class`. An ordinary
  `r21w21/P` and a hidden `r21w21/P/0x1a` are both loaded. The unhinted, scope-by-loader-off and hinted arms all
  answer the hidden class. With the latch pinned off, the ordinary class is answered.

## 2. By-name owner lookups on constant-pool names that wave 20 did not convert (Low)

Wave 20 converted `interpreter.rs:20973` (`native_named_by_invoke`) and `gc_and_alloc.rs:913/999`. The sibling helpers
in `vm/src/runtime/interpreter/jvmti_events.rs` have the same shape (`owner_name = cp.get_class_name(..)`, then
`find_class_by_name_for_class(owner_name, caller)`) and were not converted: `:2990`, `:3147` and `:3323`. For a
hidden caller's self-call, after an ordinary class of that name is loaded, JVMTI then names the ordinary class's
method, or no method at all. These answer the method an invoke at a bci names, so the visible effect is a wrong
`MethodEntry` / frame-pop / single-step attribution under an agent.
**Fix sketch:** `find_cp_class_for_class` at all three, as at the converted siblings.

Reviewed and left alone, because HotSpot resolves these by name too: descriptor-type lookups (`invokedynamic.rs:6294`
`LambdaTypes`, `:7101`, `:8919`, `helpers.rs:25627` `VhConstClassByName` (a call-site return type), `typecheck.rs:1925`),
and receiver-keyed lookups (`helpers.rs:13080`, `:60775`, `:62943`). There is one cosmetic case:
`invokedynamic.rs:7697` builds `caller_desc` from the stored name, so a hidden record whose `ObjectMethods` getter is
a `findSpecial` of itself never equals `owner_desc`. That is a refusal-message nuance for hand-built bytes only.

## 3. Hidden-class mint sites that break the `<class-file name>/0x<hex>` rule (Low)

`hidden_stored_name_is_class_file_name` (`classloading/src/class_manager.rs:4402`) and its doc say "every hidden-class
mint site appends that suffix and nothing else". Five sites do not:

- `native-builtins/src/unsafe_natives.rs:1748` (`defineAnonymousClass`): `{host_prefix}/0x{id:x}`, where the prefix is
  the HOST class name, not the class file's `this_class`. If this site is ever reached, every reference the anonymous
  class makes to its host (the common case) resolves to the anonymous class itself. That is a wrong answer, not a
  missing one. The method does not exist in JDK 17+, so this is unreachable from a JDK 25 library (the synthetic JDK
  only).
- `native-builtins/src/unsafe_natives_ext.rs:1823`: `anonymous/0x{id}`. Self-references never match (missing, not
  wrong).
- `native-builtins/src/lookup_define.rs:1215` and `:1580` (`hidden_class_base_name` falls back to the lookup class name)
  and `native-builtins/src/classloader.rs:16876` (falls back to `"HiddenClass"`), when `this_class` cannot be extracted.
  A host-name fallback would capture the host as above, but the define of such bytes should fail anyway.
- `classloading/src/class_manager.rs:11701`: the collision probe appends a second `/0x{n}` to an already-mangled
  override (`P/0x..01/0x2`), and the matcher refuses that suffix. Unreachable while `HIDDEN_CLASS_COUNTER` stays
  unique.

**Fix sketch:** mint from the parsed `this_class` at every site, or record each hidden class's class-file name
explicitly on the `Class` (an `Arc<str>` set at define time) and compare with it instead of parsing the suffix. The
second option also retires item 4.

## 4. Three copies of the hidden self-name rule, and they disagree (Low)

- `classloading/src/class_manager.rs:4402` `hidden_stored_name_is_class_file_name` (new in wave 20)
- `vm/src/runtime/interpreter/constants.rs:5758` `hidden_stored_name_is_self` (its doc says it is "the same rule")
- `vm/src/runtime/interpreter/exception_dispatch.rs:480` `ctor_target_may_initialize::names`: `rsplit_once("/0x")` and
  a prefix compare, with no hex check. It accepts `A/0x1$Inner` as `A`, and `A/0x1/0x2` as `A/0x1`.

The third copy over-accepts, which is the fail-closed direction for its only use (an `invokespecial` "may initialize").
It is still the kind of disagreeing copy AGENTS.md warns about.
**Fix sketch:** one predicate in `classloading`, called by all three (or the recorded class-file name from item 3).

## 5. The switch's reach, the eager `invokespecial` probe, and ids outside the bitmap (Low)

- **`=0` is partial.** `CRATONVM_HIDDEN_SELF_CP_BY_ID=0` restores the by-name answers only at the wave-20 doors. The
  wave-11 and wave-14 self doors stay on their own switches or none: `hidden_self_reference` (`constants.rs:6178`),
  `is_self_class_reference` (`invoke.rs:757/3382/7929`, `dispatch_static.rs:245`), and `jit_hidden_self_owner`
  (`CRATONVM_JIT_HIDDEN_SELF_BY_ID`, read per call at `constants.rs:5936`). The flag row's text should say so.
- **The latch.** It is per VM (`HiddenClassBits` is built once, at `ClassManager` construction, `class_manager.rs:7068`,
  and shared with `ClassRealm::hidden_classes`, `vm_init.rs:7213`). It is not a process global. The first-read race is
  benign: two first readers both read the environment and store the same value. Only a test's
  `set_self_cp_by_id` can lose to a concurrent first read.
- **Eager probe.** `invoke.rs:3280` evaluates `hidden_self_cp_owner` for every `invokespecial` before the `else if`
  chain that may never reach it. That is one bitmap load per call. **Fix sketch:** move it into the arm at `:3336`.
- **Ids outside the bitmap.** `HiddenClassBits` covers `[0, 1<<20)`. An id at or above that, including the lambda
  proxy ids at or above `0x8000_0000` if any frame or site ever carries one, gets `None` from `contains`. Then
  `hidden_self_cp_owner` takes `class_manager.read_recursive()` and `hidden_self_cp_answer` takes `get_class` on every
  owner lookup, so "ordinary classes pay one bitmap load" does not hold for them. **Fix sketch:** answer a known
  synthetic lambda-proxy id as not hidden-with-a-pool before the lock, or size the bitmap from the id allocator.
- **JIT hint filter (note).** `jit_bridge.rs:25454` (`callee_probe_class_id`) and `:26271` (`hinted_class_id`) drop a
  hint whose stored name is not `class_name`. A hidden id hinted with the constant-pool name therefore falls back to the
  ordinary class by name. Today no wave-20 door passes such a hint, because the wave-14
  `jit_callee_name_is_another_loaders_copy` keeps hidden self sites off these doors. Keep it that way, or compare with
  `is_self_class_reference`.

## 6. The sticky outer-resolution memo: unload and id reuse, a descriptor caller, and the lost cause (Low-medium)

`CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY`, `native-builtins/src/lang_class.rs:39268`
(`recorded_outer_resolution_failure`) and `:39298` (`record_outer_resolution_failure`). The key is
`(referencing class id, name)` (`:16894`).

- **(a) Unload and id reuse.** Rows are removed only by `forget_redefined_inner_class_answers` (`:38660`), and never at
  class unload. Class ids are reused after an unload (see the "class id reused after an unload" guards at `:16706` and
  `:21798`). A new class at a reused id whose `InnerClasses` / `EnclosingMethod` names the same outer name is then
  thrown the dead class's `NoClassDefFoundError`, without its own loader ever being asked. Separately, a VM that keeps
  defining and unloading classes with missing outers grows the map without bound. **Fix sketch:** drop the dead ids'
  rows in the unload sweep, beside the other per-VM reflect-door rows. Alternatively, store the referencing class's
  name and loader in the row and treat a mismatch as a miss.
- **(b) A caller that is not a constant-pool entry.** The doc at `:39243` says every caller resolves an `InnerClasses`
  or `EnclosingMethod` entry, but `:41058` (`getRecordComponents`) passes a record component's DESCRIPTOR type through
  the same door. HotSpot resolves that type per call (`find_java_mirror_for_type` -> `resolve_or_fail`) and records no
  resolution error for it. So with the memo, a record-component type that later becomes loadable keeps throwing.
  Worse, it shares the key with an `InnerClasses` entry of the same name, so a failed descriptor resolution poisons
  `getDeclaringClass` (and the reverse). **Fix sketch:** add a `record: bool` parameter (or a separate door), and record
  only for real `klass_at` callers: `:39179`, `:39193`, `:39369`, `:40484/40498`, `lang_reflect.rs:541`, and
  `classloader.rs:17491`.
- **(c) The cause (verify).** HotSpot's `ConstantPool::throw_resolution_error` re-throws the recorded error WITH its
  recorded cause: `SystemDictionary::add_resolution_error` keeps the cause class and message, so the second throw is
  `NoClassDefFoundError` caused by `ClassNotFoundException`. `:39280-39292` rebuilds class and message only. A probe
  that prints `getCause()` on the second ask would show the difference. Extend `R21Fz6LookupInLoad1` or
  `R21Fz7InnerRowsEm1` (X$9) with `getCause()` and measure.
- Not a defect: an `ldc` of the same entry and this door keep separate first failures (the page says so). HotSpot shares
  them per entry. That diverges only when the loader's answer changes between the two asks.

## 7. The `--compatible` name bridges ask the declaring-class check before the nesting kind (Low-medium)

- **Where:** `native-builtins/src/lang_class.rs:7311-7318` (`native_class_get_simple_name`) and `:37252-37259`
  (`native_class_get_canonical_name`). Both call `inner_classes_icce_for_mirror` (`:38908`, which resolves the declaring
  class through `declaring_member_row_outer` and checks the outer's agreement) and `name_bridge_outer_resolution_error`
  BEFORE the wave-19 `class_nesting_of` decides anything.
- **What JDK 25 does:** `getSimpleName0` -> `getSimpleBinaryName` -> `isTopLevelClass`, which is
  `!isLocalOrAnonymousClass() && getDeclaringClass0() == null`. With `EnclosingMethod` present the `&&` short-circuits,
  so `getDeclaringClass0` (HotSpot `compute_enclosing_class` + `check_for_inner_class`) is never called.
  `getCanonicalName0` returns null for `isHidden() || isLocalOrAnonymousClass()` before it reaches `getEnclosingClass`.
- **Scenario (`R21Fz7InnerRowsEm1`):** X$7 has row `(X$7, X, Seven)` and `EnclosingMethod(X)`, and X does not list it.
  HotSpot answers `simple="Seven" canonical=null`, with ICCE only from `getDeclaringClass`. CratonVM's bridges very
  likely throw ICCE from both. The same holds for X$8 (anonymous, `""` / null) and X$9 (outer `Gone` missing:
  HotSpot answers, the bridge throws NCDFE). Obfuscators that strip the OUTER class's `InnerClasses` while keeping the
  inner's produce exactly the X$7 / X$8 shape. `--jdk-only` runs the JDK body and should match.
- **A related check:** a hidden class defined from member-class bytes (the i102 shape). `getCanonicalName` is null in
  JDK 25 (`isHidden()` comes first), but the canonical bridge's pre-check raises the i102 ICCE before its own hidden
  arm. `L3W102ClassNameShapes` was recorded SAME. Re-read its `hidden-member canonical=` column: if HotSpot printed
  `null`, then either the bridge is not what `--compatible` dispatches there, or the row would show the ICCE.
- **Fix sketch:** in both bridges, compute `class_nesting_of` first. Skip the ICCE and outer-error pre-checks for
  `Local` / `Anonymous`, and for a hidden class in the canonical bridge. Keep them for `Member` and top-level (where JDK
  25 calls `getDeclaringClass0`). Gate this under `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`.

**Wave 21 fixes for items 6 and 7 (lane innerclasses).** These are pending the orchestrator's build; nothing was
built or run.

- **6a.** `forget_unloaded_reflect_door_rows` runs from the unload sweep in `gc.rs`, and the sweep's
  `unload_dead_class_metadata` source-pin list now names it. It drops:
  - the recorded failures of a dead referencing class;
  - every spoof verdict that names a dead class on either side.
  - Note: `gc.rs` says at the multianewarray prune that class ids are NOT reused. The prune bounds the map either way.
- **6b.** The `getRecordComponents` descriptor type goes through `class_seen_by_referencing_class_unrecorded`. It
  is resolved on every call and never recorded. Only the `klass_at` callers keep the recorded door.
- **6c.** HotSpot keeps the cause: `ResolutionErrorEntry` stores the cause's class and message, and
  `throw_resolution_error` throws `THROW_MSG_CAUSE`. This is from memory of the HotSpot source; nothing was
  measured. The row now stores the cause's class and message, and the rethrow builds the cause first and sets it
  as the new error's `cause`.
- **7.**
  - `name_bridge_asks_declaring_class` answers `false` in two cases:
    - for a class (an array: its element) with `EnclosingMethod`;
    - in the canonical bridge, for a hidden class.
  - Both bridges then skip all three of their declaring-class checks: `inner_classes_icce_for_mirror`,
    `name_bridge_outer_resolution_error` and `member_class_inner_classes_disagreement`.
  - The `--compatible` nesting predicates keep their check, which is reached only for Member classes.
- **L3W102 hidden-member canonical.** The probe's recorded output is not on disk; only the SAME verdict is.
  - By JDK 25's source, `getCanonicalName()` of a hidden class is null (`isHidden()` comes first).
  - Before this wave, the canonical bridge raised the i102 ICCE first. So either `--compatible` did not dispatch
    that bridge for the row, or the SAME record predates the ICCE arm.
  - The bridge now answers null either way (`r21w21_hidden_member_canonical_name_skips_the_declaring_check`).
  - Rerun the probe to confirm.

## 8. The `<init>`-return check refuses more than local-0 writes, and allocates per return (Low)

`vm/src/runtime/interpreter/exception_dispatch.rs:420` `ctor_return_names_its_initialized_receiver` reuses
`ctor_prefix_scan`. That scan also refuses (returns `None`) when an exception handler starts before the first
`invokespecial`, or when a branch at or after it targets below it. Those conditions belong to the unwinder's "throw
precedes initialization" proof, not to this question. JEP 513 (flexible constructor bodies, final in JDK 25) lets
verified javac code put `try`/`catch` and loops before `super(..)`. Such a finalizable constructor is refused, which
gives the pre-switch answer: a retried object is not registered again. Legacy `jsr`/`ret` constructors are refused too.
The impact is small, because the retry shape needs unverified code (`gc/src/reference.rs` `withdrawn_finalizables`
doc). The per-return cost is a full decode plus a `boundaries` `Vec` (one heap allocation) for every finalizable
constructor frame in the chain, on every allocation of such a class.
**Fix sketch:** a dedicated linear scan with no allocation, answering only two questions: "does anything write local 0
or use `jsr`/`ret`" and "is there an `invokespecial` before `return_pc`". Memoize the first answer per method. The
`Vec` of handler pcs (`:423`) can go too.

## 9. Inner-tables generation guard (Info: clean)

`InnerTablesFill` (`lang_class.rs:38526-38572`), `forget_redefined_inner_class_answers` (`:38648`). The order is
right. The forget bumps first (SeqCst), then clears. Each fill takes its guard after its cache miss and before reading
a table. It skips the insert if the generation moved, and removes its own row if the generation moved after the
insert. Every interleaving either drops the stale row or lets the clear remove it. The worst case is that a correct
concurrent row is removed and recomputed. The generation is per VM, not per class, so any redefinition drops every
concurrent fill (harmless). The modifiers fill (`:25545`) is guarded the same way, and the forget removes only the
redefined class's row, which is the only row that depends on that class's own table. `simple_class_name` (`:291`)
inserts unguarded but has no callers left.

## 10. Hidden-self matching, lambda proxies, hidden twins, per-VM latch, JIT doors (Info: clean, with notes)

- **Exactness.** `hidden_stored_name_is_class_file_name` takes only `<name>/0x<hex>` with a non-empty, all-hex tail.
  A nest host `p/Host` of `p/Host$$Lambda/0x..` leaves `$$Lambda/0x..` and is refused. A superclass sharing a prefix
  (`p/A` of `p/A$B/0x..`, `p/AB/0x1` against `p/A`) is refused. `p/A` against `p/A/0x1/0x2` (a hidden class whose
  class-file name is itself `p/A/0x1`) is refused. The referencing class's id selects, so two hidden classes from one
  set of bytes, and the JDK-spun `Host$$Lambda/0x..` proxies of one host, each answer themselves.
- **Arrays.** No door applies the rule to `[L<self>;` (the matcher refuses the bracket, and `jit_site_owner` /
  `resolved_ref_from_caller` return early for `[`). `anewarray <ITSELF>` resolves the element through the
  wave-11/14 `CONSTANT_Class` door. By reading, HotSpot resolves an array-class constant (`ldc`/`checkcast`
  `[LP;`) by NAME through the loader, so a hidden class's own array type is not itself. CratonVM's by-name answer agrees.
  This has not been measured; it was left out of `R21Fz7HiddenSelfLambda1` so the probe stays HotSpot-clean.
- **Granularity.** HotSpot rewrites only the `this_class` Utf8, so only `CONSTANT_Class` entries that share it name the
  hidden class. The by-string rule also catches a second `CONSTANT_Class` with its own Utf8 of the same text, which
  javac never writes. That is a hand-built-bytes nuance and not worth a fix.
- **Per-VM.** The new latch replaces `HIDDEN_CLASS_COUNTER` in `hidden_self_reference` (`constants.rs:5817`) with this
  VM's bitmap, which also removes one process-global read from every constant-pool class resolution.
- **JIT doors.** All twelve `helpers.rs` conversions sit behind `declaring_class_id != 0` or `[` guards. The compiled
  callee doors keep the wave-14 guards (`jit_static_owner_override`, `jit_callee_name_is_another_loaders_copy`), so a
  hidden self site never takes a by-name body. See item 5 for the hint filter.
- **Finalizer gate.** `DG_FINALIZER_INIT_RETURN` is armed only when both `VmConfig` fields are set
  (`interpreter.rs:10310`), and `SharedVm::new` hands the same field to the reference processor (`vm_init.rs:7610`).
  They cannot disagree within a VM.
- **Refusal epoch** (`CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH`, opt-in): a row of another epoch is reset under the write
  lock before the push. A stale row fails closed either way.
