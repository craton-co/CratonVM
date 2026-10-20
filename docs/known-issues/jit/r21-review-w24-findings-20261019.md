# JIT round 21 wave 24: adversarial review of wave 23 (lane review-w24)

Status: OPEN. 10 items. Wave 24 is the last wave of round 21, so each item is classified:

- **FIX-NOW:** small and clear, for a lane in this wave.
- **FILE:** stays on this page for the next round.

Item 1 is the only FIX-NOW defect: a JNI re-`<init>` early return leaves a stale unwinder note. Item 2 is FIX-NOW as a
narrowing of wave 23's item-3 rule. Item 9 is a doc-only FIX-NOW. Everything else is FILE or clean. Nothing was fixed:
this lane may not edit `.rs` files.

Scope: `git diff 98007436be 4d9c45daac -- '*.rs'` (wave 23 and 23b), read at HEAD 4d9c45daac, together with
`r21-review-w23-findings-20261019.md`. This review only read code. Nothing was built or run, and wave 23 itself is
unbuilt. Line numbers are HEAD's.

## Item status

| Item | Severity | Class | Verdict |
|---|---|---|---|
| 1 `jni_reinit_registers_again` returns before it consumes the unwinder note when the failure "cannot have reached" the constructor | Low (a later re-`<init>` of the same object misses its count) | FIX-NOW | FIXED (wave 24, lane finalize; pending orchestrator build): `vm/src/native/jni.rs` `jni_reinit_registers_again` decides `reached` before the processor lock, consumes the note under it (`forget_unconstructed_finalizable`), then refuses a "not reached" failure. Same switch, `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME`; with `=0` `reached` stays `true`, the wave-22 path. Test `r21w22_jni_reinit_of_a_registered_object_counts_a_second_finalizer`: a note, the SOE failure returns `false`, the note is gone |
| 2 the item-3 rule takes every `MethodCallFailed::InternalError` as "not reached", including the Java-visible `VmError::Runtime` / `Linkage` that escape a running constructor | Low (one `finalize()` where HotSpot runs two) | FIX-NOW (narrowing); FILE (exact answer) | Narrowing FIXED (wave 24, lane finalize; pending orchestrator build): `jni.rs` `jni_reinit_failure_reached_ctor` answers "reached" for `InternalError(VmError::Runtime(_) \| VmError::Linkage(_))` except `Runtime(StackOverflowError)`; every other `InternalError` stays "not reached". Same switch. The JNI test has the NPE, Linkage and SOE cases. Exact answer PARTLY FIXED (wave 25, lane initwitness; pending orchestrator build): a per-row `Object.<init>`-return witness answers "reached" whenever the interpreter's `<init>`-return hook saw the return, behind `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS` (default ON); a silent witness still falls back to the error-kind rule, see the section |
| 3 flip rows queued under a raw `class_manager.write()` are settled only at the next guarded release | Low (`--compatible` stubs; short window) | FILE | FIXED (wave 25, lane rawwrite; pending orchestrator build): the six named sites were all unit tests; the 26 PRODUCTION raw sites that reach a loader now take `ClassRealm::class_manager_write_settling()`, whose release settles the section's flips and does nothing else, behind `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS` (default ON, `=0` restores the wait). Per-site reasoning in the section |
| 4 the settle runs after `before_hooks`, so a waiting loader can run before it | Info | FILE (note) | clean, with notes. Wave 25, lane rawwrite: order left as it is, on purpose; why in the section |
| 5 the flip queue in `ClassManagerWriteGuard::drop`: re-entrancy, panics, unregistered VM, non-Java thread, lock order | Info | none | clean, with notes |
| 6 `note_stub_upgrade_finalizer_flips`: withdrawal completeness, both directions; the gate-fill re-ask | Info | FILE (two notes) | clean, with notes |
| 7 `extra_finalizers_outstanding`: every row-removal path, underflow, overcount | Info | none | clean |
| 8 the door test `r21w23_a_counted_reinit_runs_finalize_twice_through_the_door` cannot see the second run | Info (test gap) | FILE | FIXED (wave 24, lane finalize; pending orchestrator build): the door counts each ACCEPTED re-queue (`gc/src/reference.rs` `FinalizerThread::note_extra_run_requeued` / `extra_runs_requeued`, `gc_and_alloc.rs` `run_finalizers_impl`), and the door test asserts 1 with the switch on and 0 with it off, so a refused re-queue fails it. Counting the `finalize()` bodies themselves still needs a class with a counted side effect |
| 9 `r21-review-w21-findings` item 3 residual names identifiers that wave 22 removed | Info (doc) | FIX-NOW (doc) | FIXED (wave 24, lane finalize): row 3 of `r21-review-w21-findings-20261019.md` now names `ClassStore::recorded_file_names` / `recorded_file_name(id)` and points to w22 item 2 |
| 10 the call-entry null-guard mark move, `=0` fidelity, globals, allow-lists, other pages' FIXED verdicts | Info | none | clean |

No probe is given. Items 1 and 2 need a JNI library: the C harness on `r21-finalize-reinit-residuals-FIXED-20261020.md`
item 2 covers them. Item 1 is also pinned by the unit-test extension under it. Items 3-6 need a synthetic stub upgraded
under a loaded subclass while a constructor compiles, which a deterministic Java probe cannot arrange.

## 1. `jni_reinit_registers_again` returns before it consumes the unwinder note (Low, FIX-NOW)

- **Where:** `vm/src/native/jni.rs` `jni_reinit_registers_again`. In the failure arm (`_ =>`), under
  `jni_reinit_count_by_outcome`, the `!jni_reinit_failure_reached_ctor` test returns `false` before the processor lock
  is taken. `rp.forget_unconstructed_finalizable(addr, epoch)` is therefore never reached.
- **Scenario:**
  1. A JNI `CallNonvirtualVoidMethod(obj, F, F.<init>)` runs on a registered, finalizable `obj`. The constructor
     overflows the stack before its `super(..)`, for example `F() { this(depth()); }` with deep recursion in
     `depth()`.
  2. The unwinder walks the constructor frame and its JNI-upcall arm notes `(addr, epoch)` in
     `unconstructed_finalizables`.
  3. The door sees `ExceptionThrown(StackOverflowError)`, answers "not reached" and returns. The note stays.
  4. A second JNI re-`<init>` of the same object in the same address epoch now throws AFTER `Object.<init>` returned.
     The unwinder places no note for it, but `forget_unconstructed_finalizable` finds the stale note from step 2 and
     returns `true`, so the door counts nothing.
  5. HotSpot registers a `Finalizer` at that `Object.<init>` return, so `finalize()` runs once more there than here.
- **Bound:** the note list is capped (`UNCONSTRUCTED_FINALIZABLES_MAX`) and purged at the next epoch
  (`note_unconstructed_finalizable` retains only the current epoch). So the effect is one missed count, and nothing
  grows. Nothing is withdrawn by mistake either: the withdrawing door (`take_unconstructed_finalizable`) is never asked
  for an object with an active row.
- **The same skip is harmless for `InternalError`:** a VM error leaves its frames without a Java unwind, so there is
  no note to consume.
- **Fix:**
  - Compute `reached` BEFORE the lock. `jni_reinit_failure_reached_ctor` takes `class_manager.read()`, and a read
    under `ref_processor.lock()` would add a new lock order.
  - Then, under the lock: `if failure.is_some() && rp.forget_unconstructed_finalizable(addr, epoch) { return false; }`,
    then `if !reached { return false; }`, then the count.
  - This changes no answer except step 4's.
- **Test:** extend `r21w22_jni_reinit_of_a_registered_object_counts_a_second_finalizer`.
  1. Call `shared.note_unconstructed_finalizable(addr, epoch)`.
  2. Call the door with the SOE failure. It must return `false`.
  3. Assert `!rp.forget_unconstructed_finalizable(addr, epoch)`: the note is gone.

## 2. The item-3 rule takes every `InternalError` as "not reached" (Low, FIX-NOW narrowing; FILE exact answer)

- **Where:** `jni.rs` `jni_reinit_failure_reached_ctor`, whose arm is
  `MethodCallFailed::InternalError(_) => false`.
- **What the page assumed:** an `InternalError` is a VM refusal before entry. The w23 verdict lists a `--jdk-only`
  refusal and a link failure of the method id's class.
- **What else is an `InternalError` here:** `MethodCallFailed::InternalError(VmError::Runtime(..))` and
  `(VmError::Linkage(..))`. `jni_surface_jdk_only` turns these into the Java throwable each one names
  (`jni_vm_error_as_throwable`, `CRATONVM_JNI_UPCALL_TYPED_ERRORS`, default ON). They are the errors that leave the
  constructor's own body without a Java unwind ("an up-call that failed inside the VM dropped its frames", the
  `vm_error_upcall_seen` comment). For example, a `RuntimeError::NullPointerException`, `ArithmeticException` or a
  `NoClassDefFoundError` raised by an instruction after `super(..)` returned.
- **Before and after wave 23:** such a failure leaves no unwinder note, so the door cannot tell whether it was raised
  before or after `Object.<init>`.
  - Wave 22 counted it. That is right when it was raised after `Object.<init>`.
  - Wave 23 does not count it. That is right when it was raised before.
  - Ordinary constructor code runs mostly after its `super(..)`, so the old answer was the likelier one for these two
    variants.
  - The variants that really are refusals at entry are not Java-visible exceptions: `VmError::JdkOnly`,
    `VmError::Internal`, `InvalidConfiguration`, `ClassFile`.
  - The depth refusal at entry can also arrive as `VmError::Runtime(RuntimeError::StackOverflowError)`.
  - A link failure of the method id's class cannot happen at call time: `GetMethodID` already linked and initialized
    it.
- **Fix (FIX-NOW, same switch `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME`):**
  - `InternalError(VmError::Runtime(RuntimeError::StackOverflowError))` answers "not reached";
  - every other `InternalError(VmError::Runtime(_) | VmError::Linkage(_))` answers "reached", as `ExceptionThrown`
    does;
  - every other `InternalError` stays "not reached".

  Add the three cases to the JNI test, next to the existing `refused` case (`VmError::Internal`, still `false`).
- **Exact answer (FILE):** a per-call witness that `Object.<init>` returned on the receiver. For example, the
  `<init>`-return hook (`note_finalizable_init_returned`) bumps a per-row counter, and the door compares it before and
  after the call: a counter that moved means `Object.<init>` returned. That would also retire the SOE residual that
  wave 23 documents at the site, and the unwinder-note arm.
- **Exact answer: wave 25, lane initwitness (pending orchestrator build).** Switch
  `CRATONVM_JNI_REINIT_INIT_RETURN_WITNESS` (`VmConfig::jni_reinit_init_return_witness`, default ON). With `=0`, or
  with `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME=0`, the wave-24 rule above answers unchanged.
  - **The witness:** `ReferenceEntry::init_returns` (`gc/src/reference.rs`), a per-row counter. It is not a process
    global: it lives on the receiver's own finalizer row, in the VM's reference processor. It is bumped where this
    VM already treats `Object.<init>` as returned: `ReferenceProcessor::note_finalizable_init_returned`, which the
    interpreter's `return` arm calls (`exception_dispatch.rs` `finalizer_init_returned`, gate bit
    `DG_FINALIZER_INIT_RETURN`). A verified constructor returns only after its `super(..)` / `this(..)` call has
    returned, so the bump proves that `Object.<init>` returned on the object.
  - **The door:** `jni.rs` `jni_reinit_init_return_witness` reads the row's counter before the call, at both
    re-`<init>` sites. `jni_reinit_registers_again_witnessed` reads it again after the call, under the same
    processor lock that consumes the note. A failure is then decided in this order:
    1. the unwinder's note: "not reached";
    2. a witness that moved: "reached", whatever the error kind;
    3. otherwise, the wave-24 error-kind rule (`jni_reinit_failure_reached_ctor`).
  - **What this retires:** whenever the hook saw the return, it retires the wave-23 SOE residual (a
    `StackOverflowError` raised after a super constructor returned). It also retires the wave-24 guess for
    `InternalError`.
  - **Still OPEN: a silent witness is no proof.** The hook sees only interpreted constructor returns. So a failure
    whose chain returned only through these keeps the error-kind guess:
    - a compiled constructor;
    - `Object.<init>` itself, where the interpreter elides its empty body (`EMPTY_BODY_ELISION`, the trivial-ctor
      doors);
    - the JIT's elided construction.
    The common shape is `F() { super(); throw ...; }` with `F extends Object`. To close it, every tier would have to
    note an `Object.<init>` return on a finalizable receiver, for example a per-thread "armed" bit that only the JNI
    door sets, tested at the elision sites.
  - **Known imprecision:** the counter is per object, not per call. A nested re-`<init>` of the same object during
    the call (another thread, or a native the constructor calls) also moves it. The door then counts as "reached"
    a failure that may not have been. This needs a constructor that re-initializes its own already-constructed
    receiver through a second path.
  - **Tests:**
    - `gc` `the_init_return_witness_moves_per_active_row`;
    - the JNI test `r21w22_jni_reinit_of_a_registered_object_counts_a_second_finalizer`. It checks that a silent
      witness keeps the rule (SOE and refusal: no count), that a moved witness counts both, and that the note still
      wins.

## 3. Flip rows queued under a raw `class_manager.write()` wait for the next guarded release (Low, FILE)

- **Where:** `ClassManager::stub_upgrade_finalizer_flips` is drained only by `ClassManagerWriteGuard::drop`
  (`vm/src/vm/realms/class_realm.rs:2425`). A raw `parking_lot` write guard has no drop hook. The field's doc says so
  ("a raw write lock that strands a row leaves it for the next release").
- **Production raw-write sites that can upgrade a stub:** `load_class` upgrades at `class_manager.rs:9636`, and
  `define_class_shared_parsed` at `:11365`. They are reached from:
  - `vm/src/runtime/interpreter/typecheck.rs:3652`, `:3711`, `:3777` (`class_manager.write().load_class(name)`);
  - `vm/src/runtime/interpreter/invoke.rs:9612` (`define_class_with_options`);
  - `vm/src/runtime/instrument_reconstitute.rs:1125`, `:1146`.
- **Effect:** until some later `class_manager_write()` releases on any thread, two things stay stale:
  - the class's own allowed `<init>` rows in `jit_gate_pass`;
  - the ancestors' compiled constructor bodies. Item 1 of the w23 page is the scenario: B's compiled constructor
    throws before `Object.<init>` and keeps the registration.

  The allocation recipe is NOT stale: `propagate_finalizer_to_subclasses` (subclasses) and
  `upgrade_synthetic_class:18307` (the class itself) already fire `fire_jit_invalidate_hook` under the lock. Rows are
  delayed, never lost: the next guarded release of the same store settles them, and every class define releases a
  guard.
- **Fix sketch:** move the six sites to `class_manager_write()`. This is not mechanical, so it is FILE: the guard also
  drains JVMTI class hooks and runs two resweeps, and some of these sites may sit under locks that make that unsafe.
  A census by `rg -A6 'class_manager\.write\(\)'` for `load_class|define_class|try_ensure_synthetic_class` lists the
  rest. Most hits are tests in `vm/src/jit/helpers.rs`.
- **Resolution: wave 25, lane rawwrite (pending orchestrator build).** Switch
  `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS` (default ON, `=0` restores the wait), latched per VM in
  `ClassRealm::raw_write_settles_finalizer_flips`.
  - **The six named sites are all unit tests.** `typecheck.rs` `:3652` and `:3711` sit in
    `mod i1_l2_array_assignability_tests`, `:3777` in `mod i11_l2_resolved_array_verdict_tests`, `invoke.rs:9612` in
    `mod hidden_self_cp_tests`, and both `instrument_reconstitute.rs` sites in its `mod tests`. Their VMs are a bare
    `SharedVm::new`, which never registers for hooks, so even a guarded release would leave their rows queued (item
    5). They were left as they are.
  - **The real production set** comes from a multi-line census (`class_manager` and `.write()` are usually on
    separate lines, which is why `rg -A6 'class_manager\.write\(\)'` did not find it). 26 non-test raw sites reach
    `load_class`, `try_ensure_synthetic_class`, `ensure_generated_class` or `load_array_class_for_loader`. Each of these
    can upgrade a stub in place: `fabricate_class` re-scans a loaded stub, and the array path loads its component
    through `load_class`.
    - `native/jni.rs` `jni_class_or_refuse`;
    - `runtime/exceptions.rs` `create_exception_object`, `create_exception_object_no_collect`;
    - `runtime/interpreter.rs` `resolve_array_class_loader_aware_impl`, three sites;
    - `runtime/interpreter/constants.rs` `always_visible_boot_class`;
    - `runtime/interpreter/invoke.rs` `normalize_aastore_value` (boxing);
    - `runtime/interpreter/lambda.rs` `lambda_impl_owner_class_id`;
    - `runtime/interpreter/typecheck.rs` `proxy_target_class_by_name`;
    - `vm/vm_exec.rs`, 15 sites: `native_alloc_layout`, `build_current_thread_object`,
      `ensure_thread_interrupt_lock`, `array_class_id_for_loader`, `list_initiated_class_ids` (the primitive-array
      preload), `allocate_instance`, `ensure_vm_internal_class`, the native context's `try_ensure_synthetic_class`,
      `proxy_invoke_handler`, `proxy_invoke_handler_shared`, `proxy_class_array_component`,
      `proxy_descriptor_to_class_mirror`, `proxy_box_value_for_desc`, `proxy_canonical_boolean`, `proxy_box_value`;
    - `debug/mod.rs` `ensure_startup_array_classes` (`experimental-debug`).
  - **The remaining production raw writes cannot upgrade, so they stay raw:** the classpath mutators
    (`extend_*_classpath`, `retract_application_classpath`; they only clear the upgrade-absent memo), the
    `module_registry` edits, `register_virtual_component`, and the listener's own `mark_finalizable_ancestor` write
    (`vm_init.rs` `note_finalizable_define`).
  - **Why not `class_manager_write()` at these sites.** Its release also drains this thread's queued JVMTI class
    hooks, which can run an agent callback that runs Java code and waits out a pause. It also runs the deferred-`new`
    and profile-replay resweeps. These sites sit in exception construction, array resolution, boxing and
    proxy-`Method` building, and there a hook drain would change when a `ClassLoad` is posted. That drain is
    item-3-adjacent, not item 3, and it would need its own census.
  - **What the sites take instead:** `ClassRealm::class_manager_write_settling()` returns a
    `ClassManagerFlipSettlingGuard`. Its release does one thing: it takes the flips while the lock is held, through the
    same `take_owned_stub_upgrade_finalizer_flips` the guarded release now calls, releases, then calls
    `SharedVm::note_stub_upgrade_finalizer_flips`. Under `=0` it is the raw lock.
  - **Per-site checks.** The same reasoning holds at every site:
    - **Lock order:** while the lock is held the settle adds one length test and, only if a row is queued, the
      hook-registry read. That is the order class manager, then registry, which item 5 checked. After the release it
      takes the allocation-recipe cache, a class-manager read, `jit_gate_pass` (held for one `retain`), the JIT cache
      (whose `write()` is `&Self`; `invalidate_matching` takes its `mutation` lock), then the listener's raw read and
      write. These are the locks the guarded release already takes after every define. A site cannot hold a
      lower-ranked ordered lock, because its own class-manager write would already trip the order assertion.
      `jit_gate_pass` is held only for statement-local `insert` / `remove` / `retain` (`interpreter.rs` `execute`,
      `gc.rs`), and none of them encloses a class load. No site holds a class-manager read across its write: that
      would self-deadlock today.
    - **Scope:** 23 sites are statement temporaries, such as
      `shared.classes.class_manager_write_settling().load_class(n)`. Three are `let` bindings: the primitive-array
      preload, the native context's `try_ensure_synthetic_class` and the debug arrays. The release point is the same as the raw guard's. The two
      `match` scrutinees (`jni_class_or_refuse`, and the classifier in the native context's `try_ensure_synthetic_class`) run only `tracing` or
      `classify_loaded_name` under the lock, as before.
    - **Re-entrancy:** the settle defines nothing, so it cannot queue a flip and cannot re-enter a loader.
    - **Thread:** the settle needs no `JvmThread`, no safepoint and no JNI context, so it is safe on a thread that has
      none: JNI, proxy invocation, the debugger agent thread.
    - **Cost:** one `Vec::len` per release that upgraded nothing.
  - **Tests:**
    - `vm_init.rs` `the_settling_write_release_settles_a_stub_upgrades_finalizer_flips`: a stub upgrade under the
      settling lock leaves an empty queue and an unpublished subclass recipe. Under `=0` the subclass's row is still
      queued.
    - `class_realm.rs` `r21w25_raw_write_settling_tests::no_production_raw_class_manager_write_reaches_a_loader`:
      the nine files' non-test code contains no chained `classes.class_manager.write().<loader>(`.
  - **Not changed (note):** the `class_manager_write` doc still says every former direct `.write()` site was switched
    to the guarded lock. The census shows that is not so for the JVMTI drain either: a `ClassLoad` queued under one of
    these sites waits for this thread's next guarded release. That was out of this lane's scope.

## 4. The settle runs after `before_hooks` (Info, FILE note)

- `ClassManagerWriteGuard::drop` releases the lock, runs `before_hooks` (which clears
  `load_class_concurrent_for`'s per-name loading flag), and only then calls `note_stub_upgrade_finalizer_flips`.
  A second loader waiting on that flag can proceed in between. It can run `new F` and enter F's constructor through a
  stale allowed `jit_gate_pass` row before the row is dropped.
- **The order is right:** `before_hooks` must run first. Its doc records the lock-order deadlock (the per-name mutex
  against the class-manager lock), and the waiter is not GC-safe. The settle's JIT withdrawal must not run in front
  of a non-GC-safe waiter.
- **What bounds the window:** the define path's listener also runs after the release (the
  `release < listener` source pin in `r21w14_finalizable_define_listener_tests`), so this matches the define. The bar
  is re-read live, and a compile that races the withdrawal is refused at publication by the logged
  `FinalizableSubtypeOf` record (`withdraws_in`). Only an already-compiled stale body, used in that window, escapes.
- **Wave 25, lane rawwrite: the order is left as it is, on purpose.** Moving the settle in front of `before_hooks`
  would keep the per-name `loading` flag raised while the settle runs. A second loader of the same name would then
  wait on that flag through the settle, counted and not GC-safe. The settle takes:
  - `jit_gate_pass.write()`;
  - the JIT cache's `mutation` lock (`invalidate_matching`);
  - the class-manager write in `note_finalizable_define`.

  That adds three wait edges, flag → `jit_gate_pass` / `mutation` / class manager, which the release does not have
  today. A thread that holds one of these locks and then loads the same name would deadlock. I found none, but the
  JIT cache's `mutation` holders could not be audited by reading alone. Doing only the `jit_gate_pass` drop early
  would not close the window either: an already-compiled stale body is reached through the cache, not through the
  gate. So the residual stays as this item describes it: one stale compiled constructor, used by a woken waiter in
  the gap between the flag clear and the settle. Closing it soundly needs the waiter to re-ask, for example a
  per-class "flip settling" epoch that the woken loader waits out GC-safe. That is a design change and stays FILE.
  The settling write lock of item 3 has no `before_hooks`: it settles right at its own release, so this question does
  not arise there.

## 5. The flip queue in `ClassManagerWriteGuard::drop` (Info: clean, with notes)

- **Re-entrancy:** none.
  - The take happens while the guard is held. The settle runs after `self.guard = None`.
  - `note_stub_upgrade_finalizer_flips` and `note_finalizable_define` take only the RAW `class_manager.read()` and
    `.write()`, whose release drains nothing.
  - `JitCache::write()` returns `&Self` (`jit/src/lib.rs:40803`).
  - `jit_gate_pass` is held only for each `retain`.
  - A stub upgrade inside the settle is impossible: it defines nothing.
- **Lock order:**
  - Under the write guard, `hook_vm_for_domain` takes the hook-registry read lock. `set_global_shared_vm_for_hooks`
    takes the class-manager read and then the registry write, so both use the order class manager, then registry.
    No path takes the registry and then the class manager.
  - After the release: `jit_gate_pass`, then the JIT cache, then (in the listener) the class manager, raw. A caller
    that holds `jit_gate_pass` or a JitCache-internal lock across `class_manager_write()` would deadlock here. The
    pre-existing `fire_jit_invalidate_hook` flush under the lock has the same exposure, and I found no such caller.
- **VM not registered:** `hook_vm_for_domain(..)?` comes before `take_stub_upgrade_finalizer_flips`, so the rows stay
  queued for the first release after `Vm::new` registers (`vm_init.rs:14169`). A `SharedVm::new` built without a
  `Vm`, as in tests, never settles. Its rows are bounded by the number of stub upgrades.
- **Thread that drops is not a Java thread** (a compiler worker, an agent thread): the settle needs no `JvmThread`, no
  safepoint and no JNI context.
- **Panics:**
  - The rows are taken before the release, so a panic inside the settle loses them. It also skips that release's
    JVMTI drain and resweeps. The drain's own panic already behaves this way.
  - A guard dropped while the thread is unwinding from a panic still runs the settle. A second panic there aborts.

  Neither needs a change for this wave.
- **Ownership:** dropping the `Arc<SharedVm>` from `hook_vm_for_domain` at the end of `drop` cannot be the last
  strong reference: the guard borrows the realm from a live `SharedVm`.

## 6. `note_stub_upgrade_finalizer_flips` and the gate-fill re-ask (Info: clean, with notes)

- **Completeness, became finalizable:** each class C that became finalizable (the upgraded class and each changed
  subclass) gets:
  - its own `<init>` rows dropped from `jit_gate_pass`;
  - its own constructor bodies withdrawn. `withdraws_for_finalizable_subtype` takes C itself:
    `own_ctor = key.declaring_class_id == class_id`, plus splices by name or by `copied_bytecode_of`;
  - its ancestors recorded and withdrawn by `note_finalizable_define(C)`.

  Allocation fast paths baked into OTHER methods' bodies are covered by the code-cache flush of
  `fire_jit_invalidate_hook`. Duplicate work for a subclass and its upgraded parent is idempotent:
  `mark_finalizable_ancestor` returns `false` the second time.
- **Completeness, stopped being finalizable:** the recipe goes and nothing else happens. Two FILE notes:
  - **(a)** a refusal row in `finalizer_bar_refusals` for the class's own constructors keeps them interpreted for the
    VM's life. A stub upgrade moves no redefine epoch, so `CRATONVM_FINALIZER_BAR_REFUSAL_EPOCH` does not expire it.
    This fails closed and costs perf only. Dropping the class's row in the reverse branch would fix it.
  - **(b)** the `became` filter reads the queued bit, not the live one. A class whose bit moved back before the settle
    is withdrawn needlessly, which also fails closed.
- **Gate-fill re-ask (`interpreter.rs:2693`):**
  - **Correct:** the bit is flipped under the write lock before the settle's `retain`. Either the `retain` removes the
    inserted row, or the fill's re-read sees the bar and removes it. The constructor compiled by the same `execute`
    call (its `finalizer_skip` was computed before the flip) is judged at publication by the logged record.
  - **Cost:** one `read_recursive` and one store lookup per `<init>` row FILL. It does not run per call: a filled row
    short-circuits the gate. For a class that is neither finalizable nor a recorded ancestor,
    `finalizer_bars_compiling_in` answers after one bool test. This is negligible.
  - The `|| finalizer_ancestor_recorded` half is now subsumed for the purpose of keeping the row, except for an
    ancestor whose constructor cannot throw. That case only removes a row needlessly, so it was left alone.
- **Switch scope (note):** the upgraded class's OWN listener is now gated by
  `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES` too: under `=0`, nothing is queued. That matches wave 22, so `=0`
  is faithful, but the switch's name and flag-group text speak only of subclasses. A one-line doc note on the
  `INVENTORY` row would say so.
- **Wave 25, lane rawwrite (item 3):** the settling write lock runs this same function, in the same order, right at
  its own release. So the completeness and gate-fill arguments above hold for it unchanged. What changes is the
  window in which a stale allowed row or a stale compiled body stays live after a raw-site upgrade. It shrinks from
  "until some later guarded release on any thread" to the gap between that site's release and the settle on the same
  thread. Notes (a) and (b) and the switch-scope note are untouched.
  `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS` reads no row itself: under
  `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES=0` nothing is queued, so it does nothing.

## 7. `extra_finalizers_outstanding` (Info: clean)

Every way a row leaves `finalizer_refs`, at HEAD:

- `remove_collected` (`reference.rs:3343`) subtracts the pruned rows' counts.
- `withdraw_finalizable` (`:4010`) subtracts the removed row's count. A withdrawn row parked in
  `withdrawn_finalizables` and re-registered by `note_finalizable_init_returned` comes back as a fresh row with
  count 0. The sum stays exact (the count is lost, as the w23 page item 7 notes).
- `retain_reference_rows` (the deferred-sweep and registered-before prunes) touches soft, weak, phantom,
  unbound and withdrawn rows, never `finalizer_refs`.
- `relocate_list` rewrites addresses and removes nothing.

No other `retain`, `remove`, `drain`, `clear` or reassignment of `finalizer_refs` exists outside tests. Raises happen
only in `note_finalizer_registered_again`, guarded by `u32::MAX`, and lowers only in `take_extra_finalizer`. Every
subtraction is `saturating_sub`. An undercount would lose a `finalize()`, and none is reachable. An overcount would
only restore wave 22's scan, and none is reachable either.

## 8. The door test cannot see the second run (Info, FILE)

- `gc_and_alloc.rs` `r21w23_a_counted_reinit_runs_finalize_twice_through_the_door` asserts two things:
  `pending_count() == 0` and the outstanding count (0 on, 1 off).
- If `enqueue_unfinalized` refused the re-queue (its `bool` is ignored at `gc_and_alloc.rs:14624`), both assertions
  would still hold: the count is taken before the re-queue, and the queue still ends empty. So the test pins the
  take, not the second `finalize()`.
- **Fix sketch:** either assert the `enqueue_unfinalized` result through a debug counter, or use a class whose
  `finalize()` has a side effect the test can count (two runs).

## 9. `r21-review-w21-findings` item 3 residual names removed identifiers (Info, FIX-NOW doc)

- The residual verdict in that page's item-status table (row 3) says that `mark_class_hidden` "records the
  `this_class` an ordinary renamed define kept (`renamed_class_file_names`)". The same row also names
  `hidden_class_file_names`.
- Wave 22 (w22 item 2) moved both into `ClassStore::recorded_file_names` (`classloading/src/class.rs:1607`).
  `mark_class_hidden` now reads `class_store.recorded_file_name(id)` (`class_manager.rs:14161`). The behaviour matches
  the verdict; only the names are stale.
- **Fix:** replace the names in that row and point to w22 item 2.

## 10. Otherwise (Info: clean)

- **Call-entry null-guard mark (`jit_bridge.rs:18750`):** `note_bytecode_copied(declaring_id)` now runs inside the
  `try_read_untracked` class-manager scope, which a redefinition's writer excludes, as the elided-constructor mark
  does. A later refusal (a breakpoint, an unresolved field, a layout miss) leaves a conservative mark: one needless
  withdrawal at a redefinition. Lock order: class manager (read), then the JIT cache's mark. It is the same as the
  elided-constructor site.
- **`jni_receiver_after_call`:**
  - **Global refs (odd handles, weak globals included):** a global ref's table entry and an indirect local's slot
    are both remapped by every moving collection. A weak global resolves through `jobject_to_obj`, which also applies
    the SATB keep-alive. The receiver is strongly held for the call's duration.
  - **Other handles:** a `jclass` handle is excluded. A raw local, or a foreign thread's local, answers `None`: no
    count, the pre-switch answer.
  - **Note (pre-existing, FILE if wanted):** on the equal-epoch arm, and on the throw arm, the epoch is tested before
    `ref_processor.lock()` is taken. That is the shape of the wave-10 withdrawing door. If a JNI thread is GC-safe at
    this point, a moving collection in between could relocate another object to `addr`. Testing the epoch under the
    processor lock would close it.
- **`=0` fidelity:**
  - `CRATONVM_JNI_REINIT_COUNT_BY_OUTCOME=0` takes the `_` arm, with the epoch test and `forget` on a failure. That is
    the wave-22 rule exactly.
  - `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES=0` queues nothing, so the settle never runs.
  - The outstanding count has no switch and changes no answer.
- **Globals:** none added. The queue lives on the `ClassManager`, the switch on `VmConfig`, and the count on the
  `ReferenceProcessor`. The test-only `set_global_shared_vm_for_hooks` uses the existing per-domain registry.
- **Allow-lists:** none added. The `java/lang/StackOverflowError` name test in item 3's rule is a type check, not a
  dispatch allow-list.
- **Other pages' FIXED verdicts:**
  - All 22 `CRATONVM_*` switches named in FIXED verdicts under `docs/known-issues/jit/` exist as flag literals in the
    code.
  - All 170 snake_case identifiers named in those verdicts exist in the code, except item 9's (stale names, matching
    behaviour).
  - Spot-checked in detail:
    - w21 item 6(a) is `gc.rs:671`;
    - w22 item 2's prefilter clear is in `unload_user_classes_inner`;
    - w23 item 1's own-constructor `retain` and fill re-read are present;
    - w23 item 4's two subtraction sites are present.

  No other mismatch was found.
