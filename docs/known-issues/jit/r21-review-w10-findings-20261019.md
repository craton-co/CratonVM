# JIT round 21 wave 11 review: commit 9c970645f8 (waves 9-10) (lane rev-w10)

Status: OPEN — 2 items remain (low-severity residuals): 3R (`Lookup.in` with the outer class not yet loaded answers modes where HotSpot loads and throws ICCE; FIXED in wave 16, pending orchestrator build), 4R (compiled creator that retries `<init>` in a handler, and unverified re-`<init>` of a registered object, lose the registration; FIXED in wave 16, pending orchestrator build)

## Item status (triage 2026-10-19, HEAD fd8cb74135)

| Item | Verdict | Evidence |
|---|---|---|
| 1 hidden class `this(..)` not an initialization | FIXED | `vm/src/runtime/interpreter/exception_dispatch.rs:318` `ctor_pool_entry_may_initialize`, `:355`; test `:5969` |
| 2 `withdraw_finalizable` O(rows) rebuild | FIXED | `gc/src/reference.rs:3893` removes in place; test `:10645` |
| 3 `Lookup.in` hidden member of its outer class | FIXED (ICCE thrown when PRIVATE survives) | `native-builtins/src/classloader.rs:17231` `lk_class_relation` returns `Result`, `:17351`; tests `:23847`, `:23867` |
| 3R residual: outer class not loaded | FIXED (both registrations), pending orchestrator build | `native-builtins/src/lang_invoke.rs:19519` (real-JDK `in`, roots `this` / `lookup_class` / `target_class`); `native-builtins/src/classloader.rs:17262` `lk_class_relation_rooted`, `:17301` `lk_in_loads_outer_enabled`, `:17485` load arm of `lk_outermost_enclosing`, `:17627` `lk_in_method`; tests `:24065`, `:24092`, `:24113`, `:24141`; switch `CRATONVM_LOOKUP_IN_LOADS_OUTER` |
| 4 non-javac handler retries `<init>` (interpreted creator) | FIXED | `exception_dispatch.rs:376` `frame_rows_cover`, `frame_locals_hold`, `:479` `unconstructed_mark_at_entry`; test `:6029` |
| 4R residual: compiled creator retry; unverified re-`<init>` | FIXED (wave 16, lane finalize-reinit; pending orchestrator build) | `exception_dispatch.rs:361` `finalizer_init_returned`, called from `interpreter.rs:12523` (the `0xb1` arm, gate bit `DG_FINALIZER_INIT_RETURN` `:22566`, set at `:10311`), `:291` `ctor_frame_pc_precedes_initialization`; `gc/src/reference.rs:4014` `note_finalizable_init_returned`, `:3928` `withdraw_finalizable` (refuses a `constructed` row, keeps the withdrawn row), `:886` `withdrawn_finalizables`; switch `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`; tests `exception_dispatch.rs:6215`, `gc/src/reference.rs:10892`; probe `tools/probes/jit/r21/R21FinalizeRetryInit.java` |
| Fixed in place: uncached JNI switch reads | FIXED | `vm/src/config.rs:2914`, `:2921`; `vm/src/native/jni.rs:6733`, `:6747` |
| Remaining `class_manager.read()` per inherited JNI call | INFORMATIONAL (perf, measure first) | — |

Area: commit 9c970645f8: finalizer withdrawal (`exception_dispatch.rs`, `gc/src/reference.rs`, the doors),
JNI selection (`vm/src/native/jni.rs`), `jit/src/monitor_balance.rs`, `jit/src/not_entrant.rs`,
`native-builtins/src/classloader.rs` (`Lookup.in`), `native-builtins/src/lang_invoke.rs` (MHN texts), switch wiring
Found by: JIT round 21 wave 11, review lane rev-w10. This was a read-only review of `git show 9c970645f8`.
Nothing was built or run. `exception_dispatch.rs`, `gc/src/reference.rs` and `vm/src/jit/helpers.rs` were not
edited (the finalize lane owns them). Findings on those files are filed here.

## What was checked and found sound

- **The `unwind_to_handler` hot path.** A frame that is not `<init>` costs one string compare. An `<init>`
  frame reads `class_has_finalizer_shared` (the recipe, else one class-manager read) before anything else. The
  switch, the class-manager walk and the proof run only for a finalizable receiver. The proof's opcode table was
  checked byte by byte against JVMS 6.5: lengths, `wide`, tableswitch/lookupswitch padding, and the refusal of
  stores to local 0, `jsr`/`ret` and reserved opcodes.
- **Mark carry.** The mark is computed only after the frame's handler search fails. The next frame is stepped
  before its own search. A chain frame (an `<init>` on the same object, at `invokespecial`) keeps the mark. Any
  other frame at `invokespecial` is the creator and withdraws. Any other shape drops the mark. A
  resolution-error restart re-steps at the handler pc, which drops the mark (conservative). With javac, ecj or
  kotlinc output, an object whose `Object.<init>` returned is never marked. The frame's throw pc has to come
  before every `invokespecial` that could initialize its receiver, with no back edge or handler crossing it. A
  JNI `<init>` call on an already registered object is not withdrawable (`jni_constructor_call_withdrawable`).
  A note is removed when a door takes it. The exceptions are items 1 and 4.
- **Address+epoch keying (moving GC).** Every withdrawal compares `HeapRealm::address_epoch()` with the epoch
  read while the address was current. The doors re-read the object from its pin before reading the epoch. The
  withdraw door checks the epoch before it dereferences the object. Any collection, G1 cleanup or Generational
  sweep slice between the mark and the withdrawal refuses, which leaves the registration in place. A stale note
  cannot match another object, because an address names one object per epoch. `withdraw_finalizable` keeps
  enqueued and cleared rows and marks `finalizer_active` stale. `app_row_index` does not index finalizer rows.
  ZGC was not checked, because it shares its epoch with every address memo.
- **JNI selection.** Method ids encode the declaring class (`get_method_id` uses `find_method_recursive`'s
  declaring id). So `jni_mid_is_statically_bound` reads the right method. `toString`/`hashCode` (Object, not
  final) take the selecting door, `ResolvedRef::Declared(Object)`. Interface defaults take
  `Interface(Some(iface))`. Lambda proxies keep the plain door. Only a strict `Selected` replaces the by-name
  walk (`refine_lenient_selection_for`), so compatibility natives keep their answer. The JNI dispatch memo still
  serves selected calls (`jni_dispatch_memo_hit(.., receiver_selection)`).
- **`monitor_balance` sparse `Tracked`.** The set is the monitor operand slots, closed under the sources of
  reference copies into tracked slots. The walk reads an identity only at those sites, so restricting the
  partition to tracked cells gives the same verdicts as the old dense prefix below slot 1024. The javac
  (`aload; dup; astore t; monitorenter` / `aload t; monitorexit`), ecj and kotlinc
  (`aload a; astore t; aload t; monitorenter`) shapes all seed and close correctly. `meet_into` is canonical,
  and `merge_at` refuses in the same order as before. The budget bound holds with cells = tracked + depth.
- **`not_entrant.rs` generation bump.** The bump is made under `forwards`, before the box is pushed and the word
  is stored. The recording walk reads the generation before it walks. A `try_lock` miss in `ever_forwarded`
  answers `true` (fail closed). The bump fires only once per record, because `release_forwards_to` keeps boxes.
- **`classloader.rs` `lk_class_relation_by_id` / `lk_outermost_enclosing`.** All reads are lookups:
  `class_id_defined_by_loader_exact` and `class_id_by_name_near` (`find_class_by_name_for_class`) never load. The
  walk does not allocate a mirror or reach a GC point. The callers' mirrors are reused unchanged.
- **`lang_invoke.rs` MHN texts.** The format is HotSpot 25's `check_method_accessability`:
  `abstract`/`protected`/`private` in that order, nothing for package access, and
  `Method::print_external_name` (`void p.D.<init>(int)`,
  `java.lang.Object[] java.util.ArrayList.grow(int)`). The clause is joint for the same module and
  `; `-joined otherwise. A user loader prints `nameAndId` (`'r21' @<hex>`), the built-in loaders print
  `'app'`/`'platform'`/`'bootstrap'`. This matches all four orchestrator lines, provided
  `messages_name_user_loaders()` is on. Under `--compatible` without the `ModuleMessages` row, the clause is left
  out for a user loader, the same policy as the bytecode texts (pre-existing; not this commit). The 6th argument
  `strict` is the exact descriptor.
- **Switch wiring.** All five new switches have an INVENTORY row (`off_word: Some("0")`), a sorted
  `flag-surface.txt` line, and token and inventory rows. Their readers are default-on. The `TRACE_NOT_ENTRANT_INNERMOST`
  crate column now lists `jit, vm`.

## Open findings

### 1. A hidden class's `this(..)` is not recognized as an initialization, so its registration is wrongly withdrawn

- Site: `vm/src/runtime/interpreter/exception_dispatch.rs:304`, the `may_initialize` closure in
  `unconstructed_mark_of_ctor`: `Some(target) => &*class.name == target || super_name == Some(target)`.
- A hidden class is stored as `p/H/0x<hex>` (`lookup_define.rs:1215`, `class_manager.rs` `stored_name`). Its
  constant pool is not rewritten, so the `this(..)` `Methodref` names `p/H`. The name compare fails, and the
  delegating call counts as non-initializing.
- Failure: take `H(int x) { this(); throw new RuntimeException(); }` in a class defined by
  `Lookup.defineHiddenClass` that declares `finalize()`. `H()` returns, so its `Object.<init>` returned and
  HotSpot registers and finalizes the object. Here `first` stays at `code.len()` (no other initializing
  candidate), the `athrow` "precedes" it, and the proof holds. The creator frame then withdraws the row, and
  `finalize()` never runs. This is the one direction the design must never take.
- Fix (finalize lane): compare the `Methodref`'s `class_index` with the class file's `this_class` pool index.
  Alternatively, accept `target` when `class.name` is `target` plus a `/0x` tail and the class is hidden. Or, more
  simply, treat every `<init>` whose target is not provably a third class as initializing. Add a unit case:
  `aload_0; invokespecial #this; new; dup; invokespecial #5; athrow` must not be proven when `#this` names the
  hidden class's pre-mangle name.

**Verdict (lane finalize, wave 11): FIXED, pending orchestrator verification.** The pool entry test moved to
`exception_dispatch::ctor_pool_entry_may_initialize`, and its name half is `ctor_target_may_initialize`. A
`<init>` target counts as non-initializing only when it is provably a third class. The target must differ from the
class's stored name, and also, for a hidden class, from its pre-mangle name (everything before the last `/0x`). The
same two tests apply to the direct superclass (a hidden superclass spelled by its pre-mangle name). Any pool entry
the pool does not answer plainly still counts as initializing. The same predicate now also feeds the new
compile refusal (`finalizer_bars_compiling`), so the two cannot disagree. No new switch: the change is under
`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`. Test:
`r21_finalize_ctor_proof_tests::a_hidden_class_this_call_is_an_initialization`. It holds the review's
`aload_0; invokespecial #this; new; dup; invokespecial #5; athrow` body unproven at the `athrow` when `#this`
names `p/H` and the class is `p/H/0x1f2e`. It also pins that the wave-10 plain-name compare proved that body. No
probe arm was added. A `defineHiddenClass` arm needs class bytes built at run time, which the single-file probe
format does not carry.

### 2. `withdraw_finalizable` forces an O(rows) `finalizer_active` rebuild for each withdrawal

- Site: `gc/src/reference.rs:3873` (`withdraw_finalizable`: `self.finalizer_active_stale = true`).
- Each withdrawal marks the set stale. The next `is_registered_finalizable` or Finalizer discovery rebuilds it
  from every finalizer row. That next caller can be the next `new` of a finalizable class, the next
  `jni_constructor_call_withdrawable`, or the next withdrawal's own pre-check. A loop that keeps constructing a
  finalizable class whose constructor throws before `super()` therefore pays O(live finalizer rows) per
  iteration: quadratic, inside the processor mutex. The set's documented laziness assumes "at most once per
  collection", and this path breaks that assumption.
- Fix (owner of reference.rs): when the set is not stale, `self.finalizer_active.remove(&obj)` is exact. There is
  at most one active row per address, and the `retain` removes every active row at `obj`. So the stale mark is
  only needed when the set was already stale. The `Vec::retain` itself stays O(rows). That is acceptable,
  because a throwing finalizable constructor is rare. The rebuild of the whole set is the part to drop.

**Verdict (lane finalize, wave 11): FIXED, pending orchestrator verification.** `withdraw_finalizable` now drops
`obj` from `finalizer_active` in place and no longer marks the set stale. This is exact: the
`is_registered_finalizable` pre-check has just made the set current, and the `retain` removed every active row at
`obj`. The `retain` stays O(rows). Test: `gc` `r21_finalize_withdraw_tests::a_withdrawal_keeps_the_active_set_current`.
It checks that no rebuild is owed after a withdrawal, the withdrawn address is gone and the other address is kept.

### 3. `Lookup.in` treats a hidden class with a rewritten `InnerClasses` self-entry as a member of its outer class

- Site: `native-builtins/src/classloader.rs:17303` (`lk_outermost_enclosing`, the `inner_classes` arm).
- For a hidden class defined from member-class bytes, `class_manager.rs` (~11565, i102-L3) rewrites the self
  entry to the mangled name, so `inner == name` matches and the walk continues to `outer`. On HotSpot,
  `getEnclosingClass()` reaches `getDeclaringClass0()`, and `Reflection::check_for_inner_class` refuses it,
  because the outer class cannot list the mangled name. The i102-L3 comment says this VM refuses it too, in
  `getDeclaringClass0`. As a result, `VerifyAccess.getOutermostEnclosingClass` never reaches the outer class on
  HotSpot (it throws `IncompatibleClassChangeError` out of `Lookup.in`, or stops there). Here `same_nest` is
  `true`, so PRIVATE is kept. More generally, the walk accepts an `InnerClasses` link that the outer class does not
  confirm.
- Severity: low (non-javac hidden classes from member bytes). Before fixing, verify the exact HotSpot 25 answer
  with a probe: `defineHiddenClass` of `Outer$Inner` bytes, then `lookup().in(Outer.class).lookupModes()`, and
  whether it throws. Then apply the same agreement test that `getDeclaringClass0` uses here.

**Verdict (lane reflect, wave 11): FIXED for the mode answer, pending orchestrator verification; whether `in()`
must throw is still to be measured.** `lk_outermost_enclosing` no longer takes the `InnerClasses` link of a hidden
class: a hidden class with no usable `EnclosingMethod` is its own outermost class, so `same_nest` is false against
its outer class and `in()` drops PRIVATE|PROTECTED (25 from a full-power lookup) both ways. Its `EnclosingMethod`
class is still followed. Followed sources: JDK 25.0.3 `Class.getEnclosingClass` (`Class.java:1624-1651`) takes the
`getEnclosingMethod0` class when present, with no agreement check, and otherwise `getDeclaringClass0()`. That native
(HotSpot `JVM_GetDeclaringClass` -> `InstanceKlass::compute_enclosing_class`) ends in
`Reflection::check_for_inner_class`, which cannot match the mangled name in the outer class's `InnerClasses`, the
same refusal this VM makes in `lang_class::inner_classes_disagreement`. I did not have the HotSpot C++ source here;
this is from memory of it, together with that VM comment. No new switch: the change sits under wave 10's
`CRATONVM_LOOKUP_IN_PACKAGE_MEMBER` (default ON, also unbuilt). Only hidden classes are affected. The general
"outer class confirms the link" test for ordinary classes is not applied (the page's wider remark), because it
needs the outer class's `InnerClasses` and its loader's resolution in a walk that must not load.
**Residual:** if the outer class refuses the link, HotSpot's `check_for_inner_class` throws
`IncompatibleClassChangeError` inside `getOutermostEnclosingClass`, so HotSpot's `in()` probably *throws* where this VM
now answers 25. Making `in()` throw would need `lk_class_relation` to return an error, plus a change at both
callers (`classloader::lk_in_method`, `lang_invoke` `register_p63_method_handles_lookup`). Decide after the probe.
Test: `classloader.rs` `r21_lookup_in_relation_tests::hidden_member_bytes_do_not_reach_the_outer_class`. Probe:
the wave 11 rows of `tools/probes/jit/r21/R21HiddenLookupIn.java` (`HM.declaring`, `HM.enclosing`, `HM.in(O)`,
`O.in(HM)`, `HM.in(MEM)`, plus the controls `MEM.declaring` and `O.in(MEM)`). Each row prints its answer or the
exception class and message.

**Verdict (lane reflect, wave 12): the throw is implemented, pending orchestrator verification.** HotSpot 25
(orchestrator run): `HM.declaring`, `HM.enclosing`, `HM.in(O)`, `O.in(HM)` and `HM.in(MEM)` all throw
`IncompatibleClassChangeError: R21HiddenLookupInOuter and R21HiddenLookupInOuter$Mem/0x.. disagree on InnerClasses
attribute`; `MEM.declaring` is the outer class, `O.in(MEM)` 31. Change, still under `CRATONVM_LOOKUP_IN_PACKAGE_MEMBER`:
- `classloader::lk_class_relation(ctx, a, b, private_kept)` now takes `&mut dyn NativeContext` and returns
  `Result<(same_class, same_package, same_nest), MethodCallFailed>`.
- `lk_class_relation_by_id` walks only a same-package pair, lookup class first (JDK `isSamePackageMember`
  order).
- When `private_kept`, `lk_outermost_enclosing` turns the refused link of a hidden class into
  `LinkageError::IncompatibleClassChangeError` with `lang_class::inner_classes_disagreement`'s text. That is the
  message this VM's `getDeclaringClass0` raises, so `getDeclaringClass()` and `in()` agree. The hidden arm only
  reads names, and is asked only when both are known, so no load happens. The error is not allocated, and both
  callers propagate it before they allocate.
- Without `private_kept` the hidden class is its own outermost class, as in wave 11.
- Under `CRATONVM_CLASS_INNER_CLASSES_CHECK=0`, the switch that also stops `getDeclaringClass0` refusing, the link
  is followed.
- Callers pass the JDK's condition for reaching `isSamePackageMember`:
  - `lk_in_method`: `modes & PRIVATE != 0 && modes & UNCONDITIONAL == 0`.
  - `lang_invoke` `register_p63_method_handles_lookup`: not TRUSTED, PRIVATE set, not UNCONDITIONAL, and
    `lk_in_teleport` not `Across`. The ICCE comes before `newLookup`'s `checkUnprivilegedlookupClass`, as in the
    JDK.

Checked by reading the code (not run), this VM's `HM.declaring` and `HM.enclosing` already throw the same ICCE:
- `getDeclaringClass0`: `native_class_get_declaring_class` → `inner_classes_disagreement` (hidden arm).
- `getEnclosingClass` under `--jdk-only`: the JDK body → `getDeclaringClass0`.
- The `--compatible` bridge `lang_reflect::native_class_get_enclosing_class` → `inner_classes_disagreement_refusal`
  (per-VM `CRATONVM_INNER_CLASSES_AGREEMENT`, default ON).

**Residual:** if the outer class is not loaded yet, HotSpot loads it and throws. This walk must not load, so it
answers the PRIVATE-dropped modes instead.

Test: `r21_lookup_in_relation_tests::hidden_member_bytes_raise_the_declaring_class_icce_when_private_survives`. It
checks both directions with the exact text, no error without `private_kept`, and no error for another package.

**Verdict (lane lookup-in, wave 16): residual 3R FIXED in both `in` registrations, pending orchestrator build.**
`CRATONVM_LOOKUP_IN_LOADS_OUTER` (correctness, both modes, default ON, `=0` keeps the by-name answer; read once per
VM in `LoaderIdentityTables`).
- No new trait method was needed. The load uses `lang_class::class_seen_by_referencing_class_checked`: the
  without-loading view first, then `NativeContext::class_id_by_name_via_referencing_class`. That method drives the
  inner class's defining loader, loads, and does not initialize. It is HotSpot's `klass_at` in
  `compute_enclosing_class` / `JVM_GetEnclosingMethodInfo`, and the same door `getDeclaringClass0` uses here.
- `lk_outermost_enclosing(ctx, id, refuse, load)` (`classloader.rs:17485`). When a link's class is not found
  without loading, and `load && refuse` hold, the walk resolves it.
  - On success, the loaded outer class meets the existing checks. A hidden class from member bytes gets the
    `inner_classes_disagreement` ICCE. A spoofed ordinary member gets the `spoofed_member_disagreement` ICCE. In
    every other case the walk continues through the loaded class. This also fixes `p/A$B$C` against `p/A$D` when
    `p/A$B` is not loaded: the old walk stopped at the name `p/A$B`.
  - On failure, the walk returns the resolution error (`NoClassDefFoundError` for a missing outer class). It does
    so under the same rule `getDeclaringClass0` throws it by (`outer_resolution_errors_apply`), so
    `getDeclaringClass()` and `in()` give the same text. Otherwise it keeps the by-name answer.
  - The error order matches JDK 25: the lookup class's chain first, and only a same-package pair while PRIVATE
    survives.
- A load is a GC point, and both callers hold unrooted mirrors. So the load happens only through the new
  `lk_class_relation_rooted(ctx, a, b, private_kept, roots: &mut [Value])` (`:17262`). It pins every reference in
  `roots` across the walk and writes back the current addresses. `lk_class_relation` stays non-loading.
  `classloader::lk_in_method` (`:17627`) now calls the rooted form with `roots = [target]`.
- `register_p63_method_handles_lookup`'s `in` (`lang_invoke.rs:19519`) is the registration that wins with a real
  JDK. It now calls `lk_class_relation_rooted` with `roots = [this, lookup_class, target_class]`, because it reads all
  three after the call, and it re-reads them from the roots. No unit test covers this call site. No harness invokes
  that closure, which needs a real-layout `Lookup` object. The mock's pins are also no-ops, so a move cannot be
  observed. The relation itself is covered by the `classloader.rs` tests below. The real-JDK path is verified by
  the probe rows.
- HotSpot behaviour not measured this wave (nothing was run). The expected answers are as follows:
  - A spoofed pair whose outer class is not loaded throws the same ICCE as the loaded case (wave 14's
    `R21LookupInSpoofedMember`).
  - A missing outer class throws `NoClassDefFoundError` with the internal name as its message, from `klass_at`.
  - Probe rows to add: the spoofed and the missing-outer variants, with the outer class never touched before `in()`.

Tests (`r21_lookup_in_relation_tests`, all on mock classes):
- `hidden_member_bytes_load_an_unloaded_outer_class_and_raise_the_icce`: no load through the plain entry; the
  rooted entry loads the outer class and raises the exact ICCE text; the roots come back.
- `spoofed_member_of_an_unloaded_outer_class_raises_the_icce`: both directions.
- `missing_outer_class_is_the_resolution_error`: `NoClassDefFoundError`; no load without PRIVATE.
- `loads_outer_switch_off_keeps_the_by_name_answer`.

### 4. A non-javac handler that retries `<init>` on the unconstructed object loses its registration

- Site: `exception_dispatch.rs:328` (`unconstructed_mark_step`: the creator withdraws before its own handler
  search).
- This is legal but never compiler-generated bytecode: a creator keeps the `uninitialized(off)` reference in a
  local, catches the constructor's exception, and runs `aload; invokespecial <init>` again on the same object. If
  the retry completes, HotSpot registers the object at its `Object.<init>` return. Here the registration made at
  `new` was withdrawn when the first attempt unwound, and nothing re-registers it.
- The same holds for an unverified (boot or `skip_verification`) class running `invokespecial <init>` on an
  already initialized object.
- Severity: very low. Possible fixes: re-register at a later successful `<init>` return of a finalizable
  receiver, or refuse the withdrawal when the creator's exception table covers the `invokespecial` and the
  handler's StackMapTable frame keeps an `uninitialized` local. Recorded so the residual is known.

**Verdict (lane finalize, wave 11): fail closed for interpreted creators; a precise residual remains for the
rest.** The rule is now: an interpreted creator withdraws only when no row of its exception table covers its
`invokespecial`, whatever the row's catch type (`unconstructed_mark_step`, `frame_rows_cover`). Otherwise the
registration is kept, which was the pre-switch answer. Wave 11 also withdraws at the walk's ENTRY frame when the
creator lies outside the walk (`unconstructed_mark_at_entry`, see the compiled residual page). There the same
test is made on the interpreted frame directly below the entry. If that frame stands at an `invokespecial` that a
row covers, the walk only notes the object, the wave-10 answer, which a native door that allocated it takes.
Chain frames (an `<init>` on the same object) were already safe: a covering handler that catches drops the mark.
Test: `r21_finalize_ctor_proof_tests::the_entry_frame_withdraws_unless_the_frame_below_may_retry`.

**Residual (not fixed, very low):**
1. A COMPILED creator that catches the constructor's throwable and retries `<init>` on the same object. The
   entry-frame withdrawal cannot see the compiled frame's table. No compiler emits this shape. The fix would be to
   re-register at a later `<init>` return of a finalizable receiver.
2. A class that skipped verification (boot, `skip_verification`) running `invokespecial <init>` on an object that
   is already constructed and registered, with that constructor throwing before its `super(..)`. The proof then
   withdraws a valid registration. A verified class cannot reach that shape (JVMS 4.10.1.9: `<init>` takes an
   `uninitialized` receiver).

**Wave 16 (lane finalize-reinit): both residuals fixed, behind `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`
(default ON, `0` restores them; latched per VM in the reference processor).** HotSpot registers at every
`Object.<init>` return. Now:

- `withdraw_finalizable` keeps the row it takes in `ReferenceProcessor::withdrawn_finalizables` (capped at 32,
  watched, relocated by `update_after_gc`, pruned with its object, never a root).
- `exception_dispatch::finalizer_init_returned` runs when an interpreted `<init>` returns normally with a
  finalizable receiver (local 0). If the prefix scan proves the `return` precedes every initializing call, it does
  nothing: that is unverified code that skipped `super(..)`, and HotSpot registers nothing there. Otherwise it calls
  `note_finalizable_init_returned`, which does two things:
  - Re-registers a withdrawn object through `discover_reference`. That is one row, because a discovery while an
    active row exists is a no-op.
  - Marks the active row `constructed`.
- `withdraw_finalizable` refuses a `constructed` row. This is case 2: a later `<init>` on the object that throws
  before `super(..)`.
- An object whose `<init>` never returns is untouched.

The retried constructor is one the unwinder can prove, so it stays interpreted (`finalizer_bars_compiling`). Its
return is therefore an interpreted one even under a compiled creator.

**Wiring (done in the same wave):** the hook is called first in the `0xb1` (`return`) arm of `vm/src/runtime/interpreter.rs`
(`:12523`), before the held-monitor exit. A failed monitor exit cannot undo an `Object.<init>` return.

The cost is staged:
- **Every return:** one test of the per-entry gate bit `DG_FINALIZER_INIT_RETURN` (`:22566`). The bit is set at
  `:10311` from `VmConfig::finalizer_register_at_object_init`, latched per VM; without that switch nothing is ever
  withdrawn.
- **A non-constructor that passes the bit:** the name's length and first byte (`finalizer_init_returned`), no
  string compare. JVMS 4.2.2 allows `<` only in `<init>` and `<clinit>`.
- **A constructor:** the receiver's has-finalizer bit, from the lock-free allocation recipe.
- **A finalizable receiver only:** takes the reference processor's lock. `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`
  itself is latched there, per VM.

Not done: a `VmConfig` field for this switch would let the gate bit drop the lock for `=0` too. `vm/src/config.rs` is
outside the lane. The gate-word test `i82_l7_gate_word_tests` lists the new bit.

**Remaining (very low):**
- `constructed` is set only at interpreted `<init>` returns. Case 2 on an object whose every earlier `<init>` ran
  compiled still withdraws, which is the pre-switch answer.
- A JNI `<init>` upcall on a compiled body is not seen either.
- HotSpot would add a second Finalizer for a re-`<init>` of an already finalized (enqueued) object. Here the
  registration stays idempotent per active row.

Probe: `tools/probes/jit/r21/R21FinalizeRetryInit.java` builds the case 1 creator by hand (javac cannot store an
`uninitialized` reference in a local).

**Wave 17 correction: case 1 is unreachable in verified code.** HotSpot 25 refuses the creator:

> VerifyError: Stack map does not match the one at exception handler 11 ... 'R21FinalizeRetryInit$Retry' (current
> frame, locals[1]) is not assignable to uninitialized 0

HotSpot checks a handler's frame against the frame after each covered instruction as well as before it. After the
`invokespecial`, local 1 is the initialized type. CratonVM's verifier makes the same check
(`classloading/src/bytecode_verifier.rs` `check_handler_frames_at`, called after the instruction, once
`verify_insn.rs` `replace_vtype_in_frame` has run), so it refuses the creator too. This was established by reading the
code, not by running it.

Both cases are therefore reachable only with verification off (HotSpot `-XX:-BytecodeVerificationRemote`, CratonVM
`skip_verification` or boot classes). The fix stays as a defence for those paths. The probe now prints
`fin noretry 0` and `maker java.lang.VerifyError` on HotSpot 25. Its retry rows run only in the unverified arm, whose
output is expected but not yet confirmed.

## Fixed in place

- **Uncached switch reads on every inherited JNI call.** `vm/src/native/jni.rs` `jni_call_instance` read
  `CRATONVM_JNI_CALL_VIRTUAL_SELECTS` through `runtime_flag_default_on` on every inherited
  `Call<T>Method*`. Examples are `toString` with an Object id, or `Iterator.hasNext` with an interface id on an
  implementation. Each read was a declared-name lookup plus `legacy_var_os`, an `OsString` clone. It also read
  `CRATONVM_JNI_CALL_STATICALLY_BOUND_EXACT_MID` on every statically bound one. The lane rule is "hot-path reads
  cached the way the neighbouring code does". Both are now `VmConfig` fields read once per VM:
  `jni_call_statically_bound_exact_mid` and `jni_call_virtual_selects` (`vm/src/config.rs`, the field docs and
  `Default`). `jni_call_instance` reads `shared.config.*`. The source-pin test
  `r21_w10_jni_call_instance_binds_and_selects` now pins the config reads and asserts that no environment read
  is left in the body. The `types/src/flag_groups.rs` row comments name the readers. Semantics are unchanged.
  `=0` still kills each switch, but now only per VM (set at VM construction), as for every `VmConfig` switch.
- What remains on that path: one `class_manager.read()` per inherited JNI call, for
  `jni_mid_is_statically_bound` and `is_interface`. A per-thread memo keyed on the method id would remove it if a
  JNI-heavy row shows it.

## How to verify

- Build and test with `cargo test -p cratonvm-vm --lib r21_w10_jni` (both JNI tests) and
  `cargo test -p cratonvm-types` (flag surface and inventory).
- Item 1: once fixed, add a unit case in `r21_finalize_ctor_proof_tests` and a hidden-class arm in
  `tools/probes/jit/r21/R21FinalizeCtorThrows.java`. Expect HotSpot to finalize the `this()`-then-throw object.
