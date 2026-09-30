# JIT round 14 lane review6 proposals

Filed by round 14 wave 6 lane review6 while reviewing wave 5 (`74355aa4e`). Ranked.

## RV6-1. Method-monitor facts for `static synchronized` methods

**What.** SS8-3 records `Graph::method_monitor_param` only for an instance method. A
`static synchronized` method of class `C` runs holding `C`'s mirror on every entry (same
wrapped-entry and OSR argument), so inside its optimizing body `Thread.holdsLock(C.class)`
is `true` and a `static synchronized` splice of another `C` method (window
`static_class_id == C`) is a recursive pair that can be deleted.

**Benefit.** `static synchronized` helper chains (`Collections`-style counters, legacy
singletons, `SecureRandom` seeders) lose one CAS pair per nested call; the holdsLock fold
covers the SS8-5 shape from the other side.

**Cost / risk.** Small: a `Graph::method_monitor_class: Option<u32>` set in `lib.rs`
`ir_tier_attempt` beside the instance fact, matched in `try_fold_holds_lock` against a
`ConstClass` of that class id and in `ir_optimize::elide_region_nested_sync_windows`
against `w.static_class_id`. Risk: the `ConstClass` identity must be the defining loader's
mirror (the same test SS-2's static mirror already makes).

**First step.** Add the field and the elision arm behind
`CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS` (the existing switch), with a unit test
mirroring `a_window_on_the_synchronized_methods_receiver_is_deleted`.

## Round 14 wave 7 (lane sync7): RV6-1 landed

Default ON behind its own `CRATONVM_JIT_IR_SYNC_STATIC_METHOD_MONITOR_FACTS` (and off with the
existing `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS`). `Graph::method_monitor_class:
Option<u32>` is set by `lib.rs` `ir_tier_attempt` to the declaring class id of an
`ACC_SYNCHRONIZED | ACC_STATIC` method. `ir_optimize::elide_region_nested_sync_windows`
deletes a window whose `static_class_id` is that class (census
`window-elided-in-static-method`). For the `holdsLock` half the `ConstClass` identity is
proven per constant-pool entry, not by name: `lib.rs` collects the method's own `ldc` entries
whose `cp_ldc_resolver` holder is the class and whose `cp_new_resolver` answer (the no-loading
resolution) is `Resolved { class_id }` of that class
(`IrBuilder::set_method_monitor_class_cps`); `try_fold_holds_lock` folds a `holdsLock` whose
argument is a `ConstClass` of one of those entries (census `holdslock-folded-in-static-method`).
The `ConstClass` node stays (its resolution may still throw). Not done: the finished-graph fold
(`fold_method_monitor_holds_lock`) does not look through φs of `ConstClass` nodes (a class
literal kept in a local across a loop); rare. Tests: `ir_optimize.rs`
`a_static_window_on_the_static_synchronized_methods_class_is_deleted`, `ir.rs`
`r14w7_sync7_builder_tests::holds_lock_of_the_static_methods_own_class_folds`.

## RV6-2. A lock-free refusal of implicit NPEs before the native-leaf screen

**What.** `append_native_standin_frames` takes the class-manager read lock for every
capture standing at an `invokevirtual` (D2 of
`r14w6-review6-wave5-review-findings-FIXED-20260929.md`), including the VM's own implicit
receiver NPE, which `native_leaf_frame` always refuses. Pass the capture's "implicit
exception at this bci" bit (the VM raising it knows) and skip the screen for it.

**Benefit.** NPE-heavy code (null-check-by-exception idioms) stops paying a lock and an
instruction decode per throw. **Cost.** One parameter through the capture path.
**Risk.** None for correctness (the screen already answers `None` there).
**First step.** Count captures that reach `native_leaf_frame` and return `None` for a
non-static NPE (a `CRATONVM_DBG_` census) on the Tomcat fixture.

## Round 14 wave 7 (lane trace6): RV6-2 landed

With a correction: the class-manager lock is not what is saved. When the throwable's constructor
ran as Java, `capture_throwable_stack_trace`'s fill-frame trim already took it
(`throwable_capture_is_a`, asked for its own `fillInStackTrace` / `<init>` frames) and holds the
guard to the end of the capture, so `append_native_standin_frames` reuses it; when a
`native_exc_init_*` served the constructor, the screen still needs the throwable's class name, i.e.
the class store. A truly lock-free refusal needs the VM's NPE class id at hand (a per-VM field, not
this lane's file). What the screen saves is the census's and the leaf rule's decode of the call (two
`find_method` + decode + table scans). Landed as
`stackwalker::receiver_npe_gets_no_standin_frames` (NPE, interpreted innermost frame at
`invokevirtual` / `invokespecial` / `invokeinterface`), checked right after the throwable class is
read; exact (both rules answer `None` there). Compiled innermost frames are not screened (their
code is not at hand lock-free). Switch `CRATONVM_THROWABLE_RECEIVER_NPE_SCREEN`.

## RV6-3. Native leaf frames: judge finality on the Methodref's owner

**What.** D3: an `invokevirtual` whose owner is a final class but whose native is
inherited (`FinalClass.hashCode()` -> `Object.hashCode`) is entered exactly, yet refused.
Also accept when the RESOLVED owner class is final. **Benefit.** One more HotSpot-exact
frame class; tiny. **Risk.** None beyond the existing rule. **First step.** A unit test
beside the `native_leaf_frame` tests with a final subclass inheriting a native.
