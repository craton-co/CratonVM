# JIT round 21 wave 22: adversarial review of wave 21 (lane review-w22)

Status: items 1-5 settled in wave 22 (lane hidden-resid; pending orchestrator build); item 4 partly. There are 10 items. Item 1 is low-medium: a wave-21 fix went too far and is a `--compatible` regression
against HotSpot. Items 2-5 are low; item 4 is performance and item 5 is a test that cannot fail. Items 6-10 list what
was reviewed and found clean. Nothing was fixed, because this lane may not edit `.rs` files.

Scope: `git diff 57a8eae8cd 89f30b51ea -- '*.rs'` (wave 21), read at HEAD 89f30b51ea. The review was by reading only:
nothing was built or run. Line numbers are HEAD's; other lanes' uncommitted edits were ignored.

## Item status

| Item | Severity | Verdict |
|---|---|---|
| 1 the `--compatible` name bridges no longer raise the EnclosingMethod class's resolution error | Low-medium (`--compatible`; regression from wave 21's item-7 fix) | FIXED (wave 22, lane hidden-resid; pending orchestrator build): `native-builtins/src/lang_class.rs` `name_bridge_enclosing_method_error`, asked by both bridges where `name_bridge_asks_declaring_class` skips (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, no new switch); probe `R21Fz8ReviewEmGone1` run pending |
| 2 `hidden_class_file_names` is never pruned at unload | Low (unbounded growth with hidden-class churn) | FIXED (wave 22, lane hidden-resid; pending orchestrator build): the recorded names moved to `ClassStore::recorded_file_names` (`classloading/src/class.rs`), dropped by `ClassStore::remove`; the prefilter word is cleared in `unload_user_classes_inner` |
| 3 "one rule" still gives two answers: the recorded name against the stored suffix | Low (unreachable today; wrong direction for the finalizer unwinder) | FIXED (wave 22, lane hidden-resid; pending orchestrator build): one answer, `ClassStore::hidden_self_name`, read by the class manager's doors, `is_self_class_reference_in` (`dispatch_static`, `invoke.rs`) and the unwinder's `ctor_pool_entry_may_initialize` (`CRATONVM_HIDDEN_SELF_RECORDED_NAME`) |
| 4 hot-path costs: the bridges' pre-check before the name cache, and the recorded-name lookup | Low (perf; one plain class-manager read) | PARTLY FIXED (wave 22, lane hidden-resid; pending orchestrator build): `NativeClassAccess::enclosing_method_class_name` (one clone, `read_recursive`); `is_self_class_reference_in` takes the held `&Class`. The bridge pre-check was NOT moved after the cache: not behaviour-neutral, see the item |
| 5 the source pin `r21w21_descriptor_types_are_not_recorded` matches its own text | Low (test cannot fail) | FIXED (wave 22, lane hidden-resid; pending orchestrator build): the needle is built with `concat!` and searched in `native_class_get_record_components`'s body only, plus a negative check for the recorded door |
| 6 `<init>`-return scan (`ctor_body_keeps_receiver_and_calls_before`) | Info | clean, with a note |
| 7 deopt-resume conversions (item 1 of w21) | Info | clean |
| 8 past-capacity latch, lazy `invokespecial` probe, switch conventions | Info | clean, with notes |
| 9 sticky-memo prune at unload, unrecorded descriptor door, rethrow cause | Info | clean |
| 10 `defineAnonymousClass` mint, JVMTI conversions, globals and allow-lists | Info | clean |

Probes, written without being run. They follow `r21-fuzz5-probes-20261019.md`: default package, ASCII, deterministic
output, and the HotSpot expectation (by reading) in each header.

| Probe | Targets | Verdict |
|---|---|---|
| `tools/probes/jit/r21/fuzz/R21Fz8ReviewEmGone1.java` | item 1: local, anonymous and EnclosingMethod-only classes whose EnclosingMethod class is missing, in the application loader; an array of one; a control whose EnclosingMethod class loads | OPEN (run pending; expected to differ on `--compatible`) |
| `tools/probes/jit/r21/fuzz/R21Fz8ReviewAppRows1.java` | wave 21's item-7 fix on the arm it changes: the `R21Fz7InnerRowsEm1` shapes in the application loader, where `name_bridge_outer_resolution_error` runs (it returns early once any user-defined loader exists, so `R21Fz7InnerRowsEm1` never reaches it) | OPEN (run pending; expected to match in all modes) |

## 1. The `--compatible` name bridges no longer raise the EnclosingMethod class's resolution error (Low-medium)

- **Where:** `native-builtins/src/lang_class.rs:38965` `name_bridge_asks_declaring_class`, used at `:7315`
  (`native_class_get_simple_name`) and `:37285` (`native_class_get_canonical_name`). For any class (or array element)
  with `EnclosingMethod`, it answers `false`. Both bridges then skip `inner_classes_icce_for_mirror`,
  `name_bridge_outer_resolution_error` and `member_class_inner_classes_disagreement`.
- **What JDK 25 does:** `isLocalOrAnonymousClass()` is `hasEnclosingMethodInfo()`, which calls `getEnclosingMethod0()`.
  HotSpot's `JVM_GetEnclosingMethodInfo` resolves the EnclosingMethod class with `klass_at(.., CHECK_NULL)`.
  - The `&&` in `isTopLevelClass` short-circuits only AFTER that call. So a missing EnclosingMethod class is a
    `NoClassDefFoundError`, caused by `ClassNotFoundException`, out of `getSimpleName`.
  - The same error comes out of `getCanonicalName` (after `isHidden()`), `isAnonymousClass`, `isLocalClass`,
    `isMemberClass`, `getEnclosingClass` and `getEnclosingMethod`.
  - It also comes out of `getDeclaringClass` when the class has a self row with no outer class:
    `compute_enclosing_class` resolves the EnclosingMethod class there too.
- **What changed:** before wave 21, `name_bridge_outer_resolution_error` called
  `declaring_class_id_checked(.., propagate = true)`. Its local/anonymous arm (`:39278-39289`, a self row with no
  outer class) resolved the EnclosingMethod class through `class_seen_by_referencing_class_checked`, and threw what
  HotSpot throws. Wave 21 skips that check whenever `EnclosingMethod` is present, which is exactly the case where
  the arm applies. The bridges now answer `"L"` / `""` / `null` where HotSpot throws.
  - The wave-21 page's reasoning ("with `EnclosingMethod` present the `&&` short-circuits, so `getDeclaringClass0`
    is never called") is right about `getDeclaringClass0`. But it drops the EnclosingMethod resolution that
    `hasEnclosingMethodInfo` performs first.
  - A class with `EnclosingMethod` and no `InnerClasses` (the `$E` row of the probe) was already wrong before wave 21:
    the old arm needs a self row.
- **Who sees it:** `--compatible` only, with no user-defined loader registered (the arm returns early otherwise).
  That is a class path whose jar lost the enclosing class of a local or anonymous class: a shrunk, shaded or partially
  copied jar. `--jdk-only` runs the JDK body. That body calls the `getEnclosingMethod0` native (`:39710`), which
  resolves through the checked door and should match.
- **Fix sketch:** make `name_bridge_asks_declaring_class` return three answers:
  - **ask:** the class has no `EnclosingMethod`. Keep today's checks.
  - **skip:** a hidden class, in the canonical bridge only. HotSpot's `isHidden()` comes first there.
  - **resolve the EnclosingMethod class:** every other class with `EnclosingMethod`. When
    `outer_resolution_errors_apply(ctx, element)` holds, call
    `class_seen_by_referencing_class_checked(ctx, element, &em_class)` and return its `Err`. That is the recorded
    door, so a later ask rethrows the same error, as HotSpot's resolution-error table does. Then skip the
    declaring-class checks, as now.

  Keep it under `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, whose `=0` arm answers "ask", as today. Run both
  probes in both modes. `R21Fz8ReviewAppRows1` must stay matching, because its EnclosingMethod class loads.

**Verdict (wave 22, lane hidden-resid): FIXED (pending orchestrator build).**
- Both bridges, `native_class_get_simple_name` and `native_class_get_canonical_name`, now call
  `name_bridge_enclosing_method_error` in the `else` arm of `name_bridge_asks_declaring_class`, so it runs exactly
  where the declaring-class checks are skipped.
- It reads the EnclosingMethod class through `NativeClassAccess::enclosing_method_class_name` and resolves it with
  `class_seen_by_referencing_class_checked`, the recorded door. It returns the `Err`.
- Exits, in this order:
  - a cached name (as `name_bridge_outer_resolution_error`);
  - in the canonical bridge, a hidden class (`isHidden()` comes first);
  - a class with no `EnclosingMethod`;
  - `outer_resolution_errors_apply` (JDK classes keep the old answer).
- It is not gated on "no user-defined loader". HotSpot resolves the EnclosingMethod class whatever loaders exist.
- This also covers the `$E` shape (EnclosingMethod with no `InnerClasses`). It needs only `EnclosingMethod`.
- It stays under `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, with no new switch. `=0` answers "ask"
  everywhere, so it never reaches this function, as before wave 21.
- Test: `lang_class::tests::r21w22_name_bridges_resolve_the_enclosing_method_class`. The mock resolves every name,
  so the error path is pinned by source. The mock gained `set_enclosing_method`.
- Run `R21Fz8ReviewEmGone1` and `R21Fz8ReviewAppRows1` in both modes.

## 2. `hidden_class_file_names` is never pruned at unload (Low)

- **Where:** `classloading/src/class_manager.rs:6331` (field), `:12389` (insert at define),
  `:13232` `unload_user_classes_inner`. The unload drops `hidden_lookup_class_names` (`:13298`), the sibling side table
  of the same define block, but not this one.
- **Scenario:** a non-strong hidden class unloads with its mirror (since gc-common w18-d this can happen on every
  collection). Its `Arc<str>` row stays for the life of the VM. Workloads that keep defining hidden classes grow the
  map without bound: script engines, byte-code proxies, and `Lookup.defineHiddenClass` without `STRONG`. Ids are not
  reused (the `unload_user_classes_inner` comments say so), so a row cannot answer for a new class. This is a leak,
  not a wrong answer. `is_hidden_self_name` also checks `get_class`, which fails for a tombstone.
- The define-side comment "a reused id's stale row is replaced or dropped here" (`:12385`) contradicts that
  invariant. Drop it, or keep it only as a guard.
- **Fix sketch:** `self.hidden_class_file_names.remove(id);` beside `hidden_lookup_class_names.remove(id)` in
  `unload_user_classes_inner`. Better, see item 3: store the name on `Class`, so it dies with the class.

**Verdict (wave 22, lane hidden-resid): FIXED (pending orchestrator build).**
- The recorded class-file names are now a side map on `ClassStore`, `recorded_file_names` (`classloading/src/class.rs`).
- `ClassStore::remove`, which unload calls per dead id, drops the row, so it dies with the class.
- Lane hidden-resid's wave-22 rows (renamed ordinary defines) live in the same map, and the per-VM prefilter word
  is cleared in `unload_user_classes_inner`.
- The misleading "reused id" comment is gone.
- Why not a field on `Class`: there are 91 `Class { .. }` literals across crates. The store map has the same
  lifetime and needs none of those edits.
- Test: `class_manager::tests::the_recorded_class_file_name_is_one_store_answer_and_dies_with_the_class`.

## 3. "One rule" still gives two answers: the recorded name against the stored suffix (Low)

- **Where:** the doors (`ClassManager::is_hidden_self_name`, `class_manager.rs:16264`) read the recorded `this_class`.
  Two callers still read only the stored name's `/0x<hex>` suffix:
  - `vm/src/runtime/interpreter/exception_dispatch.rs:585` `ctor_target_may_initialize`, reached from
    `ctor_pool_entry_may_initialize` (`:548`), which has only a `ClassStore`;
  - `dispatch_static.rs:245`, through the pure `is_self_class_reference`.
- **Where they disagree:**
  - **The define-time collision probe** (`class_manager.rs:11767`) appends `/0x<n>` to an already-mangled override
    (`p/A/0x1f/0x2`). The doors say `p/A` is the class itself; the suffix rule refuses it (the tail is not all hex).
  - **A class marked hidden after its define** (`mark_class_hidden`) has no recorded name, so both read the suffix
    and agree.
- **Direction:** for the unwinder, `false` means "this `invokespecial <init>` cannot initialize the receiver". A
  collided hidden class's `this(..); throw ..` constructor is then "proven unconstructed". The object is withdrawn
  from finalization, but HotSpot finalizes it: the wave-11 bug, back for this one spelling. This is fail-open on the
  finalizer side.
- **Reachability:** the collision needs two hidden defines with the same override name in one loader. Every caller
  mints with the process counter (`HIDDEN_CLASS_COUNTER`), so this is unreachable today. The wave-21 page's claim of
  "one rule" is stronger than the code.
- **Fix sketch:** either change will do:
  - Put the recorded class-file name on `Class` (`hidden_file_name: Option<Arc<str>>`, set where the bitmap is
    noted) and have every rule read it. That covers the pure and store-only callers, and also retires item 2's side
    map.
  - Or make the collision probe mint `<this_class>/0x<n>` (from `class_file.this_class`, not from the override).

**Verdict (wave 22, lane hidden-resid): FIXED (pending orchestrator build).**
- One answer: `ClassStore::hidden_self_name(class, name)`, the rule with the store's recorded name. It is read by:
  - `ClassManager::is_hidden_self_name`;
  - `constants::is_self_class_reference_in` (`dispatch_static` and the three `invoke.rs` doors);
  - the unwinder's store-only `ctor_pool_entry_may_initialize`. Its hunk is small: it ORs the store answer in
    front of the pure `ctor_target_may_initialize`.
- All of these are under `CRATONVM_HIDDEN_SELF_RECORDED_NAME`, latched on the class store, per VM.
- In the unwinder, the OR can only widen "may initialize", which is the fail-closed direction.
- The collided `p/A/0x1f/0x2` spelling is now itself at every holder.
- Tests: the class-manager test above, `invoke::hidden_self_cp_tests::a_collided_hidden_define_is_itself_at_the_self_doors`
  and `the_unwinders_constructor_target_reads_the_store_answer`.

## 4. Hot-path costs: the bridges' pre-check before the name cache, and the recorded-name lookup (Low)

- **The bridges' pre-check:** `name_bridge_asks_declaring_class` runs on every `--compatible` `getSimpleName` /
  `getCanonicalName`, before `SIMPLE_CLASS_NAME_CACHE` / `CANONICAL_CLASS_NAME_CACHE` are consulted. It calls
  `ctx.enclosing_method(element)` (`vm/src/vm/vm_exec.rs:20573`), which costs:
  - a plain `class_manager.read()`, not `read_recursive`. A plain read queues behind a waiting writer, and a native
    may run while a frame up the stack holds a read (see the `jvmti_events.rs` note at `:3103`);
  - three `String` clones for a local or anonymous class.

  Only successes are cached. A refusal or error is never cached, so a cache hit can skip the pre-check. Fix: compute
  it after the cache probe, and answer "has `EnclosingMethod`" with a borrowed check under `read_recursive` (no
  clones).
- **The recorded-name lookup:** `is_hidden_self_name` adds a hash lookup on every constant-pool owner lookup from a
  hidden caller. That covers every JDK lambda proxy and `LambdaForm$MH` class, so it runs on every call that resolves
  an owner through them. Before wave 21 it was one `get_class` and a suffix compare. `is_self_class_reference_in`
  (`constants.rs:5797`, called at `invoke.rs:757`, `:3391` and `:7940`) fetches the class a second time, although the
  caller already holds it. Fix: put the name on `Class` (item 3), and take `&Class` in `is_self_class_reference_in`.

**Verdict (wave 22, lane hidden-resid): PARTLY FIXED (pending orchestrator build).**
- **Done, behaviour-neutral:**
  - new `NativeClassAccess::enclosing_method_class_name`. Its default derives from `enclosing_method`; the VM's
    override clones one `String` under `read_recursive`. `name_bridge_asks_declaring_class` and the new
    EnclosingMethod check use it.
  - `is_self_class_reference_in(cm, &Class, name)` takes the class the caller holds, with no second `get_class`,
    and costs one hash probe for a hidden class only.
- **Not done:** moving the pre-check after the name-cache probe. It is not behaviour-neutral, for two reasons:
  - `SIMPLE_CLASS_NAME_CACHE` is also filled without any check, by `resolve_simple_name` (used outside the
    bridge);
  - the lambda and CGLIB arms answer before the cache.

  So a cache-first probe could skip an ICCE the bridge throws today.

## 5. The source pin `r21w21_descriptor_types_are_not_recorded` matches its own text (Low)

- **Where:** `native-builtins/src/lang_class.rs:44985`. The first assertion is
  `src.contains("match class_seen_by_referencing_class_unrecorded(ctx, class_id, inner) {")` on
  `include_str!("lang_class.rs")`. The test's own string literal is in that file, verbatim. So the assertion holds
  even if `getRecordComponents` goes back to the recorded door.
- The two body-sliced assertions after it are fine: they find the real function by `"\nfn ..."`, which the escaped
  literal cannot match.
- **Fix sketch:** slice the body of `native_class_get_record_components` and search only there, or build the needle
  with `concat!`. The `invoke.rs` pin of the same wave does the latter (`jvmti_invoke_owner_readers_use_the_constant_pool_rule`).

**Verdict (wave 22, lane hidden-resid): FIXED (pending orchestrator build).** Both: the body of
`native_class_get_record_components` is sliced, and the needles are built with `concat!`. A negative assertion was
added too: that body must not call `class_seen_by_referencing_class_checked(`.

## 6. `<init>`-return scan (Info: clean, with a note)

`exception_dispatch.rs:447`. The opcode length table was checked against JVMS 6.5, and every opcode is right:
- 1-byte: `0x00-0x0f`, `0x1a-0x35`, `0x3b-0x83` except `0x84`, `0x85-0x98`, `0xac-0xb1`, `0xbe`, `0xbf`, `0xc2`, `0xc3`;
- 2-byte: `bipush`, `ldc`, the `xload`/`xstore` index forms, `newarray`;
- 3-byte: `sipush`, `ldc_w`, `ldc2_w`, branches, field and invoke ops, `new`, `anewarray`, `checkcast`, `instanceof`,
  `ifnull`, `ifnonnull`, `iinc`;
- 4-byte: `multianewarray`; 5-byte: `invokeinterface`, `invokedynamic`, `goto_w`;
- `wide` forms (4, and 6 for `iinc`);
- the `tableswitch` / `lookupswitch` padding (`pc + 1 + (3 - pc % 4)` is the next multiple of 4) and the bounds.

The `xstore_0` arm comes before the overlapping 1-byte range, so it wins. `frame.code` is not quickened in place
(the field fast paths use side tables), so a hot constructor is not refused because of its own rewritten opcodes.

Note: "an `invokespecial` at a lower pc" is a code-order test, not a path test. Unverified code is the only code
that reaches the retry shape. There, a forward `goto` over the only `invokespecial` reaches `return` without
`Object.<init>`, and the hook re-registers an object HotSpot never registered. The old `ctor_prefix_scan` accepted
the same shape (it refuses only back-branches and early handlers), so this is not a regression. The hook still takes
`ref_processor.lock()` on every finalizable constructor return that passes the scan (wave 19, unchanged).

## 7. Deopt-resume conversions (Info: clean)

`deopt_resume.rs:688` (hinted), `:812` `supertype_named` (start only), `:877` `resolve_unhinted_scope_class` (both
`SCOPE_BY_LOADER` arms), `:968` scope scan. Inner scopes carry `enclosing = cached.declaring_class_id` inward
(`:1119-1140`), so a hidden class's self answer never leaks into a scope that an ordinary class encloses. With the
switch on, step 1-2 and the constant-pool scan both bind the class-file name to the hidden id, and they agree. With
it off, both answer the ordinary class, and they agree too. The new test covers all three arms and the kill word.

A hidden class whose superclass is an ordinary class of its own class-file name (hand-built bytes) is refused as
"two different supertypes", which fails closed. HotSpot shares only the rewritten `this_class` Utf8, so its answer is
the ordinary superclass. That is a hand-built-bytes nuance, as w21 item 10 says.

## 8. Past-capacity latch, lazy `invokespecial` probe, switch conventions (Info: clean, with notes)

- **Latch** (`class_manager.rs:4423`): it is stored with `Release` in `note` before `class_store.add`, and read with
  `Acquire` in `contains`. Synthetic lambda-proxy ids are not in the store, so the lock-free `Some(false)` equals what
  the locked path answered (`get_class` is `None`). Once any hidden class lands past `1 << 20`, every high id goes back
  under the lock for the life of the VM. That is a perf cliff after about a million classes, which is acceptable.
- **Lazy probe** (`invoke.rs:3288`): the closure runs at the same place in the `else if` chain, with the same
  captures. The answers are identical.
- **Switch conventions:**
  - Items 6 and 7 of w21 ride on existing default-on switches, and their `=0` restores the old answers:
    - `CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY=0`: no memo, so the unrecorded descriptor door is the old door, and
      there is no rethrow;
    - `CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD=0`: "ask" everywhere.
  - Item 8 of w21 is inside the `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN` hook, and `=0` turns the whole hook
    off.
  - Three changes have no switch: the latch, the lazy probe and the recorded-name rule. They are perf changes or
    refactors that keep every answer the same for every mint site JDK 25 reaches. That is consistent with the rule.

## 9. Sticky-memo prune at unload, unrecorded descriptor door, rethrow cause (Info: clean)

- **The unload prune.** `forget_unloaded_reflect_door_rows` (`lang_class.rs:39370`) is per VM: it runs on the door
  slot of `shared.natives`. It takes two `Scratch` locks in turn, never nested.
  - The spoof keys decode correctly. In Rust, `&` binds tighter than `==`, so `*key & 0xFFFF_FFFF == SPOOF_BY_INNER`
    reads as intended.
  - The `gc.rs` pin scans only the body of `unload_dead_class_metadata`, so it cannot match its own text.
- **The rethrow** (`:39400`) matches HotSpot by reading:
  - `add_resolution_error` keeps the cause's class and `message_as_utf8`;
  - `throw_resolution_error` builds the cause with `Exceptions::new_exception` and throws `THROW_MSG_CAUSE`, which
    sets it through `initCause`;
  - CratonVM writes `cause` directly. The visible state is the same, because the field still holds `this` there.
  - The cause is pinned across the error's allocation, and no allocation happens between the unpin and the field
    store.

## 10. `defineAnonymousClass` mint, JVMTI conversions, globals and allow-lists (Info: clean)

- **`unsafe_natives.rs` `defineAnonymousClass`:** the mint now uses the parsed `this_class`. That method does not
  exist in JDK 17 and later, so the change is unreachable from a JDK 25 library. The extra parse is harmless. Bytes
  the non-preview reader rejects get the `anonymous` prefix, and the define-time recorded name covers them anyway.
- **The `jvmti_events.rs` stand-in reader (`:3147`):** `callback_runs_method` still matches by the symbolic owner's
  name. No native is ever registered under a hidden class's class-file name, so this does not misattribute.
- **Globals and allow-lists:** wave 21 adds neither a process global nor a class-name allow-list.
  - `HIDDEN_CLASS_COUNTER` and the `*_CLASS_NAME_CACHE` statics are pre-existing, and the caches are keyed by VM
    identity.
  - The JDK-prefix list in `outer_resolution_errors_apply` dates from round 18.
- **Test placement:** the new test modules sit after the source-pin cut points:
  - `invoke.rs` `hidden_self_cp_tests` and `deopt_resume.rs` `r21w16_scope_by_loader_tests` are at the ends of their
    files;
  - the new `#[cfg(test)]` on `constants.rs:5945` guards a `fn`, not a `mod`, so the
    `"\n#[cfg(test)]\nmod "` production cut is unchanged.
