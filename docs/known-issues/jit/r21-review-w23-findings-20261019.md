# JIT round 21 wave 23: adversarial review of wave 22's correctness changes (lane review-w23)

Status: OPEN (items 1-6 fixed in waves 23-24, pending orchestrator build). There are 11 items. Items 1-3 are low: a stale finalizer state after a stub upgrade, and two JNI
re-`<init>` cases where the new count disagrees with HotSpot. Item 4 is performance. Items 5 and 6 are about naming,
are info-level, and come from hand-built names only. Items 7-11 list what was reviewed and found clean. Nothing was
fixed, because this lane may not edit `.rs` files.

Scope: `git diff 89f30b51ea 98007436be -- '*.rs'` (wave 22), without the opt-in perf arms (another lane reviews those:
the init-return scan memo, the fresh-allocation volatile store, the two vh-const arms, the hidden self-name
prefilter). Read at HEAD 98007436be, by reading only: nothing was built or run. Line numbers are HEAD's. Other lanes'
uncommitted edits were ignored, except where item 1 names the one that overlaps.

## Item status

| Item | Severity | Verdict |
|---|---|---|
| 1 a stub upgrade that flips `has_finalizer` skips the VM's define-time finalizer listener | Low (`--compatible` stubs; over-finalization after a throw before `super(..)`) | FIXED (wave 23, lane finvm; pending orchestrator build): flips queued at the upgrade, settled at the write-guard release, including the class's OWN constructor rows and bodies; see the verdict under 1 |
| 2 the JNI re-`<init>` count is dropped by any collection during the constructor | Low (HotSpot finalizes twice, this VM once) | FIXED (wave 23, lane finalize; pending orchestrator build): `vm/src/native/jni.rs` `jni_reinit_registers_again` counts a normal return at the receiver's current address, read back from the native's handle by `jni_receiver_after_call` when the epoch moved. That covers a global ref or an indirect local (default ON). A raw local handle still counts nothing, the fail-safe. A throw keeps the epoch test. Switch `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME` (default ON, `VmConfig::jni_reinit_count_by_outcome`). Test `r21w22_jni_reinit_of_a_registered_object_counts_a_second_finalizer` (moved-epoch cases) |
| 3 a JNI `<init>` call that fails before its constructor runs is counted as a second `Finalizer` | Low (one extra `finalize()`) | FIXED (wave 23, lane finalize; pending orchestrator build), the cheaper option of the sketch, same switch: `jni.rs` `jni_reinit_failure_reached_ctor` refuses a count for `MethodCallFailed::InternalError` (refusals, link failures, `--jdk-only` refusals) and for a thrown `java/lang/StackOverflowError` (HotSpot's `call_helper` raises it before entry). Residual, documented at the site: a `StackOverflowError` raised deep inside the constructor after `Object.<init>` returned is also not counted (one `finalize()`, HotSpot two). That errs toward the pre-switch answer. Test: the same JNI test |
| 4 `take_extra_finalizer` scans every finalizer row on each `finalize()`, once any re-`<init>` happened | Low (perf; quadratic drain) | FIXED (wave 23, lane finalize; pending orchestrator build): `gc/src/reference.rs` `extra_finalizers_outstanding` replaces the sticky bool. It goes up in `note_finalizer_registered_again`, down in `take_extra_finalizer`, `withdraw_finalizable` and `remove_collected` (the only two places a finalizer row is removed), so a drain scans only while some count is owed. No switch: the answers are unchanged. Tests `the_outstanding_count_follows_the_rows`; the door test the review asked for is `gc_and_alloc.rs` `gci_i35_jf_forced_drain_tests::r21w23_a_counted_reinit_runs_finalize_twice_through_the_door` (through `run_finalizers_impl`, both switch arms) |
| 5 the context-free renderers still judge the `/0x` tail by the name's shape, arrays included | Info (hand-built names; already known for the renderers) | FIXED for the class-holding renderers (wave 24, lane names; pending orchestrator build), same switch `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`; the context-free renderers stay shape-judged; probe `R21Fz9ReviewHexName1`. Wave 25, lane hexname (pending orchestrator build): the compiled cast helpers and the lambda argument / return casts decide by the class too, and `Class.getName` / `getSimpleName` / `getTypeName` / `descriptorString` / `Class.cast` stop judging by the shape (new switch `CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS`); see the wave 25 note under 5 |
| 6 `raise_could_not_initialize_class_for` dots a hidden-shaped name the store does not hold | Info | FIXED (wave 24, lane names; pending orchestrator build): an id the store does not hold keeps the shape rule |
| 7 the JNI re-`<init>` count, otherwise | Info | clean, with notes |
| 8 `name_bridge_enclosing_method_error` against `getEnclosingMethod0` | Info | clean, with notes; probe `R21Fz9ReviewEmUser1` |
| 9 `ClassStore::recorded_file_names` | Info | clean |
| 10 `propagate_finalizer_to_subclasses`: lock order, hook under the lock, both directions | Info | clean, with notes |
| 11 switch `=0` fidelity, process globals, allow-lists | Info | clean |

Probes, written without being run. They follow `r21-fuzz5-probes-20261019.md`: default package, ASCII, deterministic
output, and the HotSpot expectation (by reading) in each header.

| Probe | Targets | Verdict |
|---|---|---|
| `tools/probes/jit/r21/fuzz/R21Fz9ReviewEmUser1.java` | item 8: the wave-22 EnclosingMethod resolution through a USER-DEFINED loader (`R21Fz8ReviewEmGone1` covers only the application loader); an EnclosingMethod class whose own load fails (missing superclass); stickiness once the missing class becomes loadable (loader ask counts); a hidden class with EnclosingMethod (simple throws, canonical is null) | OPEN (run pending; expected to match in all modes) |
| `tools/probes/jit/r21/fuzz/R21Fz9ReviewHexName1.java` | items 5-6: ordinary `fz9/0x2A`, `fz9/0x1F` against hidden classes, across `getName`, arrays, checkcast, `Class.cast`, `aastore` and "Could not initialize class" | OPEN (run pending). After wave 25, lane hexname (pending orchestrator build), every row is expected to match HotSpot in both modes. Before wave 25 the `name`, `simple`, `type`, `str`, `array`, `arraySimple` and `cast` rows printed `fz9/0x2A` (getName judged the shape), and `desc` printed `Lfz9.0x2A;` under `--compatible`. checkcast and aastore were already fixed by wave 24. Per-row list: `r21-fuzz9-probes-20261019.md` item 7 |

No probe is given for items 2-4: they need a JNI library. The C harness on
`r21-finalize-reinit-residuals-FIXED-20261020.md` item 2 covers them, with two variants added below.

## 1. A stub upgrade that flips `has_finalizer` skips the VM's define-time finalizer listener (Low)

- **Where:** `classloading/src/class_manager.rs:18066` (`old_has_finalizer`), `:18229` (the propagation call),
  `:18380` `propagate_finalizer_to_subclasses`. The listener the define runs is `vm/src/vm/vm_init.rs:11485`
  `SharedVm::note_finalizable_define`. It does three things:
  - records the class's non-finalizable ancestors (`ClassStore::mark_finalizable_ancestor`);
  - drops their constructors' allowed rows from `jit_gate_pass`;
  - withdraws their compiled constructor bodies (`JitCache::invalidate_for_finalizable_subtype`).
- **What wave 22 does:** it rewrites the subclasses' bits, and fires `fire_jit_invalidate_hook` for each one that
  moved. That hook (`jit_invalidate_adapter`) drops the class's allocation recipe and flushes the code cache. That is
  enough for `new` to register the class's objects, but nothing runs `note_finalizable_define` for a class that turned
  finalizable. The upgraded class itself is not covered either: the gap predates wave 22, but wave 22 is what makes
  the flip reach subclasses.
- **Scenario (`--compatible`):**
  1. `class B { B(String s) { this(Integer.parseInt(s)); } B(int i) {} }` is loaded.
  2. `class F extends S`, where `S extends B` is a synthetic stub.
  3. `B(String)` compiles. No finalizable class reaches B yet, so the bar allows it and `jit_gate_pass` keeps the row.
  4. The stub `S` is upgraded with real bytes that declare `finalize()`. F turns finalizable.
  5. `new F("x")` registers F's object at `new`. B's compiled constructor throws `NumberFormatException` before
     `Object.<init>`, with no interpreted frame, so nothing withdraws the registration.
  6. HotSpot never registered that object. This VM runs `finalize()` on it: the wave-11 bug, back for this path.
  - Also: F's own constructors' `jit_gate_pass` rows were filled while F was not finalizable.
    `finalizer_bars_compiling_in` reads `has_finalizer` live, but a memoized allowed row is not asked again. The
    flush advances the code-state epoch, while the rows are stamped with the redefine epoch, so they survive it
    (by reading `jit_realm.rs:190`; not run).
- **Reachability:** this needs a synthetic stub upgraded under a loaded subclass, a `finalize()` that only the real
  bytes declare, and a constructor that can throw before `super(..)`. It is `--compatible` only, and rare.
- **Fix sketch:** have the class manager queue every id whose bit moved (the upgraded class and each changed
  subclass). After the write guard drops, the VM runs `note_finalizable_define` for each id that became finalizable,
  and drops each moved class's `jit_gate_pass` rows. Keep it under `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`.
  Lane finvm's uncommitted edits in this worktree (`stub_upgrade_finalizer_flips`, taken at
  `ClassManagerWriteGuard::drop`) take this shape. Check that they also cover the `jit_gate_pass` rows of the class's
  OWN constructors, not only those of its ancestors.

**Verdict (wave 23, lane finvm): FIXED (pending orchestrator build).** Same switch,
`CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`; no new switch.
- `SharedVm::note_stub_upgrade_finalizer_flips` (`vm/src/vm/vm_init.rs`) handles each class that became finalizable,
  the upgraded class or a subclass:
  - it drops the class's OWN `<init>` rows from `jit_gate_pass`;
  - it withdraws its compiled or spliced constructor bodies (`invalidate_for_finalizable_subtype` on the class
    itself);
  - it then runs `note_finalizable_define`, which handles the ancestors as at define.
- **Fill race.** The bit is flipped under the write lock before the drop. The `execute` gate's fill
  (`interpreter.rs`) already re-read the ancestor record after its insert; it now also re-reads the live bar
  (`|| finalizer_bars_compiling(..)`). So a row computed before the flip and inserted after the drop is removed.
  This costs one more bar question per `<init>` row fill.
- **Re-entrancy, reviewed; no change needed.** The drain runs in `ClassManagerWriteGuard::drop` after
  `self.guard = None`, so the write lock is already released. Nothing in it takes the guard again:
  - `note_finalizable_define` takes the raw `class_manager.read()` and `.write()`, not `class_manager_write()`, so
    no nested drain can start;
  - `JitCache::write()` returns `&self`, not a lock;
  - `jit_gate_pass` is held only for its `retain`;
  - a recipe drop takes only the cache's leaf `retired` mutex.

  No thread can hold a class-manager read across a write acquisition. I found no site that keeps `jit_gate_pass`
  held while it takes a class-manager write. The established order is class manager, then JIT, which is the order
  the invalidate hook already uses under the lock.
- **Test:** `vm_init` `r21w14_finalizable_define_listener_tests::a_class_made_finalizable_by_an_upgrade_forgets_its_own_constructor_rows`
  covers both directions and pins the fill re-read.

## 2. The JNI re-`<init>` count is dropped by any collection during the constructor (Low)

- **Where:** `vm/src/native/jni.rs:6853` `jni_reinit_registers_again`, whose first test is
  `shared.mem.address_epoch() != epoch`. The epoch was read before the call (`:6821`).
- **Scenario:** `F f = new F(); reinit(f);`, where F's no-argument constructor allocates enough to trigger a
  collection, or calls `System.gc()`. `HeapRealm::address_epoch` also adds `sweep_slices_begun`, so another thread's
  concurrent sweep moves it too, even when this constructor allocates nothing. HotSpot prints `n=2`, this VM `n=1`.
  The page calls this "the answer before the switch". It is still a divergence, and on a busy VM the common one.
- **Why the epoch is not needed on the normal-return arm:** no unwinder note exists when the call returned normally.
  The only use of the pre-call address is to find the row, and the receiver's CURRENT address is available: the JNI
  handle `obj` is a local-ref slot, which the GC rewrites (`jobject_to_obj(obj)`).
- **Fix sketch:**
  - `!threw`: re-resolve the receiver from the handle, and call `note_finalizer_registered_again` with that
    address, whatever the epoch.
  - `threw`: keep the epoch test. The note is keyed by address and epoch, so a moved epoch cannot be matched, and
    "no count" is the fail-safe there.
  - Variant for the C harness: `F() { byte[][] b = new byte[64][]; for (...) b[i] = new byte[1 << 20]; }`, then
    `reinit(f)`. Expected `2` on HotSpot.

## 3. A JNI `<init>` call that fails before its constructor runs is counted as a second `Finalizer` (Low)

- **Where:** `jni.rs:6853`: `if threw && rp.forget_unconstructed_finalizable(..) { return false; }`, then the count.
  The rule is "a throw the unwinder could not place before `Object.<init>` is taken as after it".
- **Scenario:** some `Err`s from `invoke_on_class_shared` / `invoke_on_class_shared_no_retarget` /
  `invoke_on_receiver_class_selecting` are raised before any frame of the constructor exists:
  - the stack-depth refusal at entry (`StackOverflowError` in a deep native recursion);
  - a resolution or link failure of the method id's class;
  - a `--jdk-only` refusal that `jni_surface_jdk_only` surfaces.

  The unwinder never walked a constructor frame, so there is no note, and the door counts an extra `Finalizer`.
  HotSpot's `JavaCalls::call_helper` checks the stack before it enters the method, so `Object.<init>` never returned
  and there is no second registration.
- **Direction:** an extra `finalize()` on an object that HotSpot finalizes once. The withdrawing door
  (`withdrawable`) uses the same rule, but there it errs toward keeping a registration, not toward adding one.
- **Fix sketch:** count on `threw` only when the failure came out of the constructor's own frames. Two ways:
  - Have the unwinder's entry step (`exception_dispatch.rs:815` `unconstructed_mark_at_entry`, JNI-upcall arm)
    record "a constructor frame on `addr` was unwound" in the same per-processor note list, with a second tag
    ("placed after"). Then the door counts only for a normal return or for that tag.
  - Or, cheaper: treat `MethodCallFailed` variants other than `ExceptionThrown` (refusals) as "not reached", and leave
    `StackOverflowError` as a residual.

  Variant for the C harness: `reinit` called at a recursion depth where the next Java frame overflows. Expected `1`.

## 4. `take_extra_finalizer` scans every finalizer row on each `finalize()`, once any re-`<init>` happened (Low, perf)

- **Where:** `gc/src/reference.rs:4190`, from `gc_and_alloc.rs:14622`, under `ref_processor.lock()`.
- **Cost:** `extra_finalizers_noted` is sticky. After ONE JNI re-`<init>` anywhere in the VM, every later
  `finalize()` does a `rev().find` over `finalizer_refs`. That vector holds every live finalizable object, and the row
  being finalized was registered at its allocation, so it is usually deep. Draining N finalizable objects costs
  O(N x rows), under the lock that every finalizable `new` takes (`register_finalizable`).
- **Fix sketch:** replace the sticky bool with a count of outstanding extra registrations:
  - `+1` in `note_finalizer_registered_again`;
  - `-1` in `take_extra_finalizer`, and in `remove_collected` for a pruned row whose count is not zero.

  `take_extra_finalizer` then answers `false` at once whenever the count is 0, which is the steady state after the
  re-initialized objects are finalized.

## 5. The context-free renderers still judge the `/0x` tail by the name's shape, arrays included (Info)

- **Where:** wave 22 moved five sites to `hotspot_external_name_of_class` (`exceptions.rs:1792`):
  - `jni_array_store_message`;
  - `vh_array_display_name`;
  - `NativeExceptionAccess` `class_cast_message`;
  - `method_handle_constant_access_refusal_once`;
  - `raise_could_not_initialize_class_for`.

  About 40 `hotspot_external_name` callers remain shape-judged. Among them is the `RuntimeError` funnel
  (`exceptions.rs:2605`, `ArrayStoreException`) that the interpreter's `aastore` reaches, and probably the
  interpreter's checkcast message. The w15-native page says so ("the context-free renderers stay shape-judged").
- **Arrays:** `hotspot_external_name_by_flag` keeps the shape rule for every `[`-name. HotSpot's
  `Klass::external_name()` uses the hidden spelling for an object array only when its BOTTOM class is hidden. So
  `[Lfz9/0x2A;`, an array of an ORDINARY class, prints `[Lfz9.0x2A;` on HotSpot and `[Lfz9/0x2A;` here.
- **Reachability:** a class whose last segment is `0x<hex>` comes only from hand-built bytes (javac cannot write
  it). This is info-level.
- **Fix sketch:** where an array's class is held, pass the bottom element's `hidden` flag (walk `array_info` to the
  element). For the funnel, carry the class id (or a `hidden` bit) in `RuntimeError::ArrayStoreException`. The probe
  shows which texts still differ.

**Verdict (wave 24, lane names): FIXED for the class-holding renderers (pending orchestrator build).** Same switch,
`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS` (default ON); `=0` gives each site its previous text exactly.
- `exceptions.rs` `bottom_class_hidden_in` / `bottom_class_hidden` walk `array_info` from the held id (a class, or a
  reference array's component id) to the bottom class, and answer its `hidden` flag only when that class is the one
  the text names; anything else (id not in the store, broken chain, primitive bottom, name mismatch) answers
  `None`, and the site keeps its shape rule. `external_name_by_bottom_class_or` renders by that flag:
  `[Lfz9/0x2A;` of an ordinary class is `[Lfz9.0x2A;`, of a hidden one `[Lfz9/0x2A;`.
- Sites: the interpreter `checkcast` operands (`opcodes.rs`; the old shape call also DOTTED a hidden array's tail,
  because the trailing `;` defeated the hex test), the `aastore` texts of the interpreter (both twins) and of the JIT
  store helper (`array_store_element_text`, rendered before the `RuntimeError` funnel, whose shape rewrite leaves the
  result unchanged, so `RuntimeError::ArrayStoreException` keeps its shape), `jni_array_store_message` (the value
  and the array's bottom component), `vh_array_display_name` and `NativeExceptionAccess::class_cast_message`.
- Not changed: the remaining context-free `hotspot_external_name` callers (they hold no class).
- Test: `exceptions.rs` `tests::an_array_is_named_by_its_bottom_class_hidden_flag`. Probe: `R21Fz9ReviewHexName1`
  checkcast and aastore rows are now expected to match HotSpot in both modes.

**Wave 25, lane hexname (pending orchestrator build).** The probe's class-name rows are fixed too. Wave 24 left them
out because they are not message renderers. Two switches cover the change:

- `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`, the same switch as wave 24, now covers the rest of the cast texts:
  - `vm/src/jit/helpers.rs` `jit_checkcast_body`: an array receiver is named by its bottom class's flag (the bare
    `replace` also dotted a hidden bottom's tail), and the target by the site's resolved class
    (`typecheck_target_for_site`), so a hidden target keeps its tail;
  - the JIT `checkcast java/lang/Double` helper of the lambda int-to-double unbox;
  - the two `lambda.rs` cast refusals (an argument of the instantiated method type, and the `Number` / `Character` /
    `Boolean` return unbox).

  `=0` gives each site its previous text.
- New `CRATONVM_CLASS_NAME_HIDDEN_BY_CLASS` (default ON; `=0` restores the shape rule) covers the
  `native-builtins/src/lang_class.rs` name answers. `dotted_binary_name_of_class` keeps a `/0x<hex>` tail only
  when the bottom class is hidden. `bottom_class_hidden_of` finds that class by walking `array_component_class_id`
  and checking the name, so a mismatched id, a lambda-proxy id or an unknown id keeps the shape rule. It is used by:
  - `native_class_get_name`, on its strict, by-id and legacy paths. The by-id path no longer serves a `/0x`-tailed
    name from the shared dotted cache, because stack trace elements fill that cache without the flag;
  - `top_level_simple_name_of_class` (`getSimpleName`), and the array fast path of `getTypeName`;
  - `native_class_descriptor_string` (`--compatible`). For an ordinary class it now answers `Lfz9/0x2A;`, where
    the shape gave `Lfz9.0x2A;`;
  - `class_get_name_of` and the target of `native_class_cast`, through `reflect_text_class_name_of_class`.

  The answer is decided only for a name whose shape looks hidden, so other names pay nothing.
- Tests: `exceptions.rs` `tests::the_compiled_cast_texts_are_named_by_the_bottom_class`, and `lang_class.rs`
  `tests::r21w25_a_hex_tail_is_hidden_only_for_a_hidden_class`.
- Still judged by the shape (they hold no class, or are outside this probe):
  - the `dotted_class_name` cache users (stack trace elements, StackWalker);
  - `reflect_text_class_name` callers that are not listed above;
  - `package_name_of` (`getPackageName`), `array_descriptor_to_package_name` and `runtime_package_name`
    (the same-runtime-package test of access checks). All three cut the name at `/0x`, so an ordinary
    `fz9/0x2A` answers package `""` where HotSpot answers `fz9`. For the access test this means a
    package-private member shared with `fz9/Other` would be refused. No row of the probe reaches it; it is a
    candidate for the next naming lane;
  - `getCanonicalName`;
  - the native-api `aastore_value_external_name` stand-ins;
  - `proxy_cast_refusal_message`;
  - the `lambda.rs` cast target names (taken from a descriptor token);
  - the NoSuchMethodError texts.

## 6. `raise_could_not_initialize_class_for` dots a hidden-shaped name the store does not hold (Info)

- **Where:** `exceptions.rs:6948`. `class_is_hidden` (`:6912`) answers `false` when `get_class(class_id)` is `None`.
  With `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS` on, the new second arm then dots the whole name:
  `p/Main$$Lambda/0x1a` gives `p.Main$$Lambda.0x1a`. Before wave 22 the shape rule kept the tail.
- **Reachability:** an id with no store entry and a recorded initialization error. The ids that are not in the store
  are the synthetic lambda-proxy ids (w22 item 8). This is unlikely: a JDK 25 lambda proxy has no failing `<clinit>`.
- **Fix sketch:** read `get_class(class_id).map(|c| c.hidden)` once. `Some(h)` picks the arm, and `None` keeps the
  shape rule (`raise_could_not_initialize_class`).

**Verdict (wave 24, lane names): FIXED as sketched (pending orchestrator build).** `class_is_hidden` became
`class_hidden_in_store` (`Option<bool>`): `Some(true)` keeps the tail, `Some(false)` dots under
`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`, and `None` (the synthetic lambda-proxy ids, hidden on HotSpot) keeps the shape
rule, so `p/Main$$Lambda/0x1a` prints `p.Main$$Lambda/0x1a`. No new switch: `=0` was already the shape rule for
every arm but the hidden one. Test: `exceptions.rs` `tests::an_id_the_store_does_not_hold_has_no_hidden_answer`.

## 7. The JNI re-`<init>` count, otherwise (Info: clean, with notes)

Checked against HotSpot (`_return_register_finalizer` at every `Object.<init>` return, one `Finalizer` each, each
with its own "already finalized" state):

- **Double finalize:**
  - The count is armed only for a finalizable receiver with an ACTIVE row before the call (`jni.rs:6821`). That is
    exactly when `jni_constructor_call_withdrawable` answers `None`, so a first construction (an `AllocObject`
    instance) is never counted twice.
  - The `<init>`-return hook (`note_finalizable_init_returned`) only marks `constructed`, and adds no count.
  - `discover_reference` on an active row is still a no-op.
- **Lost finalize:**
  - Each run takes one count (`take_extra_finalizer`, the newest enqueued row at the address) and re-queues the object
    BEFORE `finalize()` runs. The queue roots and relocates it, so a resurrecting or allocating first run cannot lose
    the second.
  - `enqueue_unfinalized` refuses only an address already waiting, and the object was just dequeued.
  - The inline drain and the delivery thread both loop until the queue is empty, so `System.runFinalization()` sees
    both runs, as HotSpot's (both `Finalizer`s are pending from the same collection).
  - Every production enqueue marks the row `enqueued` first (`mark_finalizer_enqueued`, `process_final_refs`), so
    the `e.enqueued` filter finds it.
- **GC retention:** one more queue residency between the two runs. HotSpot also keeps the object reachable from the
  second `Finalizer` until it runs. Rows are relocated with `relocate_list`, so `reference_obj` (read by
  `note_finalizer_registered_again`) and `referent` (read by `take_extra_finalizer`) stay equal for a FINALIZER row.
- **Withdraw and unwinder notes:**
  - A counted row is `constructed`, which `withdraw_finalizable` refuses.
  - The JNI upcall note is consumed by `forget_unconstructed_finalizable` without a withdrawal.
- **Notes:**
  - Under `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN=0`, `withdraw_finalizable` ignores `constructed`, so a counted
    row can be withdrawn and its count lost. A row re-registered from `withdrawn_finalizables` starts again at 0.
    Both paths are reachable only from unverified code (residuals page, item 1).
  - The door takes the processor lock twice per finalizable JNI `<init>`, once in `jni_constructor_call_withdrawable`
    and once in `jni_reinit_of_registered_finalizable`. Both read `is_registered_finalizable`, so one read could feed
    both.
  - Test gap: no test drives `run_finalizers_impl`'s re-queue. The gc test pins `take_extra_finalizer`, and the JNI
    test pins the door. A test that registers an object, notes one extra registration, enqueues it, and counts two
    `finalize()` runs through `run_finalizers_impl` would pin the second run. The JNI test's `return` inside
    `for on in [true, false]` also skips the `off` arm when the allocation cache is published.

## 8. `name_bridge_enclosing_method_error` against `getEnclosingMethod0` (Info: clean, with notes)

`native-builtins/src/lang_class.rs:39026`, read against JDK 25 `Class.java` and HotSpot `JVM_GetEnclosingMethodInfo`:

- **`getSimpleName`:** `isArray()` recurses into the component; `isTopLevelClass()` is
  `!hasEnclosingMethodInfo() && getDeclaringClass0() == null`; and `getEnclosingMethod0` resolves the class with
  `klass_at(.., CHECK_NULL)`. The bridge walks to the element, resolves only when EnclosingMethod is present, and is
  asked exactly in the arm where `name_bridge_asks_declaring_class` skips the declaring-class checks.
- **`getCanonicalName`:** the hidden exit comes first (`isHidden() || isLocalOrAnonymousClass()`), on the element, as
  HotSpot's recursion does.
- **Not gated on "no user-defined loader":** right, HotSpot resolves whatever loaders exist.
  `R21Fz9ReviewEmUser1` is the first probe on that arm.
- **The recorded door:** `class_seen_by_referencing_class_checked` gives the sticky rethrow, with its cause. That is
  HotSpot's resolution-error table, keyed here by (class, name) rather than by constant-pool index. javac shares one
  `Class` entry per name, so the two keys agree.
- **`--jdk-only`:** it runs the JDK body. The nesting predicates (`class_nesting_predicate`) are registered only by
  `register_synthetic_overrides` (`synthetic-jdk`), so in `--compatible` `isAnonymousClass` / `isLocalClass` /
  `isMemberClass` run the JDK bodies too, through the checked `getEnclosingMethod0`.
- **Notes:**
  - `klass_at` also runs `verify_constant_pool_resolve`, an access check. An EnclosingMethod class that is not
    public, in the same package name but another runtime package (another loader), is an `IllegalAccessError` on
    HotSpot. The door only resolves. That needs hand-built or split-loader bytes.
  - The cache-first exit inherits w22 item 4's caveat: `SIMPLE_CLASS_NAME_CACHE` is also filled by
    `resolve_simple_name` outside the bridge.

## 9. `ClassStore::recorded_file_names` (Info: clean)

- **Unload and id reuse:** the row is written once, at `class_manager.rs:12516`, just before `class_store.add`,
  with no fallible step between them. A failed define therefore leaves no row behind for the next `next_id()`.
  `ClassStore::remove` (`class.rs:3163`) has only the unload caller, and it drops the row. A stub upgrade and a
  redefinition mutate in place (`get_mut`), so the row survives them, as it should. Ids are not reused.
- **Lock order:** the map lives inside the store, under the class-manager guard, with no new lock. The store-only
  unwinder reads it through the `&ClassStore` it already holds.
- **Renamed ordinary defines** are recorded but never answer: `hidden_self_name` and `hidden_class_file_name` both
  require `hidden`. They cost one map row per ordinary define whose override name differs from `this_class`, which
  HotSpot refuses outright ("wrong name"), so the rows are rare.

## 10. `propagate_finalizer_to_subclasses`: lock order, hook under the lock, both directions (Info: clean, with notes)

- **Order:** `descendants_of` is breadth-first, so each parent's bit is final before its children read it, as
  `recompute_subclass_layouts` relies on. The rule is the define's (`class_has_finalizer_by_hotspot_rule`): an own
  non-empty `finalize()` keeps its bit, and an own empty one cancels the inherited bit (HotSpot
  `set_precomputed_flags`). The test covers both directions.
- **The hook under the class-manager write lock:** `jit_invalidate_adapter` takes the hook-VM registry, the
  allocation cache's `retired` mutex and the JIT cache write lock, never the class manager. The two pre-existing
  calls in the same function (the layout hooks) already run there, so the lock order is unchanged.
  - Perf note: each changed subclass flushes the VM's whole code cache. N changed subclasses mean N flushes. A
    single flush after the loop, plus a per-id allocation-cache drop, would do.
- **False to true:** instances allocated while the stub stood were never registered. HotSpot finalizes them, and no
  heap walk here can recover that. Item 1 covers the VM-side state that is left stale.
- **True to false:** already-registered instances keep their rows. Their `finalize()` then dispatches to the real
  parent's EMPTY body, which is invisible. The only trace is one more collection before Phantom/Cleaner delivery.

## 11. Switch `=0` fidelity, process globals, allow-lists (Info: clean)

- `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN=0`: `VmConfig::jni_reinit_registers_finalizer_again` gates both
  doors, so nothing is counted and nothing is taken. This is the one-`finalize()` answer.
- `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES=0`: no propagation and no hook. The subclasses' old bits stay.
- `CRATONVM_HIDDEN_SELF_RECORDED_NAME=0` has three effects:
  - no renamed-define row is written;
  - `dispatch_static` takes the pure suffix arm;
  - the unwinder's OR is skipped.

  The class manager's doors and `is_self_class_reference_in` keep wave 21's recorded name, which had no switch. That
  is consistent: `=0` restores the pre-wave-22 answers.
- `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS=0`: the shape rule at all six sites. It is read from the flag table per message
  (message paths only), as other text switches are.
- **Globals:** none added. The switches are `VmConfig` fields, a latch on the `ClassStore` or `ClassManager`, or the
  `ReferenceProcessor`'s own `extra_finalizers_noted`. `SIMPLE_CLASS_NAME_CACHE` / `CANONICAL_CLASS_NAME_CACHE`
  predate wave 22 and are keyed by VM identity.
- **Allow-lists:** none added. The JDK-prefix list in `outer_resolution_errors_apply` dates from round 18.
