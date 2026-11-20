#[allow(unused_imports)]
use super::*;

/// Execute a method on the given class.
///
/// This is called by `invoke_on_class_shared` for non-native methods.
pub fn execute(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    method_descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Interpretation);
    // Interpreter round i3 wave 81, lane L3: the reflective door census's
    // `call` split (`CRATONVM_REFLECT_DOOR_PHASE_STATS`, a measurement
    // instrument, default off: one registry field load per `execute`).
    crate::vm::reflect_door_gate_stamp(
        shared,
        cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteEntry,
    );
    if crate::runtime::env_cache::exec_frame_trace()
        && method_name == "aotContributedInitializerStartsManagementContext"
    {
        let (cname, loader) = {
            let cm = shared.classes.class_manager.read();
            (
                cm.get_class(class_id).map(|c| c.name.to_string()),
                cm.get_loader_id(class_id),
            )
        };
        eprintln!(
            "[EXEC-FRAME-TRACE] method={} class_id={:?} class_name={:?} loader={:?}",
            method_name, class_id, cname, loader
        );
    }
    // S-bytebuddy r1 — Rust-side recursion guard.
    //
    // ByteBuddy's `JavaDispatcher.run()` performs deep reflection via
    // `Method.invoke`, whose bytecode dispatches back into this `execute`
    // function. The Java-level frame counter (`thread.frames`) is checked at
    // each push (see `max_stack_depth` guard below), BUT a `Method.invoke`
    // chain that re-enters `execute` re-invokes this Rust function on the
    // native call stack BEFORE the Java frame is pushed. That recursion is
    // not bounded by `max_stack_depth`. Result: under a deep ByteBuddy
    // reflection cascade, the Rust call stack blows past the OS guard page
    // (even at the 64 MB main-vm setting) and the process aborts with the
    // Rust stack-overflow handler (rc=127, SIGABRT) — bypassing any Java
    // try/catch.
    //
    // Throw a Java `StackOverflowError` BEFORE we recurse further. JVMS lets
    // the implementation raise SOE at any depth; ByteBuddy's reflection
    // helpers catch `Throwable` and recover.
    //
    // Limit calibration (H6): the ceiling is no longer a single hard-coded
    // constant. It is derived per-thread from the thread's ACTUAL native
    // stack size (see `EXEC_DEPTH_CEILING` / `derive_exec_depth_ceiling` /
    // `init_thread_exec_depth_ceiling` above). The previous fixed `10_000`
    // assumed a 64 MiB stack and overflowed the 8 MiB worker-carrier stack —
    // a hard, uncatchable process abort — before it ever tripped. Each
    // `execute` level burns several KiB of native stack (recursive
    // `execute_frame` + invoke dispatch + JIT entry trampoline); the derived
    // ceiling reserves head-room so the guard fires (throwing a catchable
    // `StackOverflowError`) before the OS guard page does.
    //
    // Wave 133 (lane L4): and the stack pointer itself, which the count only
    // estimates (`reentry_native_stack_exhausted`, default ON).
    //
    // Wave 134 (lane L4, review of w133 item 4): nothing is pushed and no
    // monitor is taken yet (the synchronized entry is below), so the trip
    // returns the same lazy error as the `EXEC_DEPTH` trip next to it, which
    // the caller's dispatch loop converts with this guard armed: no Java runs
    // below the floor, so the margin under it is never relied on.
    // `CRATONVM_EXEC_NATIVE_STACK_GUARD_LAZY_ERROR=0` builds it in place
    // (wave 133).
    if reentry_native_stack_exhausted() {
        // t1 wave 11, lane INTA: no frame is pushed, so a constructor's
        // receiver loses its allocation-time finalizer registration here too.
        withdraw_ctor_receiver_at_entry_trip(shared, thread, method_name, args);
        dump_stack_on_soe(thread);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }
    // `EXEC_DEPTH` is a module-level thread-local since the tier-1 round
    // (wave 2, lane K), so a carrier slice can set the carrier's count aside
    // (`replace_exec_depth`).
    struct DepthGuard;
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            EXEC_DEPTH.with(|d| {
                let v = d.get();
                d.set(v.saturating_sub(1));
            });
        }
    }
    let depth = EXEC_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    // H6: trip against the per-thread ceiling derived from the thread's
    // ACTUAL native stack size (see `EXEC_DEPTH_CEILING` /
    // `init_thread_exec_depth_ceiling`). The previous hard-coded `10_000`
    // was calibrated for a 64 MiB stack and overflowed the 8 MiB worker
    // carriers' native stack — a hard, uncatchable process abort — before
    // it tripped. Throwing here yields a *catchable* `StackOverflowError`.
    let ceiling = EXEC_DEPTH_CEILING.with(|c| c.get());
    if depth > ceiling {
        EXEC_DEPTH.with(|d| {
            let v = d.get();
            d.set(v.saturating_sub(1));
        });
        // The cap's own instrument, wired back. `note_bb_dispatcher_cap_hit`
        // was written for the r2 hard cap this H6 per-thread ceiling replaced,
        // and its doc comment still said "used by the S-bytebuddy r2 hard cap
        // in `interpreter::execute`" while nothing called it — so the counter
        // read zero on every run that has ever existed, and a zero there is
        // indistinguishable from "the guard never fired". It matters which:
        // this arm converts an uncatchable native stack overflow into a
        // catchable `StackOverflowError`, and a workload hitting it steadily is
        // a workload whose ceiling is calibrated wrong.
        crate::dispatch_trace::note_bb_dispatcher_cap_hit(depth);
        withdraw_ctor_receiver_at_entry_trip(shared, thread, method_name, args);
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }
    let _exec_depth_guard = DepthGuard;
    // Interpreter round i4 wave 91, lane L4: the finder census's Java
    // re-entry split (`CRATONVM_DBG=mh-finder-census`; one flag read
    // otherwise). Dropped at this function's return.
    let census_reentry =
        cratonvm_native_builtins::mh_finder_census::execute_clock(method_name, method_descriptor);
    // Interpreter round i2 wave 53, lane L6: Java a native runs is not "in
    // native" for a JDWP suspension (one load of a thread-owned field).
    let _nested_java_ack = NestedJavaAck::enter(thread);

    // GC-safety: `args` belongs to the caller and is not a GC root, but this
    // function can reach a safepoint long before its last read of it — a
    // lambda/proxy rescue in the no-`Code` arm, class loading during the
    // eager first-call compile, the compiled-entry `<clinit>` walk, and a
    // compiled-body run that deopts back here. A reference argument that
    // moved in between would be read back as its from-space address (the
    // moving young generation is on by default). Pin the object arguments for
    // the whole call and read them back through `live_args!()` at every use
    // after a possible safepoint; the pins are dropped (truncated to this
    // watermark) on every return path, including after the frame is built —
    // from then on the frame's locals are the scanned copy.
    // See docs/feature-designs/real-jdk-mode.md.
    let mut args_buf: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::from_slice(args);
    // `mut`: the first-call door's weak-argument swap (gch h3/fd, d9d P2)
    // shrinks its count when an argument was collected.
    let mut args_root_guard = field_access::InvokeArgsRootGuard::new(thread, &args_buf);
    macro_rules! live_args {
        () => {{
            args_root_guard.refresh(&mut args_buf);
            &args_buf[..]
        }};
    }

    // The entry diagnostics (`CRATONVM_DBG_LETSGO` ring, the `intValue` and
    // `AnnotationScopeMetadataResolver.<init>` traces) live in
    // `execute_entry.rs` (a pure move).
    execute_entry::execute_entry_trace_hooks(
        shared,
        thread,
        class_id,
        method_name,
        method_descriptor,
        args,
    );
    // Find the method
    //
    // S112r9 — when the resolved method has no Code attribute (abstract or
    // interface declaration), throw a real Java `AbstractMethodError` instead
    // of returning an opaque `VmError::Internal`. Java callers (e.g. Spring's
    // `try { ... } catch (Throwable t) { handleRunFailure(...); throw new
    // IllegalStateException(t); }`) can then catch and rewrap. Previously
    // this produced a Rust-side `MethodCallFailed::InternalError` that
    // propagated past Java try/catch handlers and was either converted to a
    // CLI `bail!()` (rc=1, JIT off) or — when it crossed a JIT dispatch
    // boundary — silently dropped (rc=0, JIT on). Either way Spring Boot
    // never reached `printBanner`. Throwing AbstractMethodError lets
    // SpringApplication.run() catch the failure, log it through its own
    // failure path, and at least produce a partial banner / startup-failure
    // banner before exiting.
    //
    // `CRATONVM_EXECUTE_SHARED_METHOD_VIEW` (interpreter round i3 wave 77,
    // lane L4, default off; `ExecuteMethodView`): the class name, method name
    // and descriptor the skip key and the frame carry are the class's and the
    // method's own shared `Arc<str>`s (refcount bumps) and the source file is
    // copied into its `Arc` once. Off: the copies this block always made (a
    // class-name copy, the source file copied here and again into the frame's
    // `Arc`, fresh name `Arc`s for the skip key and the frame). In both
    // states the exception table is copied once, straight into the `Arc` the
    // frame takes (`execute_exception_table`; it was a `Vec` clone plus a
    // second copy), so no state allocates more than before.
    // The record saves JIT gate/body lookups. Without a JIT its validation
    // and map lookup cost more than the metadata copies they replace (j27
    // host A/B), so interpreter-only VMs retain their ordinary entry.
    let cached_entry = shared.config.jit_enabled;
    let shared_view = true;
    // `CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME` (interpreter round i6 wave 125,
    // lane L6, default off, page `i125-L6`): the cached frame below on a
    // virtual thread only, whose OSR offer refuses an owned frame. Off: the
    // global switch alone, as before.
    let cached_reentry_frame = shared.config.execute_cached_reentry_frame
        || (matches!(thread.kind, crate::threading::ThreadKind::Virtual));
    let (
        code_attr,
        source_file,
        class_name_str,
        view,
        is_synchronized,
        is_static,
        reentry_key,
        entry_record,
    ) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id).ok_or_else(|| VmError::Internal {
            message: format!("class {class_id} not found"),
        })?;
        // `CRATONVM_NATIVE_OWN_BODY_HINT` (interpreter round i3 wave 76, lane
        // L4): a native re-entering its own JDK body hands over the method's
        // declared index (`set_execute_method_hint`); it is used only when it
        // still names this `(name, descriptor)` in this class, which then is
        // the method `find_method` would return (one per name and
        // descriptor). Off: one per-VM bool load.
        let hinted = if shared.config.native_own_body_hint {
            take_execute_method_hint(class_id)
                .and_then(|i| class.methods.get(i as usize))
                .filter(|m| &*m.name == method_name && &*m.descriptor == method_descriptor)
        } else {
            None
        };
        // `CRATONVM_EXECUTE_METHOD_INDEX_MEMO` (interpreter round i6 wave
        // 115, lane L7, default off; `JitRealm::execute_method_index`): the
        // method's declared index from the per-VM memo, used only when it
        // still names this `(name, descriptor)` in this class (so a stale or
        // colliding row is a miss, never a wrong method). A miss scans, as
        // before, and files the index. Off: one per-VM bool load.
        let entry_slot = cached_entry
            .then(|| execute_method_index_key(class_id, method_name, method_descriptor));
        let entry_record = if cached_entry {
            entry_slot.and_then(|slot| shared.jit.execute_entry_records.read().get(&slot).cloned())
        } else {
            None
        };
        let entry_method = entry_record
            .as_ref()
            .and_then(|entry| class.methods.get(entry.index))
            .filter(|m| &*m.name == method_name && &*m.descriptor == method_descriptor);
        let method = match entry_method.or(hinted) {
            Some(m) => m,
            None => (if shared.config.execute_method_index_memo {
                find_declared_method_memoized(
                    shared,
                    class_id,
                    class,
                    method_name,
                    method_descriptor,
                )
            } else {
                class.find_method(method_name, method_descriptor)
            })
            .ok_or_else(|| {
                VmError::Linkage(LinkageError::NoSuchMethodError {
                    class_name: class.name.to_string(),
                    method_name: method_name.to_string(),
                    method_descriptor: method_descriptor.to_string(),
                })
            })?,
        };
        let entry_record = if cached_entry && method.code().is_some() {
            if let Some(entry) =
                entry_record.filter(|e| e.matches(method, class.source_file.as_deref()))
            {
                Some(entry)
            } else {
                let entry = class
                    .methods
                    .iter()
                    .position(|m| std::ptr::eq(m, method))
                    .and_then(|index| {
                        entry_record::ExecuteEntryRecord::new(
                            index,
                            method,
                            class.source_file.as_deref(),
                        )
                    })
                    .map(Arc::new);
                if let (Some(entry), Some(slot)) = (&entry, entry_slot) {
                    shared
                        .jit
                        .execute_entry_records
                        .write()
                        .insert(slot, entry.clone());
                }
                entry
            }
        } else {
            None
        };
        let method_code = method.code();
        let has_code = method_code.is_some();
        let is_synchronized = method.is_synchronized();
        let is_static = method.is_static();
        let method_is_native = method.is_native();
        // Off: a fresh copy, as the owned `String` this used to be.
        let class_name_view: Arc<str> = if shared_view {
            Arc::clone(&class.name)
        } else {
            Arc::from(&*class.name)
        };
        let view = ExecuteMethodView {
            shared: shared_view,
            names: shared_view.then(|| (Arc::clone(&method.name), Arc::clone(&method.descriptor))),
        };
        // `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` (interpreter round i3 wave
        // 79, lane L7, default off): the method's declared index, its own
        // names and the pool generation, read under this guard, for the
        // per-VM record the frame is pushed from (`execute_reentry_record`).
        // Off: one per-VM bool load.
        let reentry_key = if cached_reentry_frame && has_code {
            class
                .methods
                .iter()
                .position(|m| std::ptr::eq(m, method))
                .and_then(|i| u32::try_from(i).ok())
                .map(|index| ExecuteReentryKey {
                    index,
                    name: Arc::clone(&method.name),
                    descriptor: Arc::clone(&method.descriptor),
                    pool_generation: cratonvm_classloading::class_redefinition_count(),
                })
        } else {
            None
        };
        // Only the bytecode (an `Arc` view), the exception table and the two
        // sizes are read below. The Code attribute's own `attributes` (the
        // line and local-variable tables, the stack map) were deep-copied on
        // every call for nothing: ~7% of a native's up-call into a small
        // Java method (`hashCode` from a `--compatible` map native; the
        // osrdoor user-key page, round 16 wave 3 profile). JIT round 16, hcb.
        let code_attr_opt = if let Some(entry) = &entry_record {
            Some(entry.code.clone())
        } else {
            method_code.map(|c| ExecuteCode {
                max_stack: c.max_stack,
                max_locals: c.max_locals,
                code: c.code.clone(),
                // `CRATONVM_EXECUTE_FEWER_COPIES` (wave 83, lane L3, default
                // off): an empty table is the shared empty `Arc` (`Arc::default`,
                // no allocation since Rust 1.80), not a fresh header allocated
                // here and freed at the return. Off: the copy, as before.
                exception_table: if (shared.config.execute_fewer_copies
                    || true)
                    && c.exception_table.is_empty()
                {
                    Arc::default()
                } else {
                    execute_exception_table(&c.exception_table)
                },
            })
        };
        // `CRATONVM_EXECUTE_LEAN_REENTRY` (interpreter round i5 wave 114, lane
        // L7, default off): the source file is copied only by a reader that
        // needs it (`ExecuteSourceFile::get`): the interpreted frame, the
        // precise handler frame, the eager compile. A compiled body's return,
        // the common native re-entry, copies nothing. Off: copied here, as
        // before.
        let source_file = if let Some(entry) = &entry_record {
            ExecuteSourceFile::Copied(entry.source_file.clone())
        } else {
            ExecuteSourceFile::Deferred {
                class_id,
                present: class.source_file.is_some(),
            }
        };
        drop(cm);
        if !has_code {
            // The no-`Code` arm — the resolved-class native, the lambda and
            // annotation proxy receivers, the receiver-hierarchy rescue,
            // Path A / Path B, then `AbstractMethodError` (or, for an
            // `ACC_NATIVE` method, `UnsatisfiedLinkError`) — lives in
            // `execute_entry.rs`. It always answers the call.
            return execute_entry::execute_no_code_rescue(
                shared,
                thread,
                class_id,
                method_name,
                method_descriptor,
                args,
                class_name_view.to_string(),
                execute_entry::NoCodeFlags {
                    is_native: method_is_native,
                    is_static,
                    is_synchronized,
                },
                &mut args_buf,
                &args_root_guard,
            );
        }
        // `has_code` is `true` here (the `!has_code` arm above always
        // returns), so `code_attr_opt` is `Some`. Route the impossible
        // `None` to a typed `VmError::Internal` instead of `.expect()` so
        // this hot dispatch file stays panic-free (NEW-7 / B3 gate).
        let code_attr = match code_attr_opt {
            Some(c) => c,
            None => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "has_code true implies code present".to_string(),
                }));
            }
        };
        (
            code_attr,
            source_file,
            class_name_view,
            view,
            is_synchronized,
            is_static,
            reentry_key,
            entry_record,
        )
    };
    // Interpreter round i4 wave 93, lane L7: the JIT state of a callee a
    // census scope re-entered (`reentry-callee.<scope>`;
    // `CRATONVM_DBG=mh-finder-census`, one flag read otherwise).
    // Wave 114, lane L7: not charged to the re-entry's prologue row
    // (`ExecuteClock::excluding`).
    if let Some(clock) = census_reentry.as_ref() {
        if cratonvm_native_builtins::mh_finder_census::in_reentry_now() {
            clock.excluding(|| {
                jit_bridge::census_note_reentry_callee_state(
                    shared,
                    class_id,
                    &class_name_str,
                    method_name,
                    method_descriptor,
                )
            });
        }
    }

    // ACC_SYNCHRONIZED (JVMS §2.11.10). `execute` used to read the flag and use
    // it only for the deopt refusal, so a caller that entered bytecode through
    // it directly — `invoke_virtual_bytecode_only`,
    // `invoke_special_bytecode_only_shared`, the boot helpers — ran a
    // synchronized body without its monitor, and `wait`/`notify` on it threw
    // `IllegalMonitorStateException`. Only `invoke_on_class_shared_inner`
    // wrapped the call in a guard. Take the monitor here: the guard below
    // holds it across the compiled-entry run (a compiled body has no monitor
    // prologue — its caller supplies it) and every early return, and hands it
    // to the interpreted frame's `monitor_on_exit` when one is pushed, so
    // `pop_and_recycle_frame_with_reason` releases it however the frame
    // leaves (and a frozen continuation keeps it). A caller that already
    // holds it re-enters, which a Java monitor allows; the counts balance.
    // `invoke_on_class_shared_inner` no longer takes it before calling here,
    // so that route enters the monitor once. `CRATONVM_NO_EXECUTE_SYNC=1`
    // restores the old behaviour (see `execute_sync_disabled`).
    //
    // `<clinit>`: JVMS §4.6 ignores every flag but `ACC_STATIC` on a class
    // initializer, and locking the mirror there would nest a monitor inside
    // the initialization-lock protocol.
    //
    // Interpreter round i2 wave 56, lane L6 (stage 2 of
    // `CRATONVM_SYNC_METHOD_FRAME_FIRST`): a CONTENDED entry is deferred to
    // the frame this function pushes below (`SyncEntry::Deferred`), so a
    // thread blocked entering a synchronized `run()`, a reflective call or a
    // compiled caller's by-name call lists the method on top, as HotSpot's
    // `lock_method` does. Such a call skips the first-call compile below (a
    // compiled body has no frame to block in; `deferred_sync_entry` joins
    // `env_disable_jit`, which never seals) and enters the monitor right
    // after the push. The uncontended entry is the guard as before.
    let mut deferred_sync_entry = false;
    let mut sync_monitor: Option<crate::vm::vm_exec::SynchronizedMethodGuard> = if is_synchronized
        && method_name != "<clinit>"
        && !execute_sync_disabled()
    {
        let obj = if is_static {
            // The class mirror, the same object `synchronized (X.class)`
            // locks. Getting it can allocate; the arguments are pinned
            // (`args_root_guard`), so on a full heap its first use
            // collects and throws `OutOfMemoryError` (gen r5w1/oom5).
            constants::class_mirror_or_oom(shared, thread, class_id, "static-synchronized-monitor")?
        } else {
            match live_args!().first() {
                Some(Value::Object(Some(receiver))) => *receiver,
                // A null `this` is the `NullPointerException` HotSpot's
                // monitor enter raises, not a VM-internal failure (which
                // no Java `catch` can see).
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into());
                }
                _ => {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: "synchronized instance method called with null or \
                                      missing this"
                            .to_string(),
                    }));
                }
            }
        };
        // Blocks on contention; pins and remaps `args_buf` across the wait.
        // Refreshed first, so a collection anywhere since the last read
        // (the mirror fetch allocates) cannot hand it a from-space address.
        args_root_guard.refresh(&mut args_buf);
        match crate::vm::vm_exec::monitor_enter_synchronized_method_or_defer(
            shared,
            thread,
            obj,
            &mut args_buf,
        ) {
            crate::vm::vm_exec::SyncEntry::Held(held) => Some(
                crate::vm::vm_exec::SynchronizedMethodGuard::adopt(shared, thread, held),
            ),
            crate::vm::vm_exec::SyncEntry::Deferred(_) => {
                deferred_sync_entry = true;
                None
            }
        }
    } else {
        None
    };

    // If the JIT early-compile path encounters an exception from a callee
    // dispatch, we stash it here and fall through to the interpreter, which
    // pushes a frame and routes through the exception table.
    let mut jit_early_exception: Option<ObjectRef> = None;
    // The bci the compiled body stamped at the throw site that produced
    // `jit_early_exception`, captured at the drain because later work clears the
    // signal. `-1` means "not stamped"; see the routing site below for why this
    // sink cannot afford to route without it.
    let mut jit_early_throw_bci: i64 = -1;
    // gch h3/fd (d9d P2): the compiled run held this call's arguments weakly
    // (`jit_bridge::WeakExecuteArgs`) and one was collected; a re-run from
    // entry is then refused (the entry state no longer exists).
    let mut first_call_weak_arg_collected = false;

    // Interpreter round i3 wave 83, lane L3: the door census's `call_exec`
    // cut (prologue / gate block); one registry field load when off.
    crate::vm::reflect_door_gate_stamp(
        shared,
        cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteGate,
    );

    // Try JIT compilation for this method.
    //
    // Interpreter round i3 wave 76, lane L4
    // (`CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT`, default off): a VM without
    // a JIT skips the whole block. Its only outputs are a compiled entry, a
    // `jit_skip_set` seal, a gate-pass memo fill and a tier-up offer, none of
    // which a VM that never compiles reads; `env_disable_jit` below already
    // sends such a VM down the no-compile arm, but only after the skip key's
    // three `Arc<str>` allocations and two read locks per call. Two cached
    // per-VM bools, so the default path pays two loads.
    if !(shared.config.execute_no_jit_gate_without_jit && !shared.config.jit_enabled) {
        // Three fresh `Arc<str>`s, or (`CRATONVM_EXECUTE_SHARED_METHOD_VIEW`)
        // three refcount bumps of the class's and the method's own names.
        let skip_key: (Arc<str>, Arc<str>, Arc<str>) = (
            view.class_name(&class_name_str),
            view.method_name(method_name),
            view.descriptor(method_descriptor),
        );
        /// Announce a permanent `jit_skip_set` seal under `CRATONVM_DBG_JITC`.
        ///
        /// Sealing is STRICTLY STRONGER than a compile bail: a bail is retried
        /// (up to `MAX_TIER_FAIL_RETRIES`) and is reported by the tier
        /// manager, whereas a seal removes the method from every later path —
        /// including the invocation-counter upgrade that reaches
        /// `jit::try_compile`'s relaxed, dataflow-based gates. A sealed method
        /// is not merely uncompiled, it is UNTRACKED: it appears nowhere in
        /// `CRATONVM_DBG=jit-method-stats`, not even as hot-but-stuck, so
        /// "this method is never even considered" had no observable signal at
        /// all before this line.
        fn note_jit_skip_seal(site: &str, key: &(Arc<str>, Arc<str>, Arc<str>)) {
            if None::<std::ffi::OsString>.is_some() {
                eprintln!(
                    "[cratonvm-jitc] jit-skip-seal site={site} {}.{}{}",
                    key.0, key.1, key.2
                );
            }
        }
        let already_skipped = if let Some(entry) = &entry_record {
            entry.skipped(shared, &skip_key)
        } else {
            shared.jit.jit_skip_set.read().contains(&skip_key)
        };
        // PERF FIX (2026-07-15, companion to the invoke-cache + jit-bail-list
        // memoization fixes): `already_skipped` (a single `jit_skip_set`
        // RwLock-read + hash lookup) was computed first but NOT used to
        // short-circuit the expensive eligibility computation below — every
        // call to `execute()` for an already-skip-listed method still paid
        // the `class_manager` read lock, `should_skip_jit_with_init`'s
        // policy walk, the ForkJoinTask ancestor-chain check
        // (`is_fjp_subclass_blocklisted`), a `native_methods.find` hash
        // lookup, AND — worst case — `jit_method_calls_native_shadowed`'s
        // full O(method-bytecode-size) decode-and-scan, only to have the
        // combined `if` condition below discard every one of those results
        // in favor of the already-known `already_skipped == true` verdict.
        // Once a method is skip-listed it can never leave the list within
        // this process (mirrors the `mark_jit_bail_listed` invariant this
        // same session's other fix relies on), so none of this is needed
        // when `already_skipped` is true — skip straight to cheap defaults.
        // The POSITIVE half of the same short-circuit. `already_skipped` covers
        // methods that FAIL this gate; a method that PASSES was recorded
        // nowhere, so it re-ran the whole computation on every `execute()`
        // entry — forever, and *before* the `JitCache` consult in the `else`
        // branch below, so a fully compiled hot method paid it too. The
        // expensive term is `jit_method_calls_native_shadowed`: an
        // O(method-bytecode) decode with a three-string-hash `slot_for_exact`
        // probe per invoke instruction in the body. Measured on netty
        // `AdaptiveByteBufAllocatorTest`, that scan reached 2.15% of CPU
        // through `slot_for_exact` alone while only 637 methods were ever
        // sealed for the reason it computes — it was re-running, not running
        // once per method.
        //
        // Stamped with this VM's `JitCache::redefine_epoch()` because a stale PASS is unsafe in a
        // way a stale seal is not: see `JitRealm::jit_gate_pass`.
        // Keyed on `ClassId`, not on the class name the negative set uses — see
        // `JitRealm::jit_gate_pass` for why the name is safe there and unsafe
        // here. The two `Arc` clones are refcount bumps, not allocations.
        //
        // `CRATONVM_EXECUTE_FEWER_COPIES` (interpreter round i3 wave 83, lane
        // L3, default off; `VmConfig::execute_fewer_copies`): the key is built
        // once, whole, and the lookup borrows it. Off: the lookup builds a
        // second key from two more clones (four refcount operations per call,
        // two increments here and two decrements at its drop), as before.
        // `CRATONVM_EXECUTE_LEAN_REENTRY` (wave 114, L7) implies this lookup.
        let fewer_copies = shared.config.execute_fewer_copies
            || true
            || cached_entry;
        let gate_pass_key = (class_id, skip_key.1.clone(), skip_key.2.clone());
        let gate_pass_memo = if already_skipped
        {
            None
        } else {
            let epoch = shared.jit.jit_cache.redefine_epoch();
            let entry_gate = entry_record.as_ref().and_then(|entry| entry.gate(epoch));
            if entry_gate.is_some() {
                entry_gate
            } else {
                let memo = shared.jit.jit_gate_pass.read();
                let hit = if fewer_copies {
                    memo.get(&gate_pass_key).copied()
                } else {
                    memo.get(&(class_id, gate_pass_key.1.clone(), gate_pass_key.2.clone()))
                        .copied()
                };
                drop(memo);
                let hit = hit.and_then(|(e, iface)| (e == epoch).then_some(iface));
                if let (Some(entry), Some(iface)) = (&entry_record, hit) {
                    entry.note_gate(epoch, iface);
                }
                hit
            }
        };
        // JIT round 21 wave 11 (lane finalize): a constructor that can throw
        // before its `super(..)` and constructs a finalizable object stays
        // interpreted (`exception_dispatch::finalizer_bars_compiling`). Since
        // wave 13 (review rev-jit w13 item 3, rev-native
        // `r21-finalize-bar-recomputed-on-every-interpreted-ctor-entry`) the
        // answer is memoized both ways: a refusal per class id
        // (`finalizer_bars_compiling_memoized`, `JitRealm::finalizer_bar_refusals`;
        // wave 14 moved it out of the name-keyed `jit_skip_set`, item 3 of
        // `r21-review-w14-findings-20261019.md`; it only ever flips from
        // allowed to refused, at the define of a finalizable subclass), and
        // an allowed answer rides in the `jit_gate_pass` memo, whose entries
        // for the constructors of the ancestors such a define reports the
        // define listener drops (`SharedVm::note_finalizable_define`).
        let finalizer_skip = !already_skipped
            && gate_pass_memo.is_none()
            && finalizer_bars_compiling_memoized(shared, class_id, method_name, method_descriptor);
        let (is_interface_default, static_skip_reason, fjp_skip, native_skip) = if already_skipped {
            (false, None, false, false)
        } else if let Some(is_interface_default) = gate_pass_memo {
            // Recorded eligible under the current redefine epoch: all three
            // skip reasons were false when it was recorded, and each is a pure
            // function of this method's static bytecode and metadata.
            cratonvm_jit::note_jit_gate_pass_hit();
            (is_interface_default, None, false, false)
        } else {
            // Static eligibility check — see vm/src/jit/skip_list.rs for the full
            // policy mapping (each entry is documented against a roadmap item in
            // docs/feature-designs/real-jdk-mode.md Phase A1).
            let is_interface_default = {
                let cm = shared.classes.class_manager.read();
                cm.get_class(class_id).map_or(false, |c| c.is_interface())
            };
            // The static JIT ban list was deleted 2026-07-31 (see
            // docs/feature-designs/jit-ir-optimizations.md).
            // Nothing is statically skipped now; `CRATONVM_JIT_DENY` is the single
            // remaining force-interpret lever, applied in `jit::try_compile`.
            let policy = ();
            // T1.1.f — classify init complexity so trivial `<init>`/`<clinit>`
            // methods (just `aload_0; invokespecial; return`) become
            // JIT-eligible. The classifier walks the bytecode and returns
            // `Trivial` iff there are no field stores, no synchronization,
            // and no invokedynamic. For any other method name, the
            // classifier result is `Unknown` (the classifier is only
            // consulted for `<init>`/`<clinit>`).
            let _ = policy;
            let static_skip_reason: Option<()> = None;
            // RFJP.1 (RETIRED, lever-only) — see `is_fjp_subclass_blocklisted`.
            // Inert unless `CRATONVM_JIT_FJP_SUBCLASS_BLOCKLIST=1`.
            let fjp_skip = is_fjp_subclass_blocklisted(shared, &class_name_str, Some(class_id));
            // S111r15 - refuse to JIT a method directly backed by a Rust native at
            // this FIRST-CALL compile path too. Without this, `Character.toLowerCase(C)C`
            // bypassed the native override and corrupted Spring property-name parsing.
            //
            // bytebuddy_probe / ANTLR cold-path follow-up: do not reject every
            // bytecode override merely because an ancestor has an identity native
            // (`Object.equals`/`hashCode`/`toString`). A real override shadows that
            // native and can compile. Keep the ByteBuddy safety case by rejecting
            // methods whose own bytecode contains an invoke that resolves to a
            // native-shadowed target such as `Object.equals`.
            //
            // Real-JDK §7: a compile-eligibility probe, not a resolution — no
            // dispatch happens here and no invocation is counted. Every kind
            // must keep suppressing the compile: an `Intrinsic` legitimately
            // supersedes bytecode (§1.4) so compiling the bytecode would be
            // wrong, a `Bridge`/`SyntheticStub` shadow is adjudicated by
            // `resolve_dispatch` on the interpreter path, and under `JdkOnly` a
            // `SyntheticStub` must reach that adjudication to be *refused*
            // rather than be quietly compiled around. So the kind is read
            // (`find_with_kind`, identical cost) but deliberately not filtered.
            let native_skip = if native_override::registered_native_will_run(
                shared,
                &class_name_str,
                method_name,
                method_descriptor,
            ) && !shared.jit.jit_cache.is_reentry_only_key(
                // Interpreter round i6 wave 117, lane L2
                // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): a shadowed method
                // marked re-entry-only compiles for the natives' bytecode-only
                // re-entries; its body publishes re-entry-only, so no caller
                // binds it (`JitCache::get`).
                &class_name_str,
                method_name,
                method_descriptor,
            ) && !native_override::mark_intrinsic_own_body_reentry_only(
                // JIT round 21 lane reflect3/4 (REFLECT4-21-1,
                // `CRATONVM_JIT_BYTECODE_ONLY_BODY`): a reviewed `Intrinsic`
                // re-entering its own body marks it re-entry-only here
                // instead of sealing it.
                shared,
                &class_name_str,
                method_name,
                method_descriptor,
            ) {
                true
            } else if !crate::runtime::env_cache::jit_native_shadow_caller_seal() {
                // The whole-method CALLER seal is RETIRED (opt-in since
                // interpreter round i1 wave 42, lane L2;
                // `CRATONVM_JIT=native-shadow-caller-seal` restores it). A
                // compiled call cannot bypass the native-vs-bytecode decision
                // at a site: this door's invoke sites dispatch through the
                // helpers (`JitInvokeInfo`), which arbitrate per call; its raw
                // binds are its own self-call, the `Math.sqrt` intrinsic and
                // natives `admit_direct_native_entry` admits; an elided
                // constructor asks `is_elidable_construction` (a native
                // constructor is not elidable); its `try_compile` route
                // screens a baked CALL (`direct_bind_name_refusal`) and a
                // splice (`resolve_inline_site_from`) per site. The default
                // configuration's worker never asked this seal and compiles
                // the 1,125 methods it sealed under `CRATONVM_BG_COMPILE=0`
                // with the suites passing (the orchestrator's census after
                // wave 41, `i38-L2-compile-door-review-items-left-open`). The
                // method's OWN registered native (the arm above) still
                // refuses, and seals as `calls-native-shadowed-method`.
                //
                // History: the lever was added for measurement, because the
                // seal excluded 1,281 methods from the JIT
                // on a Spring Boot context startup — more than the 1,155 that
                // reach C2 — and the per-arm census shows the population is
                // dominated by PRECISE hits (`direct=1015`), not by the
                // class-blind arm (169, whose removal was measured worth
                // nothing). So the question is no longer "is the detection too
                // wide" but "is the per-METHOD granularity worth replacing with
                // per-SITE", and that is a large compiler change. Pricing the
                // ceiling first is cheaper than building it: if the whole seal
                // is worth ~0 on this workload, the change should not be
                // attempted at all.
                false
            } else {
                jit_method_calls_native_shadowed(
                    shared,
                    class_id,
                    &code_attr.code,
                    code_attr.code.len(),
                )
            };
            // Record the ELIGIBLE verdict so the next entry short-circuits.
            // Scoped to exactly the three static per-method facts the seal
            // block below scopes itself to — `env_disable_jit` /
            // `redefine_jit_quiesced` / `gpu_gate_skip` / `clinit_skip` are
            // runtime or call-site-dependent and are deliberately NOT folded
            // in, in either direction.
            if static_skip_reason.is_none()
                && !fjp_skip
                && !native_skip
                && !finalizer_skip
            {
                cratonvm_jit::note_jit_gate_pass_fill();
                shared.jit.jit_gate_pass.write().insert(
                    (class_id, gate_pass_key.1.clone(), gate_pass_key.2.clone()),
                    (shared.jit.jit_cache.redefine_epoch(), is_interface_default),
                );
                // JIT round 21 wave 14 (lane finalize; item 2 of
                // `r21-review-w14-findings-20261019.md`): the allowed answer
                // above may predate a finalizable define that recorded this
                // constructor's class (`SharedVm::note_finalizable_define`
                // records under the class-manager write lock, THEN drops the
                // memo rows). A row inserted after that drop would never
                // expire, so the record is read again after the insert: a
                // drop that ran before the insert recorded first, and this
                // read sees it.
                // Wave 23 (lane finvm, item 1 of `r21-review-w23-findings`):
                // also the live bar, which a stub upgrade that made the class
                // finalizable flips before it drops the class's rows.
                if method_name == "<init>"
                    && (finalizer_ancestor_recorded(shared, class_id)
                        || finalizer_bars_compiling(shared, class_id, method_name, method_descriptor))
                {
                    shared.jit.jit_gate_pass.write().remove(&gate_pass_key);
                }
            }
            (
                is_interface_default,
                static_skip_reason,
                fjp_skip,
                native_skip,
            )
        };
        // DEBUG diagnostic — print every JIT compile decision for the
        // LazyProjection.equals method while bytebuddy_probe diagnosis
        // is in progress. Remove after fix lands.
        if crate::runtime::env_cache::dbg_bblp()
            && class_name_str.contains("LazyProjection")
            && method_name == "equals"
        {
            eprintln!(
                "[BBLP-firstcall] class={} method={} desc={} native_skip={} static_skip={:?} fjp_skip={} already_skipped={}",
                &*class_name_str, method_name, method_descriptor,
                native_skip, static_skip_reason, fjp_skip, already_skipped,
            );
        }
        // Kill-switch: CRATONVM_DISABLE_JIT=1 forces interpreter-only execution.
        // Mirrors the gates in `try_jit_compile_callee` / `try_jit_upgrade_with_gate` /
        // `try_osr` so the user-facing CRATONVM_DISABLE_JIT flag actually disables
        // the FIRST-CALL JIT compile path here too.
        // Continuations currently freeze precise interpreter frames. Until a
        // compiled activation can deopt directly into a heap stack chunk, a
        // virtual thread must not enter machine code that may cross a yield.
        // This is a per-thread execution gate; platform threads still compile
        // and use the shared artifacts normally.
        //
        // The BISECT levers ride along here for the same reason. This block's
        // eager first-call `x64::compile_with_param_slots` (further down)
        // reaches the backend WITHOUT going through `cratonvm_jit::try_compile`,
        // which is where `CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_ONLY` used
        // to be applied — so the single-pass tier was force-interpretable by
        // neither lever, and a bisect that could not stop the compile read as
        // an exoneration. Folded into `env_disable_jit` rather than into the
        // static-reason group below on purpose: that group SEALS the method
        // into `jit_skip_set`, and these are process-wide diagnostic flags, not
        // per-method facts about the bytecode. See
        // `cratonvm_jit::jit_force_interpret`.
        let env_disable_jit = crate::runtime::env_cache::disable_jit()
            || deferred_sync_entry
            || matches!(thread.kind, crate::threading::ThreadKind::Virtual)
            || cratonvm_jit::jit_force_interpret(&class_name_str, method_name);
        // Redefinition does NOT permanently bar a class from compiling.
        //
        // This used to be `class_was_redefined(shared, class_id)`. The
        // generation only ever increases, so that predicate is true forever
        // once a class has been redefined — and it sat in the skip set below,
        // which meant a class redefined once was condemned to the interpreter
        // for the life of the process. That is the whole of the "every call
        // into a redefined class costs ~33 µs" defect: one
        // `Mockito.mock(Foo.class)` permanently de-optimized `Foo`. Measured
        // on `docs/known-issues/repros/redefine-call-cost`: 691 ns/call
        // compiled before the redefine, 33,773 ns/call interpreted after it,
        // from redefining with *byte-identical* bytecode.
        //
        // It was also protecting nothing. `redefine_class` already evicts
        // every compiled artifact — `jit_cache.write().clear_all()` in
        // `vm_exec.rs` — so no code compiled
        // from the old body can survive the redefinition, and a later
        // compilation necessarily reads the current (agent-woven) bytecode
        // out of the class store. Blocking recompilation on top of a full
        // eviction is belt-and-braces that only costs throughput.
        //
        // What is deliberately NOT claimed: this does not order a compile
        // already in flight against a concurrent redefinition. A body
        // compiled from generation N could in principle publish after the
        // `clear_all()` for generation N+1. That race predates this change
        // (it applies to the first redefinition of any class), and closing it
        // wants a publish-time generation check rather than a permanent ban.
        //
        // `RedefineCorrectnessProbe` in the repro directory is the guard: it
        // redefines with a body whose arithmetic differs, then drives the
        // method hot, and fails if the recompiled code reverts to the old
        // body. That assertion was vacuous while this gate blocked
        // compilation, and is load-bearing now.
        let redefine_jit_quiesced = false;
        // GPU-offload JIT admission gate (known-issues followups item 2):
        // while `--gpu` is active, a caller whose bytecode contains an
        // offload-eligible invokestatic must stay interpreted, or its
        // JIT-compiled body would bypass the offload hook and silently
        // end GPU dispatch for that call site. One bool read when --gpu
        // is off; see offload_jit_gate for the cache/scan details.
        #[cfg(feature = "gpu-offload")]
        let gpu_gate_skip = crate::runtime::offload_jit_gate::caller_blocks_jit_by_name(
            shared,
            class_id,
            method_name,
            method_descriptor,
        );
        #[cfg(not(feature = "gpu-offload"))]
        let gpu_gate_skip = false;
        // A `<clinit>` must not take the eager FIRST-CALL compile door.
        //
        // Introduced as an ordering fix: entering a compiled artifact used to
        // run a `static_init_classes` pre-walk that `ensure_class_initialized`d
        // the declaring class of EVERY getstatic/putstatic site anywhere in the
        // body, before bytecode zero (that walk is gone — interpreter round i1
        // waves 2 and 7 — so the cost argument below is what keeps this). For
        // a `<clinit>` it inverted the very order the initializer exists to
        // establish, and the JDK has initializers whose correctness is exactly
        // that order:
        //
        //   `java/lang/constant/ConstantDescs.<clinit>` assigns
        //   `BSM_PRIMITIVE_CLASS` (line 198) and only then reads
        //   `PrimitiveClassDescImpl.CD_int` (line 249) — and
        //   `PrimitiveClassDescImpl.<clinit>`'s own ctor reads
        //   `ConstantDescs.BSM_PRIMITIVE_CLASS`. Hoisting the line-249 trigger
        //   to method entry runs that ctor against a null `BSM_PRIMITIVE_CLASS`
        //   → `NullPointerException` → `ExceptionInInitializerError`, and both
        //   classes are poisoned for the rest of the process. `ConstantDescs
        //   .<clinit>` then never executes a single putstatic. Deterministic:
        //   `MethodHandles.arrayElementVarHandle(int[].class)` reaches it
        //   through `MethodTypeForm` → `sun/invoke/util/Wrapper.<clinit>`.
        //
        // Only reachable with `CRATONVM_BG_COMPILE=0` today, because the
        // default background pipeline does not first-call-compile here — which
        // is why the documented opt-out was unusable on any real classpath.
        //
        // Refusing costs nothing: a `<clinit>` runs at most once per class per
        // loader, so a method-entry compile can never amortize its own codegen.
        // A `<clinit>` with a genuinely hot loop still reaches the OSR door,
        // which enters mid-body and does not run this pre-walk.
        let clinit_skip = method_name == "<clinit>";
        if env_disable_jit
            || redefine_jit_quiesced
            || already_skipped
            || static_skip_reason.is_some()
            || fjp_skip
            || native_skip
            || gpu_gate_skip
            || clinit_skip
            // JVMTI interp-only mode (never sealed: it ends when the agent
            // disables the event). See `jvmti_requires_interpreter`. The
            // JDWP half concerns this method only (wave 8, lane L1).
            || jvmti_requires_interpreter_for_method(
                shared,
                class_id,
                method_name,
                method_descriptor,
            )
            // JIT round 21 wave 11 (lane finalize): see `finalizer_skip`
            // above (memoized per class id since wave 14, never sealed by
            // name).
            || finalizer_skip
        {
            // Method has known JIT issues — skip JIT.
            //
            // PERF FIX (2026-07-15, completes the `already_skipped` short-
            // circuit added above): `static_skip_reason` / `fjp_skip` /
            // `native_skip` are all pure functions of this method's static
            // bytecode+metadata (policy table lookup, ForkJoinTask ancestor
            // chain, native-shadow bytecode scan) — none of them can change
            // for a given class+method+descriptor within this process. Yet
            // this branch previously left `jit_skip_set` untouched, so
            // `already_skipped` could NEVER become true for a
            // native_skip/static_skip_reason/fjp_skip method: every future
            // call to `execute()` for it recomputed all three from scratch
            // (worst case, `jit_method_calls_native_shadowed`'s full
            // O(bytecode-size) decode-and-scan) forever, defeating the very
            // short-circuit `already_skipped` exists for. Seal it here, the
            // same way a permanent backend-compile failure already seals via
            // `mark_jit_bail_listed`/the `jit_skip_set.write().insert(...)`
            // calls in the compile-attempt branch below. Scoped to the
            // static per-method reasons only — `env_disable_jit` /
            // `redefine_jit_quiesced` / `gpu_gate_skip` are runtime or
            // call-site-dependent, not per-method-permanent, so they must NOT
            // poison this method's entry for future calls where those flags
            // may differ.
            // `clinit_skip` joins the sealed set for the same reason the other
            // three do: it is a pure function of the method name, so recomputing
            // it on every call is waste. (`<clinit>` is called once, so the seal
            // rarely pays — but leaving it out would make `already_skipped`
            // unreachable for it, which is the trap this seal exists to avoid.)
            if static_skip_reason.is_some() || fjp_skip || native_skip || clinit_skip {
                // Name WHICH of the four fired. The single label
                // `static-policy-or-native-shadow` covered all of them, and a
                // Spring Boot context startup seals 856 methods through here —
                // more than the 69 whose compile was attempted and refused —
                // with no way to tell a policy-table entry from a native-shadow
                // scan hit. Those want opposite fixes: one is a list somebody
                // can shorten, the other is a scan that may be over-matching.
                // Priority order, not a set: the reasons can co-occur, and the
                // first one listed is the one that would still seal the method
                // if every other were lifted.
                let seal_site = if static_skip_reason.is_some() {
                    "static-policy-table"
                } else if native_skip {
                    "calls-native-shadowed-method"
                } else if fjp_skip {
                    "forkjointask-subclass"
                } else {
                    "clinit"
                };
                note_jit_skip_seal(seal_site, &skip_key);
                // Counted as well as traced: `CRATONVM_DBG_JITC` produces ~1 GB
                // on a Spring startup, so the census has to be readable from the
                // one-line `jit-method-stats` dump instead.
                cratonvm_jit::note_jit_skip_seal_reason(seal_site);
                shared.jit.jit_skip_set.write().insert(skip_key.clone());
            }
        } else {
            {
                // RBC.5 — consult the JIT cache FIRST. An already-compiled method
                // needs NO `padded_bytecode` (alloc + memcpy), NO `jit_scan`
                // (linear bytecode walk) and NO Arc key allocations —
                // `JitCache::get` takes `&str`. The previous order paid all of
                // that on EVERY uncached invocation of every JIT-eligible
                // method. The org/bouncycastle blanket ban happened to
                // short-circuit it for BC code at the `static_skip_reason` gate,
                // which made LIFTING the ban look ~4.7× slower on the asn1
                // RegressionTest even when not a single BC method was compiled
                // (the ~190s CPU-bound anomaly in
                // docs/feature-designs/jit-ir-optimizations.md).
                let compiled = if let Some(entry) = &entry_record {
                    entry.compiled(shared, class_id, &class_name_str)
                } else {
                    let jit_cache = shared.jit.jit_cache.read();
                    // Interpreter round i6 wave 117, lane L2
                    // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`): the method
                    // `execute` runs as bytecode may run its re-entry-only body.
                    jit_cache.get_for_reentry(
                        &class_name_str,
                        method_name,
                        method_descriptor,
                        class_id,
                    )
                };
                // CRATONVM_JIT_C2_FIRST_CALL: set when the gated branch DEFERS a
                // not-yet-hot method (returns None to interpret rather than compile).
                // The first-call-failure seal below must NOT fire for that case —
                // sealing inserts `skip_key` into `jit_skip_set`, which makes the next
                // call's `already_skipped` gate skip the whole JIT block, permanently
                // stopping the invocation counter from ever re-running (so an
                // `execute`-only-reached hot method would never compile gate-ON). A
                // genuine hot-path backend bail (try_jit_compile_callee → None) leaves
                // this false and still seals, matching the gate-OFF semantics.
                let mut c2_not_hot = false;
                // Set when the ordinary-door route below defers a static
                // method whose class is still being initialized: a transient
                // `None` that the seal after this closure must not make
                // permanent (round 12 wave 2).
                let mut eager_deferred = false;
                // Interpreter round i6 wave 117, lane L7: the background-compile
                // arm (the default: count the call, maybe offer it for tier-up,
                // interpret) runs here, not inside the closure below. That closure
                // is the eager first-call compile, compiled as its own function
                // (`execute::closure$16` in the release PDB) with a 13 176-byte
                // frame and a `__chkstk` probe, and every `execute` of a method
                // without a compiled body (each native re-entry into a cold or
                // not-yet-published callee) called it only to take this arm. Same
                // work, same order; a pure refactor.
                let bg_compile_arm =
                    compiled.is_none() && crate::runtime::env_cache::bg_compile();
                if bg_compile_arm {
                    execute_count_for_background_compile(
                        shared,
                        class_id,
                        &class_name_str,
                        method_name,
                        method_descriptor,
                    );
                    c2_not_hot = true; // keep the counter running; do not seal
                }
                let eager_compile = || {
                    // wire-tiered-manager Step 7 (full eager-reroute): when the
                    // background pipeline is the default (`bg_compile`, default-ON),
                    // the WORKER is the compiler — do NOT eager- or inline-compile on
                    // the mutator here. Count invocations and, once warm, ensure the
                    // worker + enqueue via the tiered manager, then return None to
                    // INTERPRET; the worker compiles off-thread and publishes into
                    // `jit_cache`, and a later invocation (here, for
                    // reflective/uncached-hot methods, or via the cached-dispatch
                    // fast-path) flips this call site to the Jit body. `c2_not_hot =
                    // true` keeps the counter running so the method is never sealed as
                    // permanently-uncompiled. Opt-out (`CRATONVM_BG_COMPILE=0`) falls
                    // through to the historical eager/inline paths below.
                    // Interpreter round i6 wave 117, lane L7: answered before this
                    // closure is called (`bg_compile_arm` above), so this arm is
                    // reached only if the switch were read differently here; kept
                    // so the closure stays complete on its own.
                    if crate::runtime::env_cache::bg_compile() {
                        execute_count_for_background_compile(
                            shared,
                            class_id,
                            &class_name_str,
                            method_name,
                            method_descriptor,
                        );
                        c2_not_hot = true; // keep the counter running; do not seal
                        return None; // interpret — the worker compiles off-thread
                    }
                    // activate-ir-optimizer (runtime wiring, CRATONVM_JIT_C2_FIRST_CALL,
                    // default-OFF): replace the eager single-pass first-call compile
                    // below with an invocation-counted upgrade through the optimizing
                    // IR pipeline. The eager `x64::compile` (further down) caches a
                    // non-IR body on call #1 and thereby preempts `jit::try_compile`
                    // on every later path. Here we instead: (a) count invocations and,
                    // below the warmup threshold, return None so the method INTERPRETS
                    // — which ALSO un-preempts the dispatcher/OSR warmup paths that are
                    // already wired to `try_compile(optimize=true)`, so a method that
                    // goes hot through the stackless dispatcher still reaches IR there;
                    // and (b) once hot, compile via `try_jit_compile_callee` →
                    // `jit::try_compile(optimize=true)`, which SUBSUMES single-pass
                    // (it falls back to `x64::compile_with_param_slots` internally for
                    // IR-incompatible bodies), then re-fetch the cached body. The
                    // single-pass block below is unreached while the flag is set.
                    if c2_first_call_enabled() {
                        // The canonical key; see the background-compile twin above.
                        let invoc_key = cratonvm_jit_api::invoc_key_parts(
                            class_id.as_u32(),
                            method_name,
                            method_descriptor,
                        );
                        let n = shared.jit.profile_store.increment_invocation(invoc_key);
                        if n < shared.config.jit_invocation_threshold {
                            c2_not_hot = true; // defer, do NOT seal (counter must keep running)
                            return None; // not hot yet — interpret (dispatcher/OSR may reach IR)
                        }
                        // Two refusals of the by-name door below are not
                        // verdicts at this door, and its `None` reaches the
                        // `early-backend-bail` seal after this closure (round
                        // 12 wave 2, tier proposal W2-5):
                        //  * a static method whose class is still initializing
                        //    (JVMS 5.5) compiles once initialization is done,
                        //    so defer it without a seal;
                        //  * an `ACC_SYNCHRONIZED` method is refused because a
                        //    raw direct call supplies no monitor, but `execute`
                        //    holds it around the body (`sync_monitor` above),
                        //    so compile it through the wrapped-entry door the
                        //    background worker uses.
                        // `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0` restores the
                        // by-name door for both.
                        {
                            if is_static
                                && !shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(class_id)
                                    .is_some_and(crate::vm::is_class_initialized_fast)
                            {
                                eager_deferred = true;
                                return None;
                            }
                            // Only while `execute` itself takes the monitor
                            // (`CRATONVM_NO_EXECUTE_SYNC` unset).
                            if is_synchronized && !execute_sync_disabled() {
                                let mut policy_declined = false;
                                return match cratonvm_jit::tiered::contain_compile_panic(|| {
                                    jit_bridge::try_jit_compile_wrapped_entry(
                                        shared,
                                        class_id,
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                        true, // optimize = C2 / optimizing IR pipeline
                                        &mut policy_declined,
                                    )
                                }) {
                                    Ok(Some((body, _entry, _needs_ctx))) => Some(body),
                                    Ok(None) | Err(_) => None,
                                };
                            }
                        }
                        // For this frame's own class id, the key the re-probe
                        // below reads: by name, a class name two loaders define
                        // was refused as ambiguous.
                        return match try_jit_compile_callee_for_class(
                            shared,
                            class_id,
                            &class_name_str,
                            method_name,
                            method_descriptor,
                            true, // optimize = C2 / optimizing IR pipeline
                        ) {
                            Some(_) => {
                                let jit_cache = shared.jit.jit_cache.read();
                                jit_cache.get(&class_name_str, method_name, method_descriptor, class_id)
                            }
                            None => None,
                        };
                    }
                    // ── The admission gate, third door ────────────────────
                    //
                    // This block reaches `x64::compile_with_param_slots`
                    // directly, like `compile_osr_artifact` and unlike
                    // `jit::try_compile`. It had already been taught the
                    // kill-switch and the bisect levers by hand (see
                    // `env_disable_jit` above) but never the permanent
                    // bail-list, the code-cache cap, or the compile-epoch
                    // witness — so an eager first-call compile could re-run the
                    // pipeline on a method the backend had permanently refused,
                    // commit code past a cap the ordinary door was respecting,
                    // and publish a body stamped at buffer finalize rather than
                    // from before the first constant-pool read.
                    // `compile_gate::admit` asks all of them; the token owns
                    // the epoch witness and must outlive the resolution below.
                    //
                    // x86-64 ONLY. This door reaches `x64::compile_with_param_slots`
                    // directly, so on any other architecture it would publish
                    // x86-64 bytes as the method's entry point. Other targets
                    // compile through `jit::try_compile`, which selects their
                    // backend. `cfg!` rather than `#[cfg]`, so the rest of this
                    // path stays type-checked everywhere and is not dead code.
                    if cfg!(not(target_arch = "x86_64")) {
                        return None;
                    }
                    // The code-cache cap is transient (reclamation restores
                    // headroom), and its refusal -- by this door's `admit`
                    // below or by the ordinary door's -- reached the
                    // `early-backend-bail` seal after this closure, which shut
                    // this door and `execute`'s cache consult on the method for
                    // good (interpreter round i1 wave 38, lane L2; item 3 of
                    // the wave-37 compile-door review). Deferred instead, asked
                    // as one relaxed load, so a VM at its cap does not run
                    // `admit`'s drain on every call of every uncompiled method.
                    if cratonvm_jit::jit_code_cache_under_pressure() {
                        eager_deferred = true;
                        return None;
                    }
                    // A method with an exception table takes the ordinary door
                    // (round 12 wave 2, lane tier). This door hands the backend
                    // no handler tables at all. With no `protected_ranges`, a
                    // sibling tail-call or a caller-held synchronized CALL
                    // inside a `try` tears this frame down first, so the
                    // callee's throw escapes the handler. With no
                    // `precise_exception_frames`, a handler that reads a local
                    // beyond the parameters is declined by
                    // `run_jit_callee_handler` (no frame was published) and the
                    // exception propagates past it. This door used to seal
                    // only the `athrow` half of that population, permanently
                    // (`early-rbc6-athrow-with-handler`, on a rule
                    // `try_compile_inner` dropped long ago), and compiled the
                    // rest without the tables. `jit::try_compile` stages every
                    // table and applies the RBC.6 gate, so ask it, at this
                    // door's own tier (single-pass), as the C2-first-call arm
                    // above does at the optimizing one. Its refusal is a real
                    // bail and is sealed below like this door's own.
                    //
                    // A long/double parameter takes the same route: this door
                    // lays parameters out one slot per argument, so it bailed
                    // on them (the category-2 guard below), and that bail was
                    // sealed too. `try_compile` lays them out by JVM slot.
                    //
                    // `CRATONVM_JIT_EAGER_DOOR_ROUTE_ALL=1` sends every method
                    // this way (tier proposal W2-1's measurement arm).
                    if !code_attr.exception_table.is_empty()
                        || crate::jit::count_param_slots_jvm_spec(method_descriptor)
                            != crate::jit::count_param_slots(method_descriptor)
                        || eager_door_routes_every_method()
                    {
                        // That door refuses a static method whose class is
                        // not initialized yet (JVMS 5.5; a call from the
                        // class's own `<clinit>` reaches here). Defer without
                        // a seal; the next call after initialization compiles.
                        if is_static
                            && !shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(class_id)
                                .is_some_and(crate::vm::is_class_initialized_fast)
                        {
                            eager_deferred = true;
                            return None;
                        }
                        // The wrapped-entry door, as the background worker
                        // compiles: `execute` holds an `ACC_SYNCHRONIZED`
                        // method's monitor around the body, which the plain
                        // by-name door (`try_jit_compile_callee_for_class`)
                        // refuses to assume. Panics contained like that door's.
                        let mut policy_declined = false;
                        return match cratonvm_jit::tiered::contain_compile_panic(|| {
                            jit_bridge::try_jit_compile_wrapped_entry(
                                shared,
                                class_id,
                                &class_name_str,
                                method_name,
                                method_descriptor,
                                false, // this door's tier: single-pass
                                &mut policy_declined,
                            )
                        }) {
                            Ok(Some((body, _entry, _needs_ctx))) => Some(body),
                            Ok(None) => {
                                // Wave 41 (lane L2): a publication the
                                // redefinition hold refused is not sealed.
                                if shared.jit.jit_cache.publications_held() {
                                    eager_deferred = true;
                                }
                                None
                            }
                            Err(_) => None,
                        };
                    }
                    let admission = match cratonvm_jit::compile_gate::admit(
                        // THIS VM's bail list, the one its other doors record
                        // into (interpreter round i1 wave 11).
                        shared.jit.tiered_manager.verdicts(),
                        class_id,
                        &class_name_str,
                        method_name,
                        method_descriptor,
                        cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                        // M7 — THIS VM's runtime de-speculation verdicts. Was a
                        // process-global map keyed by a tuple two VMs in one
                        // process routinely share, so a second VM's give-up
                        // decision shut this door on a method that had never
                        // mis-speculated.
                        Some(&shared.jit.runtime_despec),
                    ) {
                        Ok(admission) => admission,
                        Err(refusal) => {
                            // Wave 38: a transient refusal is not sealed.
                            if refusal.is_transient() {
                                eager_deferred = true;
                            }
                            return None;
                        }
                    };
                    let _phase =
                        cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Compilation);
                    let eager_decision_started = cratonvm_jfr::jit_decision::jit_decision_enabled()
                        .then(std::time::Instant::now);
                    // `H20-1`'s other half, landed with `H12-1` O1/O2 on
                    // 2026-09-22: say which policy this door's direct-call
                    // ladder obeys, so `undeclared_direct_bind_rows` can tell a
                    // door that refused from a door that never asked. Before
                    // this, `--real-jdk` and `--fast-boot` measured an IDENTICAL
                    // per-door bind profile and nothing in the tree could say
                    // why (`H12-1` §3b).
                    //
                    // Declaring is not deciding. The decision is
                    // `helpers::admit_direct_native_entry`'s, at each bind site,
                    // because that is the only place a refusal can fall through
                    // to the `invoke_info` build instead of stranding the pc.
                    admission.declare_direct_call_policy(
                        cratonvm_jit::compile_gate::DirectCallPolicy::from_jdk_only(
                            crate::vm::dispatch_policy(shared).is_jdk_only(),
                        ),
                    );
                    // Wave 42 (lane L2): `code_attr` was copied at this call's
                    // entry, before the admission's epoch witness opened. A
                    // redefinition that installed in between must not be
                    // compiled around (`jit_bridge::method_code_is_current`);
                    // deferred, not sealed: the next call compiles the new
                    // bytecode. Since wave 43 the re-check is
                    // `jit_bridge::CompileInputs::adopt_code`, the rule every
                    // compile door's adopted bytecode passes.
                    let Some(eager_inputs) = jit_bridge::CompileInputs::<Arc<[u8]>>::adopt_code(
                        shared,
                        class_id,
                        method_name,
                        method_descriptor,
                        &code_attr.code,
                    ) else {
                        if crate::runtime::env_cache::dbg_jitc() {
                            eprintln!(
                                "[cratonvm-jitc] eager door deferred {class_name_str}.{method_name}{method_descriptor}: \
                                 the class was redefined after this call read its bytecode"
                            );
                        }
                        eager_deferred = true;
                        return None;
                    };
                    let padded = Arc::clone(eager_inputs.get());
                    let code_len = code_attr.code.len();
                    let scan = match crate::jit::x64::jit_scan(&padded, code_len, method_descriptor)
                    {
                        Some(s) => s,
                        None => {
                            // RBC.4 — seal scan-rejected methods so this path
                            // doesn't re-run jit_scan on every uncached
                            // invocation.
                            note_jit_skip_seal("early-jit-scan-reject", &skip_key);
                            cratonvm_jit::emit_direct_door_jit_decision(
                                &class_name_str,
                                method_name,
                                method_descriptor,
                                cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                                cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                                "early-jit-scan-reject",
                                cratonvm_jit::take_jit_bail_site(),
                                code_len,
                                eager_decision_started,
                            );
                            shared.jit.jit_skip_set.write().insert(skip_key.clone());
                            return None;
                        }
                    };
                    // The pre-round-12 split, reached only under
                    // `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0` (a method
                    // with handlers otherwise left above): seal an `athrow`
                    // method with handlers, compile the rest without tables.
                    if scan.has_athrow && !code_attr.exception_table.is_empty() {
                        note_jit_skip_seal("early-rbc6-athrow-with-handler", &skip_key);
                        cratonvm_jit::emit_direct_door_jit_decision(
                            &class_name_str,
                            method_name,
                            method_descriptor,
                            cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                            cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                            "early-rbc6-athrow-with-handler",
                            None,
                            code_len,
                            eager_decision_started,
                        );
                        shared.jit.jit_skip_set.write().insert(skip_key.clone());
                        return None;
                    }
                    // Interpreter round i1 wave 28, lane L2
                    // (`docs/feature-designs/real-jdk-mode.md`):
                    // the rest of that population is compiled WITH its table,
                    // on the method-entry door's own RBC.6 rule. A handler that
                    // reads a local beyond the parameters needs a precise
                    // exceptional frame at every throwing site in its ranges,
                    // or the method is sealed as that door bails it
                    // (`rbc6-handler-reads-unsafe-local`); the table itself is
                    // staged on the request below by the helper every
                    // single-pass door shares
                    // (`BackendRequest::stage_exception_table`).
                    let early_precise_exception_frames = !code_attr.exception_table.is_empty()
                        && cratonvm_jit::handler_resume_requires_precise_locals(
                            &padded,
                            code_len,
                            &code_attr.exception_table,
                            method_descriptor,
                            is_static,
                        );
                    if early_precise_exception_frames
                        && cratonvm_jit::precise_handler_frame_blocking_site(
                            &padded,
                            code_len,
                            &code_attr.exception_table,
                        )
                        .is_some()
                    {
                        note_jit_skip_seal("early-rbc6-handler-reads-unsafe-local", &skip_key);
                        cratonvm_jit::emit_direct_door_jit_decision(
                            &class_name_str,
                            method_name,
                            method_descriptor,
                            cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                            cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                            "early-rbc6-handler-reads-unsafe-local",
                            None,
                            code_len,
                            eager_decision_started,
                        );
                        shared.jit.jit_skip_set.write().insert(skip_key.clone());
                        return None;
                    }
                    let class_name_arc: Arc<str> = Arc::from(&*class_name_str);
                    let method_name_arc: Arc<str> = Arc::from(method_name);
                    let descriptor_arc: Arc<str> = Arc::from(method_descriptor);
                    // Resolve multianewarray entries if present
                    let mut mna_info = Vec::new();
                    if !scan.multianewarray_ops.is_empty() {
                        let cm = shared.classes.class_manager.read();
                        let class = cm.get_class(class_id)?;
                        for &(pc, cp_idx, _ndims) in &scan.multianewarray_ops {
                            // A malformed CP entry is still a whole-compile refusal.
                            let _ = class.constant_pool.get_class_name(cp_idx)?;
                            mna_info
                                .push((pc, crate::jit::pack_multianewarray_site(class_id.as_u32(), cp_idx)));
                        }
                    }
                    // Resolve typecheck entries (checkcast/instanceof) if present
                    let mut typecheck_info: Vec<(usize, *const u8, usize)> = Vec::new();
                    let mut owned_jit_strings: Vec<Box<str>> = Vec::new();
                    if !scan.typecheck_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        let class = cm_lock.get_class(class_id)?;
                        for &(pc, cp_idx) in &scan.typecheck_ops {
                            let class_name = class.constant_pool.get_class_name(cp_idx)?;
                            // Interned for the life of the process rather than
                            // owned by this compilation: `jit_checkcast` /
                            // `jit_instanceof` memoize on the `(ptr, len)` pair
                            // in thread-locals that outlive the compiled
                            // method, so a recycled address would answer for
                            // the class that used to live there. See
                            // `cratonvm_jit::intern_typecheck_target`.
                            //
                            // Resolved here through THIS class's own defining
                            // loader, exactly as the interpreter's constant-pool
                            // resolution would, and interned under that
                            // identity. The class dictionary is keyed by
                            // `(ClassLoaderId, name)`, so handing the runtime
                            // helper a bare name left it guessing between two
                            // loaders' same-named copies.
                            //
                            // Only what that loader has defined or initiated
                            // counts (`jit_known_class`), and under
                            // `--real-jdk` a JDK-global name the loader may
                            // define itself only once it has answered it
                            // (`jit_known_class_constant`, interpreter round i1
                            // wave 38, lane L2); anything else is interned
                            // under its holder below and resolved through the
                            // loader at run time. The entry's record first
                            // (JIT round 16, lane classsite: the cast the
                            // interpreter runs at this entry answers it).
                            let target_id = jit_bridge::jit_known_class_constant_at(
                                shared,
                                &cm_lock,
                                class_name,
                                class_id,
                                Some(cp_idx),
                            )
                            .map(|id| id.as_u32());
                            // Round 9 wave 8b (arraylist8 request 3): this door
                            // also hands `instanceof` the final-USER-class
                            // verdict, as the three `CompileRequest` doors do.
                            let target_is_final = target_id.is_some_and(|id| {
                                jit_bridge::jit_class_is_final_for_typecheck_in(
                                    shared, &cm_lock, id,
                                )
                            });
                            let (ptr, len) = match target_id {
                                Some(_) => cratonvm_jit::intern_typecheck_target_with_finality(
                                    class_name,
                                    target_id,
                                    target_is_final,
                                ),
                                None => cratonvm_jit::intern_typecheck_unresolved_in(
                                    class_name,
                                    class_id.as_u32(),
                                    cp_idx,
                                ),
                            };
                            typecheck_info.push((pc, ptr, len));
                        }
                    }
                    // Resolve static field entries (getstatic/putstatic) if present
                    let mut static_field_info: Vec<(usize, u32, usize, u8, bool)> = Vec::new();
                    if !scan.static_field_ops.is_empty() {
                        for &(pc, cp_idx) in &scan.static_field_ops {
                            // A field whose owner nothing has loaded yet is
                            // the common case at a method's FIRST call, and the
                            // interpreted call this falls back to resolves it:
                            // deferred without the `early-backend-bail` seal a
                            // few times per method, then sealed as before
                            // (interpreter round i1 wave 39, lane L2; item 3 of
                            // `i38-L2-compile-door-review-items-left-open`).
                            let field = match resolve_field_ref(shared, class_id, cp_idx) {
                                Ok(field) => field,
                                Err(_) => {
                                    let retry = shared.jit.bridge.eager_resolver_miss_may_retry(
                                        class_id,
                                        method_name,
                                        method_descriptor,
                                    );
                                    if crate::runtime::env_cache::dbg_jitc() {
                                        eprintln!(
                                            "[cratonvm-jitc] eager-first-call resolver miss \
                                             {class_name_str}.{method_name}{method_descriptor} \
                                             pc={pc} static cp={cp_idx}: {}",
                                            if retry { "deferred" } else { "sealed" }
                                        );
                                    }
                                    eager_deferred = retry;
                                    return None;
                                }
                            };
                            // JIT round 15 wave 4 (lane fieldowner): a site whose
                            // owner the interpreter resolves through the loader
                            // to another class is a resolver miss here too
                            // (`jit_bridge::jit_field_owner_refusal`).
                            if let Some(why) =
                                jit_bridge::jit_field_owner_refusal(shared, class_id, cp_idx, &field)
                            {
                                let retry = shared.jit.bridge.eager_resolver_miss_may_retry(
                                    class_id,
                                    method_name,
                                    method_descriptor,
                                );
                                if crate::runtime::env_cache::dbg_jitc() {
                                    eprintln!(
                                        "[cratonvm-jitc] eager-first-call field-owner refusal \
                                         {class_name_str}.{method_name}{method_descriptor} \
                                         pc={pc} static cp={cp_idx}: {why}"
                                    );
                                }
                                eager_deferred = retry;
                                return None;
                            }
                            // JVMS §6.5 linking ICCE: leave a `getstatic` of an
                            // instance field to the interpreter, which throws it,
                            // and likewise an illegal final-field `putstatic`
                            // (`IllegalAccessError`).
                            if !field.is_static
                                || (padded.get(pc).copied() == Some(0xb3)
                                    && final_field_put_illegal(
                                        shared,
                                        class_id,
                                        method_name,
                                        &field,
                                    ))
                            {
                                return None;
                            }
                            let cm_lock = shared.classes.class_manager.read();
                            let class = cm_lock.get_class(class_id)?;
                            let nat_idx = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::FieldReference {
                                    name_and_type_index,
                                    ..
                                }) => *name_and_type_index,
                                _ => return None,
                            };
                            let (_, descriptor) = class.constant_pool.get_name_and_type(nat_idx)?;
                            let type_tag = *descriptor.as_bytes().first()?;
                            // The slot's own descriptor must be the one the
                            // constant pool names, as the OSR door and the
                            // `CpResolvers` ask (interpreter round i1 wave 38,
                            // lane L2; item 9 of the wave-37 door review).
                            if !jit_bridge::jit_field_tag_agrees(&field, type_tag, cp_idx) {
                                if crate::runtime::env_cache::dbg_jitc() {
                                    eprintln!(
                                        "[cratonvm-jitc] eager-first-call field-tag refusal \
                                         {class_name_str}.{method_name}{method_descriptor} pc={pc} \
                                         static cp={cp_idx}"
                                    );
                                }
                                return None;
                            }
                            static_field_info.push((
                                pc,
                                field.declaring_class_id.as_u32(),
                                field.field_index,
                                type_tag,
                                field.is_volatile,
                            ));
                        }
                    }
                    // Resolve invoke entries (invokevirtual/invokespecial/invokeinterface) if present
                    let mut invoke_info: Vec<(usize, *const crate::jit::JitInvokeInfo)> =
                        Vec::new();
                    let mut owned_jit_invoke_infos: Vec<Box<crate::jit::JitInvokeInfo>> =
                        Vec::new();
                    // The callee-deopt service's info for this method's raw
                    // static self-calls (`BackendRequest::self_call_invoke_info`),
                    // staged by the self-call arm below. One per compile: every
                    // such site calls this same method.
                    let mut eager_self_call_invoke_info: usize = 0;
                    let mut direct_calls_early: Vec<(usize, crate::jit::JitDirectCall)> =
                        Vec::new();
                    let mut mic_slots_early: Vec<(usize, *const crate::jit::JitMICSlot)> =
                        Vec::new();
                    let mut owned_mic_slots_early: Vec<Box<crate::jit::JitMICSlot>> =
                        Vec::new();
                    let mut pic_slots_early: Vec<(usize, *const crate::jit::JitPICSlot)> =
                        Vec::new();
                    let mut owned_pic_slots_early: Vec<Box<crate::jit::JitPICSlot>> =
                        Vec::new();
                    // Per-allocation ctor-dispatch elision: a `new C(); dup;
                    // invokespecial C.<init>()V` whose `C.<init>` is the empty
                    // default constructor (`is_elidable_construction`) has NO
                    // observable effect — the inline-TLAB `new` already zeroed
                    // the fields and the only call is the no-op `Object.<init>`.
                    // For such a site we emit the call site AS `Object.<init>`
                    // so the existing codegen elision (the fix-2 `0xb7` arm)
                    // drops the per-object `jit_invoke_dispatch` entirely. This
                    // is the dominant mutator cost on allocation-heavy code
                    // (object `binarytrees`: `TreeNode.<init>` was dispatched
                    // ~135K times at depth 10). Sound in BOTH paths: even if the
                    // elision did not fire, dispatching `Object.<init>` on the
                    // freshly-allocated object is the same no-op as `C.<init>`.
                    // Opt out with `CRATONVM_NO_CTOR_DIRECT_CALL`.
                    let ctor_direct_call_off =
                        crate::runtime::env_cache::ctor_direct_call_disabled();
                    // Trivial `invokespecial …<init>()V` sites, deferred so their
                    // target class can be resolved via `load_class_concurrent`
                    // AFTER `cm_lock` is dropped — `find_class_by_name` does not
                    // see app-loaded classes in this context (the `new`-site path
                    // resolves classes the same way for the same reason).
                    let mut pending_ctor_sites: Vec<(usize, String, usize)> = Vec::new();
                    if !scan.invoke_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        let class = cm_lock.get_class(class_id)?;
                        for &(pc, cp_idx, opcode) in &scan.invoke_ops {
                            // Resolve method reference from constant pool
                            let (ref_class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::MethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                Some(ConstantPoolEntry::InterfaceMethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                _ => continue,
                            };
                            let target_class =
                                match class.constant_pool.get_class_name(ref_class_idx) {
                                    Some(n) => n,
                                    None => continue,
                                };
                            let (method_name_ref, descriptor_ref) =
                                match class.constant_pool.get_name_and_type(nat_idx) {
                                    Some(pair) => pair,
                                    None => continue,
                                };
                            // Count JIT arg slots (receiver + params for virtual, just params for static)
                            let param_count = crate::jit::count_param_slots(descriptor_ref);
                            let mut invoke_kind = match opcode {
                                0xb6 => 0u8, // invokevirtual
                                0xb7 => 1,   // invokespecial
                                0xb9 => 2,   // invokeinterface
                                0xb8 => 3,   // invokestatic
                                _ => continue,
                            };
                            // JVMS 5.4.6 — an `invokevirtual` naming a PRIVATE
                            // method is not a dispatch site. This door reaches
                            // the backend WITHOUT going through
                            // `cratonvm_jit::try_compile`, so it does not
                            // inherit the reclassification made there and has to
                            // make the same one itself. See
                            // `invoke::invokevirtual_site_targets_private`; it is
                            // this door, reached only under
                            // `CRATONVM_BG_COMPILE=0`, that kept
                            // `TCPSSLOptions.init()` from ever running.
                            //
                            // An `invokeinterface` (kind 2) whose resolved method
                            // is a PRIVATE interface method is pinned the same
                            // way (JVMS §6.5 selection step 1): it selects that
                            // method, never the receiver's public namesake.
                            if matches!(invoke_kind, 0 | 2)
                                && crate::runtime::interpreter::invoke::invokevirtual_site_targets_private(
                                    &cm_lock,
                                    class_id,
                                    target_class,
                                    method_name_ref,
                                    descriptor_ref,
                                )
                            {
                                // `--real-jdk`: no compiled receiver check for
                                // a pinned private INTERFACE method (wave 37,
                                // lane L2); keep the method interpreted.
                                if opcode == 0xb9 && shared.classes.selection_checks {
                                    jit_bridge::dbg_special_site_refusal(
                                        "eager-first-call",
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                        (target_class, method_name_ref, descriptor_ref),
                                        "invokeinterface-private-receiver-unchecked",
                                    );
                                    return None;
                                }
                                if invoke_kind == 0 {
                                    cratonvm_jit::PRIVATE_INVOKEVIRTUAL_PINNED
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                invoke_kind = 1;
                            }
                            let invoke_kind = invoke_kind;
                            // This door binds an `invokespecial` by its
                            // constant-pool class; a site the interpreter
                            // selects differently (or refuses) keeps the
                            // method interpreted (interpreter round i1 wave
                            // 37, lane L2; `special_site_bind_refusal`).
                            // Interpreter round i6 wave 120, lane L2
                            // (`CRATONVM_JIT_INVOKE_SITE_CLASSIFIER`, default
                            // ON since the i6 wave-126 flip): with the classifier the selection start is
                            // BOUND here, as at the single-pass OSR door
                            // (`jit_bridge::classify_invoke_site`): its name
                            // replaces `target_class` for the rest of the site.
                            let mut substituted_owner: Option<Arc<str>> = None;
                            if opcode == 0xb7 && method_name_ref != "<init>" {
                                let is_iface = matches!(
                                    class.constant_pool.get(cp_idx),
                                    Some(ConstantPoolEntry::InterfaceMethodReference { .. })
                                );
                                let refusal = if jit_bridge::invoke_site_classifier_enabled() {
                                    match jit_bridge::classify_invoke_site(
                                        shared,
                                        &cm_lock,
                                        class_id,
                                        target_class,
                                        is_iface,
                                        method_name_ref,
                                        descriptor_ref,
                                    ) {
                                        jit_bridge::SiteBinding::ConstantPoolClass => None,
                                        // Interpreter round i7 wave 137, lane L2
                                        // (page `interpreter-i137-L2-eager-first-call-door-binds-a-substituted-special-owner-by-name-FIXED-20261021`):
                                        // this door carries the owner by NAME only
                                        // (the dispatch info's class-id hint is the
                                        // caller; `owner_class_id` stays 0), as the
                                        // single-pass OSR door did before wave 135.
                                        // Bound only when the caller's loader names
                                        // exactly the selected class; a child-first
                                        // loader's same-named shadow of the ancestor
                                        // keeps the method interpreted, where the
                                        // interpreter selects by id
                                        // (`CRATONVM_INVOKESPECIAL_OWNER_BY_ID`).
                                        jit_bridge::SiteBinding::Owner(owner, owner_id)
                                            if constants::class_resolved_without_loading(
                                                shared, &cm_lock, class_id, &owner,
                                            ) != Some(owner_id) =>
                                        {
                                            Some("invokespecial-selection-start")
                                        }
                                        jit_bridge::SiteBinding::Owner(owner, _) => {
                                            if crate::runtime::env_cache::dbg_jitc() {
                                                eprintln!(
                                                    "[cratonvm-jitc] invokespecial-selection SUBSTITUTED \
                                                     eager-first-call {class_name_str}.{method_name}{method_descriptor}: \
                                                     {target_class}.{method_name_ref}{descriptor_ref} binds {owner}"
                                                );
                                            }
                                            substituted_owner = Some(owner);
                                            None
                                        }
                                        jit_bridge::SiteBinding::Refused(why) => Some(why),
                                    }
                                } else {
                                    jit_bridge::special_site_bind_refusal(
                                        shared,
                                        &cm_lock,
                                        class_id,
                                        target_class,
                                        is_iface,
                                        method_name_ref,
                                        descriptor_ref,
                                        true,
                                    )
                                };
                                if let Some(why) = refusal {
                                    jit_bridge::dbg_special_site_refusal(
                                        "eager-first-call",
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                        (target_class, method_name_ref, descriptor_ref),
                                        why,
                                    );
                                    return None;
                                }
                            }
                            let target_class: &str =
                                substituted_owner.as_deref().unwrap_or(target_class);
                            let is_recursive_call = target_class == &*class_name_str
                                && method_name_ref == method_name
                                && descriptor_ref == method_descriptor;
                            // The tail-shaped static self-call
                            // (`invokestatic_self_call_uses_tail_jump`) and
                            // every other one take the same raw route here.
                            //
                            // BUG-1 companion (eager first-call compile parity
                            // with `jit::try_compile`'s routing): NON-tail
                            // static self-recursive sites stay on the raw
                            // direct-CALL path — the backend emits the cheap
                            // `self_call_stack_guard` call (always wired via
                            // `build_helpers`) instead of the full
                            // `jit_invoke_dispatch` round trip. `scan.needs_heap`
                            // is already true (every invoke op sets it), so the
                            // guard's vm_ptr frame slot exists.
                            if invoke_kind == 3 && is_recursive_call {
                                // t1 wave 3, lane X: the positive self-call
                                // marker the emitter requires under
                                // `CRATONVM_JIT_STATIC_SELF_CALL_MARKER`.
                                direct_calls_early.push((
                                    pc,
                                    cratonvm_jit::static_self_call_marker(
                                        param_count,
                                        crate::jit::return_type(descriptor_ref),
                                    ),
                                ));
                                // The callee-deopt service's info for the raw
                                // self-call, as the method-entry door stages it
                                // (`stage_self_call_invoke_info`) and the OSR
                                // door since t1 wave 3. Without it an inner
                                // activation's throw skipped that activation's
                                // own handler and was resolved at the outermost
                                // compiled frame
                                // (`regression-suite/probes/R10SelfRecCatch.java`'s
                                // shape, reachable here under
                                // `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`). On its
                                // own channel, NOT in `invoke_info`: a row there
                                // would route the site to the dispatch helper.
                                if eager_self_call_invoke_info == 0 {
                                    let class_ref = cratonvm_jit::intern_jit_invoke_str(&class_name_str.to_string());
                                    let method_ref = cratonvm_jit::intern_jit_invoke_str(&method_name.to_string());
                                    let desc_ref = cratonvm_jit::intern_jit_invoke_str(&method_descriptor.to_string());
                                    // SAFETY: class_ref, method_ref, desc_ref point into the boxed strs that were just pushed to owned_jit_strings, which outlives the JitInvokeInfo.
                                    let info = Box::new(crate::jit::JitInvokeInfo {
                                        class_name: class_ref,
                                        method_name: method_ref,
                                        descriptor: desc_ref,
                                        // Receiver-INCLUDED; a static callee has none.
                                        num_jit_args: param_count,
                                        return_type: crate::jit::return_type(descriptor_ref),
                                        invoke_kind: 3,
                                        declaring_class_id: class_id.as_u32(),
                                        owner_class_id: 0,
                                    });
                                    // Cast: the arena address, as `BackendRequest` carries it.
                                    eager_self_call_invoke_info =
                                        &*info as *const crate::jit::JitInvokeInfo as usize;
                                    owned_jit_invoke_infos.push(info);
                                }
                                continue;
                            }
                            // Math.sqrt intrinsic: inline as SQRTSD (no dispatch overhead)
                            if invoke_kind == 3
                                && target_class == "java/lang/Math"
                                && method_name_ref == "sqrt"
                                && descriptor_ref == "(D)D"
                            {
                                direct_calls_early.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry: crate::jit::MATH_SQRT_INTRINSIC,
                                        needs_context: false,
                                        num_params: 1,
                                        return_type: b'D',
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                            // `Integer.valueOf(I)` thin direct call — statically
                            // bound NATIVE callee, so the eager callee compile can
                            // never succeed and the generic dispatch round trip is
                            // pure fixed overhead on the hottest autoboxing path.
                            // Parity with the recognition in `jit::try_compile`;
                            // see `jit::helpers::jit_integer_value_of_direct`.
                            if invoke_kind == 3
                                && target_class == "java/lang/Integer"
                                && method_name_ref == "valueOf"
                                && descriptor_ref == "(I)Ljava/lang/Integer;"
                            {
                                // `H12-1` O2 (2026-09-22). This ladder used to take
                                // the address raw, and `H12-1` §2c said why that was
                                // a defect even though both of its binds are
                                // `intrinsic` and therefore correct: it is unguarded,
                                // not safe, so the next helper added here would be a
                                // live defect on the day it was added with nothing to
                                // say so. `admit_direct_native_entry` keeps the
                                // registration-order property the old comment was
                                // about -- it takes the address of the item, never a
                                // `*_DIRECT_FN` cell this door may outrun -- and adds
                                // the question.
                                //
                                // A `None` FALLS THROUGH to the `invoke_info` build
                                // below; it must not `continue`, or the pc is left
                                // with no metadata and the backend reads it as a raw
                                // self-call.
                                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                                    shared,
                                    &admission,
                                    crate::jit::helpers::DirectNativeShadow::IntegerValueOf,
                                ) {
                                    direct_calls_early.push((
                                        pc,
                                        crate::jit::JitDirectCall {
                                            entry,
                                            needs_context: true,
                                            num_params: 1,
                                            return_type: b'L',
                                            guard_class_id: 0,
                                        },
                                    ));
                                    continue;
                                }
                            }
                            if matches!(invoke_kind, 0 | 1)
                                && target_class == "java/lang/reflect/Constructor"
                                && method_name_ref == "newInstance"
                                && descriptor_ref == "([Ljava/lang/Object;)Ljava/lang/Object;"
                            {
                                direct_calls_early.push((
                                    pc,
                                    crate::jit::JitDirectCall {
                                        entry: crate::jit::helpers::jit_ctor_new_instance_direct
                                            as *const () as usize,
                                        needs_context: true,
                                        num_params: 1,
                                        return_type: b'L',
                                        guard_class_id: 0,
                                    },
                                ));
                                continue;
                            }
                            // `Integer.intValue()` thin direct call — `Integer`
                            // is `final`, so a site declared against it is
                            // statically monomorphic (guard-free); the helper
                            // handles the null-receiver NPE itself.
                            if invoke_kind == 0
                                && target_class == "java/lang/Integer"
                                && method_name_ref == "intValue"
                                && descriptor_ref == "()I"
                            {
                                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                                    shared,
                                    &admission,
                                    crate::jit::helpers::DirectNativeShadow::IntegerIntValue,
                                ) {
                                    cratonvm_jit::note_integer_int_value_direct_site();
                                    direct_calls_early.push((
                                        pc,
                                        crate::jit::JitDirectCall {
                                            entry,
                                            needs_context: true,
                                            num_params: 0,
                                            return_type: b'I',
                                            guard_class_id: 0,
                                        },
                                    ));
                                    continue;
                                }
                            }
                            // `ThreadLocal.get()` thin direct call (SBR round 2,
                            // wave 8, lane `tlocal`): not final, so the helper
                            // screens the receiver's class itself. `--real-jdk`
                            // only; a `None` falls through to the `invoke_info`
                            // build below, as every bind at this door does.
                            if invoke_kind == 0
                                && (target_class == "java/lang/ThreadLocal"
                                    || target_class == "java/lang/InheritableThreadLocal")
                                && method_name_ref == "get"
                                && descriptor_ref == "()Ljava/lang/Object;"
                                && cratonvm_jit::thread_local_get_direct_enabled()
                                && crate::vm::dispatch_policy(shared).is_jdk_only()
                            {
                                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                                    shared,
                                    &admission,
                                    crate::jit::helpers::DirectNativeShadow::ThreadLocalGet,
                                ) {
                                    cratonvm_jit::note_thread_local_get_direct_site();
                                    direct_calls_early.push((
                                        pc,
                                        crate::jit::JitDirectCall {
                                            entry,
                                            needs_context: true,
                                            num_params: 0,
                                            return_type: b'L',
                                            guard_class_id: 0,
                                        },
                                    ));
                                    continue;
                                }
                            }
                            // `ThreadLocal.set(Object)`, the twin of the `get()` bind above.
                            if invoke_kind == 0
                                && (target_class == "java/lang/ThreadLocal"
                                    || target_class == "java/lang/InheritableThreadLocal")
                                && method_name_ref == "set"
                                && descriptor_ref == "(Ljava/lang/Object;)V"
                                && cratonvm_jit::thread_local_get_direct_enabled()
                                && crate::vm::dispatch_policy(shared).is_jdk_only()
                            {
                                if let Some(entry) = crate::jit::helpers::admit_direct_native_entry(
                                    shared,
                                    &admission,
                                    crate::jit::helpers::DirectNativeShadow::ThreadLocalSet,
                                ) {
                                    cratonvm_jit::note_thread_local_get_direct_site();
                                    direct_calls_early.push((
                                        pc,
                                        crate::jit::JitDirectCall {
                                            entry,
                                            needs_context: true,
                                            num_params: 1,
                                            return_type: b'V',
                                            guard_class_id: 0,
                                        },
                                    ));
                                    continue;
                                }
                            }
                            // Defer trivial `<init>()V` sites for after-lock
                            // elidability resolution (see the block + pending
                            // list comments above).
                            if invoke_kind == 1
                                && method_name_ref == "<init>"
                                && descriptor_ref == "()V"
                                && !ctor_direct_call_off
                            {
                                pending_ctor_sites.push((
                                    pc,
                                    target_class.to_string(),
                                    param_count,
                                ));
                                continue;
                            }
                            let num_jit_args = if invoke_kind == 3 {
                                param_count
                            } else {
                                param_count + 1
                            }; // +1 for receiver
                            let return_type = crate::jit::return_type(descriptor_ref);
                            // Store strings in owned vec; take raw pointers for JIT metadata
                            let class_ref = cratonvm_jit::intern_jit_invoke_str(&target_class.to_string());
                            let method_ref = cratonvm_jit::intern_jit_invoke_str(&method_name_ref.to_string());
                            let desc_ref = cratonvm_jit::intern_jit_invoke_str(&descriptor_ref.to_string());
                            // SAFETY: class_ref, method_ref, desc_ref point into the boxed strs that were just pushed to owned_jit_strings, which outlives the JitInvokeInfo.
                            let info = Box::new(crate::jit::JitInvokeInfo {
                                class_name: class_ref,
                                method_name: method_ref,
                                descriptor: desc_ref,
                                num_jit_args,
                                return_type,
                                invoke_kind,
                                declaring_class_id: class_id.as_u32(),
                                owner_class_id: 0,
                            });
                            let info_ptr: *const _ = &*info;
                            owned_jit_invoke_infos.push(info);
                            invoke_info.push((pc, info_ptr));
                            allocate_dynamic_dispatch_slots(
                                invoke_kind,
                                pc,
                                &mut mic_slots_early,
                                &mut owned_mic_slots_early,
                                &mut pic_slots_early,
                                &mut owned_pic_slots_early,
                            );
                        }
                    }
                    // Resolve the deferred trivial-ctor sites now that `cm_lock`
                    // is released. For each `invokespecial C.<init>()V`, resolve
                    // `C` (already loaded — `load_class_concurrent` returns the
                    // cached id without side effects) and check
                    // `is_elidable_construction`. If elidable, emit the site AS
                    // `java/lang/Object.<init>` so the codegen's fix-2 elision
                    // drops the per-object dispatch; otherwise emit the real
                    // dispatch `JitInvokeInfo`. (Soundness: an elidable `C.<init>`
                    // writes no field and only calls the no-op `Object.<init>`,
                    // so eliding it — or dispatching `Object.<init>` on the
                    // freshly zeroed object if the elision somehow did not fire —
                    // is identical to running `C.<init>`.)
                    let dbg_ctor = None::<std::ffi::OsString>.is_some();
                    // Pcs whose `<init>()V` target `is_elidable_construction` PROVED empty. The
                    // backend may elide only these; a no-arg constructor that is NOT proven empty
                    // keeps both its allocation and its call, because eliding it would drop
                    // whatever the body writes to global state (see
                    // docs/feature-designs/jit-ir-optimizations.md).
                    let mut elidable_init_pcs: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    for (pc, tclass, pcount) in pending_ctor_sites {
                        // Through this class's own loader, as the OSR door
                        // asks (interpreter round i1 wave 38, lane L2): the
                        // loader-blind `load_class_concurrent` judged the
                        // class path's same-named constructor for a user
                        // loader's site (`L2W38EagerDoorLoaderNew`).
                        let elidable = jit_bridge::resolve_cp_class_for_owner(
                            shared, class_id, &tclass,
                        )
                        .map(|tid| {
                            let cm2 = shared.classes.class_manager.read();
                            is_elidable_construction(shared, &cm2, tid)
                        })
                        .unwrap_or(false);
                        if elidable {
                            elidable_init_pcs.insert(pc);
                        }
                        if dbg_ctor {
                            eprintln!(
                                "[ctor-fix] {}.{}{} ctor site pc={} target={} elidable={}",
                                &*class_name_str,
                                method_name,
                                method_descriptor,
                                pc,
                                tclass,
                                elidable,
                            );
                        }
                        let info_class: &str = if elidable {
                            "java/lang/Object"
                        } else {
                            &tclass
                        };
                        let class_ref = cratonvm_jit::intern_jit_invoke_str(&info_class.to_string());
                        let method_ref = cratonvm_jit::intern_jit_invoke_str(&"<init>".to_string());
                        let desc_ref = cratonvm_jit::intern_jit_invoke_str(&"()V".to_string());
                        // SAFETY: refs point into the boxed strs just pushed to owned_jit_strings,
                        // which outlives the JitInvokeInfo (same lifetime contract as the loop above).
                        let info = Box::new(crate::jit::JitInvokeInfo {
                            class_name: class_ref,
                            method_name: method_ref,
                            descriptor: desc_ref,
                            num_jit_args: pcount + 1, // receiver + params
                            return_type: b'V',
                            invoke_kind: 1,
                            declaring_class_id: class_id.as_u32(),
                            owner_class_id: 0,
                        });
                        let info_ptr: *const _ = &*info;
                        owned_jit_invoke_infos.push(info);
                        invoke_info.push((pc, info_ptr));
                    }
                    // Resolve new/anewarray info (Phase 39: correct ClassId + field count for JIT new)
                    // Only resolve for non-synthetic classes (real JDK bytecode) to avoid
                    // expensive class loading cascades during JIT of synthetic code.
                    // Tuple: (pc, class_id, num_fields,
                    // has_nonzero_tag_primitive_init, has_finalizer).
                    // The last two are conservative true/true here so the JIT goes through
                    // the post-init helper — matches pre-CRIT-2 behavior. A follow-up should
                    // extract the real flags from class metadata to enable the skip path.
                    let mut new_info: Vec<(usize, u32, usize, bool, bool)> = Vec::new();
                    let mut anewarray_info: Vec<(usize, u32)> = Vec::new();
                    // Cold-`new` fix - sites this path cannot resolve at compile
                    // time. Previously baked as the nonsense sentinel entry
                    // `(pc, class_id 0, 0 fields, true, true)`, which did NOT
                    // "make the JIT skip this site and defer to the interpreter"
                    // as the comment below claimed: the codegen found an entry at
                    // that pc and compiled an allocation against class id 0. They
                    // now compile to the CP-indexed helper, which performs the
                    // real loader-faithful resolution, the JVMS 5.4.4 access
                    // check and `<clinit>` at the actual program point - the same
                    // work `Instruction::New` does.
                    let mut new_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
                    let mut anewarray_deferred_info: Vec<(usize, u32, u16)> = Vec::new();
                    let is_real_class = shared
                        .classes.class_manager
                        .read()
                        .get_class(class_id)
                        .map(|c| !c.origin.is_compatibility_stub())
                        .unwrap_or(false);
                    if is_real_class && (!scan.new_ops.is_empty() || !scan.anewarray_ops.is_empty())
                    {
                        // Collect class names from constant pool (read lock)
                        let new_class_names: Vec<(usize, u16, Option<String>)> = {
                            let cm_lock = shared.classes.class_manager.read();
                            if let Some(class) = cm_lock.get_class(class_id) {
                                scan.new_ops
                                    .iter()
                                    .map(|&(pc_new, cp_idx)| {
                                        (
                                            pc_new,
                                            cp_idx,
                                            class
                                                .constant_pool
                                                .get_class_name(cp_idx)
                                                .map(|s| s.to_string()),
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        let arr_class_names: Vec<(usize, u16, Option<String>)> = {
                            let cm_lock = shared.classes.class_manager.read();
                            if let Some(class) = cm_lock.get_class(class_id) {
                                scan.anewarray_ops
                                    .iter()
                                    .map(|&(pc_arr, cp_idx)| {
                                        (
                                            pc_arr,
                                            cp_idx,
                                            class
                                                .constant_pool
                                                .get_class_name(cp_idx)
                                                .map(|s| s.to_string()),
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        // Resolve class names to ClassIds (write lock for loading).
                        // Through this class's own loader, the OSR door's rule
                        // (`resolve_cp_class_for_owner`; interpreter round i1
                        // wave 38, lane L2): the loader-blind
                        // `load_class_concurrent` bound the class path's
                        // same-named class into a user loader's method
                        // (`L2W38EagerDoorLoaderNew`). A `None` defers the site.
                        for (pc_new, cp_idx_new, name_opt) in new_class_names {
                            if let Some(name) = name_opt {
                                let load_result =
                                    jit_bridge::resolve_cp_class_for_owner(shared, class_id, &name);
                                if let Some(target_id) = load_result {
                                    // JVMS 5.4.4 / 6.5 `new`: don't bake an
                                    // inlined fast-path allocation for a `new`
                                    // site the accessor is not permitted to
                                    // reach (see the matching check added to
                                    // `Instruction::New` above). Falling back
                                    // to the sentinel entry -- exactly what
                                    // the load-failure arm below already does
                                    // -- makes the JIT skip this site and
                                    // defer to the interpreter, which performs
                                    // the real access check and throws
                                    // `IllegalAccessError`. Without this, a
                                    // method that gets tiered up to JIT before
                                    // its first *interpreted* execution could
                                    // silently bypass access control.
                                    // The interpreter's own verdict (i10-L2),
                                    // so the two cannot drift: it admits a
                                    // missing class rather than inventing a
                                    // denial, and it carries HotSpot's
                                    // serialization-accessor exemption the
                                    // raw `check_class_access` lacked.
                                    let accessible = constants::class_constant_fill_admitted(
                                        shared, class_id, &name, target_id,
                                    );
                                    // Pending initialization also defers: the
                                    // CP-indexed helper runs `<clinit>`, a
                                    // resolved row allocates with no init check
                                    // (wave 4, L7; see
                                    // `jit_bridge::jit_new_target_init_pending`),
                                    // and so does an unlinkable one (wave 8, L5).
                                    let init_pending =
                                        jit_bridge::jit_new_target_pending_after_link(
                                            shared, target_id,
                                        );
                                    if accessible && !init_pending {
                                        // The real flags, as the method-entry
                                        // and OSR doors bake them (JIT round 15
                                        // wave 5, lane osrtier: the round-14
                                        // "measured slower" predates the inline
                                        // TLAB bump); the OSR door's switch
                                        // `CRATONVM_JIT_OSR_REAL_NEW_SITE_FLAGS=0`
                                        // restores the literal `(true, true)` row.
                                        let (num_fields, needs_post_alloc_init, has_finalizer) = {
                                            let cm = shared.classes.class_manager.read();
                                            if jit_bridge::osr_real_new_site_flags_enabled() {
                                                jit_bridge::jit_new_site_flags(&cm, target_id)
                                            } else {
                                                (
                                                    cm.get_class(target_id)
                                                        .map(|c| c.num_total_fields)
                                                        .unwrap_or(0),
                                                    true,
                                                    true,
                                                )
                                            }
                                        };
                                        new_info.push((
                                            pc_new,
                                            target_id.as_u32(),
                                            num_fields,
                                            needs_post_alloc_init,
                                            has_finalizer,
                                        ));
                                    } else {
                                        new_deferred_info
                                            .push((pc_new, class_id.as_u32(), cp_idx_new));
                                    }
                                } else {
                                    new_deferred_info.push((pc_new, class_id.as_u32(), cp_idx_new));
                                }
                            }
                        }
                        for (pc_arr, cp_idx_arr, name_opt) in arr_class_names {
                            if let Some(name) = name_opt {
                                // An inaccessible component is deferred to the
                                // CP helper, which raises the `--real-jdk`
                                // IllegalAccessError (i9-L2).
                                if let Some(target_id) =
                                    jit_bridge::resolve_cp_class_for_owner(shared, class_id, &name)
                                        .filter(|&id| {
                                            constants::class_constant_fill_admitted(
                                                shared, class_id, &name, id,
                                            )
                                        })
                                {
                                    anewarray_info.push((pc_arr, target_id.as_u32()));
                                } else {
                                    anewarray_deferred_info
                                        .push((pc_arr, class_id.as_u32(), cp_idx_arr));
                                }
                            }
                        }
                    }

                    // invokedynamic-uncommon-trap fix: resolve each `indy_ops`
                    // site's target descriptor to the minimal stack-effect info
                    // the codegen needs (arg slot count + return type tag) — see
                    // the `indy_info` field doc on the x64 `Compiler` struct. A
                    // site that cannot be resolved (class not loaded, malformed
                    // pool) is simply omitted; the x64 codegen's 0xba arm then
                    // bails the whole compile (`return false`) rather than
                    // guessing, exactly like the OSR/hot-path resolvers.
                    let mut indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)> = Vec::new();
                    if !scan.indy_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_indy, cp_idx) in &scan.indy_ops {
                                if let Some(ConstantPoolEntry::InvokeDynamic {
                                    name_and_type_index,
                                    ..
                                }) = class.constant_pool.get(cp_idx)
                                {
                                    if let Some((_, descriptor)) =
                                        class.constant_pool.get_name_and_type(*name_and_type_index)
                                    {
                                        let arg_slots = crate::jit::count_param_slots(descriptor);
                                        let ret_type = crate::jit::return_type(descriptor);
                                        let arg_type_tags =
                                            crate::jit::indy_arg_type_tags(descriptor);
                                        let bridge_site = jit_bridge::interned_jit_indy_bridge_site(
                                            shared,
                                            &cm_lock,
                                            class,
                                            class_id,
                                            cp_idx,
                                            Some((method_name, method_descriptor, pc_indy)),
                                        );
                                        indy_info.push((
                                            pc_indy,
                                            arg_slots,
                                            ret_type,
                                            arg_type_tags,
                                            bridge_site,
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    // Resolve instance field info for getfield/putfield (M5 fix)
                    let mut field_info: Vec<(usize, usize, u8)> = Vec::new();
                    // Compact reference-field layout: per-pc packed offset + ref-ness
                    // so the single-pass codegen emits inline compact field access.
                    let mut compact_field_info: Vec<(usize, u32, bool)> = Vec::new();
                    // Volatile getfield/putfield pcs: the backend fences each
                    // volatile store (`BackendRequest::volatile_field_pcs`).
                    // See `volatile-instance-fields-are-plain-accesses-in-compiled-code`.
                    let mut early_volatile_field_pcs: std::collections::HashSet<usize> =
                        std::collections::HashSet::new();
                    let compact_fields = cratonvm_types::compact_ref_fields_enabled();
                    if !scan.field_ops.is_empty() {
                        for &(pc_f, cp_idx) in &scan.field_ops {
                            if let Ok(field) = resolve_field_ref(shared, class_id, cp_idx) {
                                // JIT round 15 wave 4 (lane fieldowner): no row
                                // for a site whose owner the interpreter resolves
                                // through the loader to another class
                                // (`jit_bridge::jit_field_owner_refusal`); the
                                // backend refuses a site with no row.
                                if jit_bridge::jit_field_owner_refusal(shared, class_id, cp_idx, &field)
                                    .is_some()
                                {
                                    continue;
                                }
                                // JVMS §6.5 linking ICCE: a `getfield` of a
                                // static field gets no row, so the site stays
                                // unresolved and the interpreter throws. So
                                // does an illegal final-field `putfield`
                                // (`IllegalAccessError`).
                                if field.is_static
                                    || (padded.get(pc_f).copied() == Some(0xb5)
                                        && final_field_put_illegal(
                                            shared,
                                            class_id,
                                            method_name,
                                            &field,
                                        ))
                                {
                                    continue;
                                }
                                if field.is_volatile {
                                    early_volatile_field_pcs.insert(pc_f);
                                }
                                let cm_lock = shared.classes.class_manager.read();
                                if let Some(class) = cm_lock.get_class(class_id) {
                                    let nat_idx = match class.constant_pool.get(cp_idx) {
                                        Some(ConstantPoolEntry::FieldReference {
                                            name_and_type_index,
                                            ..
                                        }) => *name_and_type_index,
                                        _ => continue,
                                    };
                                    if let Some((_, descriptor)) =
                                        class.constant_pool.get_name_and_type(nat_idx)
                                    {
                                        // No row for an empty descriptor (it
                                        // used to default to `I`) or for a slot
                                        // whose own descriptor disagrees with
                                        // the pool's: the backend refuses a
                                        // site with no row, as the OSR door and
                                        // the `CpResolvers` refuse it
                                        // (interpreter round i1 wave 38, lane
                                        // L2; item 9 of the wave-37 review).
                                        let Some(&type_tag) = descriptor.as_bytes().first() else {
                                            continue;
                                        };
                                        if !jit_bridge::jit_field_tag_agrees(&field, type_tag, cp_idx)
                                        {
                                            if crate::runtime::env_cache::dbg_jitc() {
                                                eprintln!(
                                                    "[cratonvm-jitc] eager-first-call field-tag \
                                                     refusal {class_name_str}.{method_name}\
                                                     {method_descriptor} pc={pc_f} instance \
                                                     cp={cp_idx}"
                                                );
                                            }
                                            continue;
                                        }
                                        field_info.push((pc_f, field.field_index, type_tag));
                                        if compact_fields {
                                            if let Some((c_off, c_ref)) =
                                                cratonvm_types::compact_field_disp(
                                                    field.declaring_class_id.as_u32(),
                                                    field.field_index,
                                                )
                                            {
                                                compact_field_info.push((
                                                    pc_f,
                                                    c_off as u32,
                                                    c_ref,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Resolve ldc/ldc_w constants (int, float).
                    // Skip early compilation for methods with String/Class ldc —
                    // those will be handled by OSR which can wire callees as direct calls.
                    let mut ldc_info_early: Vec<(usize, i64)> = Vec::new();
                    let mut ldc_string_info_early: Vec<(usize, u32, u16)> = Vec::new();
                    let mut ldc_class_info_early: Vec<(usize, u32, u16)> = Vec::new();
                    // The `ldc`-family pcs whose constant is floating-point.
                    // Codegen types these by their consuming opcode, but the
                    // deopt operand-stack snapshot has no consuming opcode to
                    // ask — see `x64::Compiler::ldc_fp_pcs`.
                    let mut ldc_fp_pcs_early: rustc_hash::FxHashSet<usize> =
                        rustc_hash::FxHashSet::default();
                    let mut has_unsupported_ldc = false;
                    if !scan.ldc_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_ldc, cp_idx) in &scan.ldc_ops {
                                match class.constant_pool.get(cp_idx) {
                                    Some(ConstantPoolEntry::Integer(v)) => {
                                        // Widening: i32 -> i64 (sign-extended, JVM i2l)
                                        ldc_info_early.push((pc_ldc, *v as i64));
                                        // Cast: JIT ABI — i64 register convention
                                    }
                                    Some(ConstantPoolEntry::Float(v)) => {
                                        ldc_info_early.push((pc_ldc, v.to_bits() as i64));
                                        // Cast: JIT ABI -- float bits to i64
                                        ldc_fp_pcs_early.insert(pc_ldc);
                                    }
                                    Some(ConstantPoolEntry::StringReference { string_index })
                                        if class
                                            .constant_pool
                                            .get_utf8_wide(*string_index)
                                            .is_none() =>
                                    {
                                        // Wired 2026-07-18, mirroring the OSR-artifact
                                        // path. Before this, ANY method with a string
                                        // constant went into `jit_skip_set` here, which
                                        // also blocked the hot-path `jit::try_compile` -
                                        // OSR artifacts were such methods' ONLY compiled
                                        // form.
                                        //
                                        // The SITE is recorded, not the text: since
                                        // 2026-08-20 codegen materialises through
                                        // `helpers.ldc_string_cp`, which answers from
                                        // the `(class, cp index)`-keyed record JVMS
                                        // §5.4.3 requires. The `get_utf8` call is only
                                        // the representability test the `get_utf8_wide`
                                        // guard above pairs with.
                                        match class.constant_pool.get_utf8(*string_index) {
                                            Some(_) => {
                                                ldc_string_info_early
                                                    .push((pc_ldc, class_id.as_u32(), cp_idx));
                                            }
                                            None => has_unsupported_ldc = true,
                                        }
                                    }
                                    // `ldc <Class>`: recorded as a site and
                                    // served at run time by
                                    // `helpers.ldc_class_cp`. Recorded even if
                                    // that helper turns out to be unwired — the
                                    // backend then refuses the method, which is
                                    // a bail, whereas `has_unsupported_ldc`
                                    // additionally inserts it into
                                    // `jit_skip_set` and so would block the
                                    // hot-path compile too.
                                    Some(ConstantPoolEntry::ClassReference { .. }) => {
                                        ldc_class_info_early.push((
                                            pc_ldc,
                                            class_id.as_u32(),
                                            cp_idx,
                                        ));
                                    }
                                    // Wide-string / MethodHandle / condy ldc
                                    // kinds - still unwired on this path (as on
                                    // the OSR path).
                                    _ => {
                                        has_unsupported_ldc = true;
                                    }
                                }
                            }
                        }
                    }
                    // Unsupported ldc kinds (wide-string/Class/...) cannot be
                    // early-compiled; OSR handles such methods later.
                    if has_unsupported_ldc
                    {
                        // Round 13 wave 1 (lane replay,
                        // `r12w3-tier-eager-first-call-front-end-not-yet-equivalent`):
                        // a constant kind THIS front end does not wire is not a
                        // verdict on the method, and the seal below kept it off
                        // every door that reads `jit_skip_set` for good. Hand it
                        // to the ordinary door, as a method with handlers is
                        // handed above; its refusal is sealed after this closure
                        // (`early-backend-bail`) like any other. This door's
                        // admission is closed first: that door opens its own.
                        drop(admission);
                        if is_static
                            && !shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(class_id)
                                .is_some_and(crate::vm::is_class_initialized_fast)
                        {
                            eager_deferred = true;
                            return None;
                        }
                        let mut policy_declined = false;
                        return match cratonvm_jit::tiered::contain_compile_panic(|| {
                            jit_bridge::try_jit_compile_wrapped_entry(
                                shared,
                                class_id,
                                &class_name_str,
                                method_name,
                                method_descriptor,
                                false, // this door's tier: single-pass
                                &mut policy_declined,
                            )
                        }) {
                            Ok(Some((body, _entry, _needs_ctx))) => Some(body),
                            Ok(None) => {
                                // Wave 41 (lane L2): a publication the
                                // redefinition hold refused is not sealed.
                                if shared.jit.jit_cache.publications_held() {
                                    eager_deferred = true;
                                }
                                None
                            }
                            Err(_) => None,
                        };
                    }
                    if has_unsupported_ldc {
                        // Mark as skipped so we don't retry
                        note_jit_skip_seal("early-unsupported-ldc", &skip_key);
                        shared.jit.jit_skip_set.write().insert(skip_key.clone());
                    }

                    // Resolve ldc2_w constants (long, double)
                    let mut ldc2w_info_early: Vec<(usize, i64)> = Vec::new();
                    if !scan.ldc2w_ops.is_empty() {
                        let cm_lock = shared.classes.class_manager.read();
                        if let Some(class) = cm_lock.get_class(class_id) {
                            for &(pc_ldc, cp_idx) in &scan.ldc2w_ops {
                                let val = match class.constant_pool.get(cp_idx) {
                                    Some(ConstantPoolEntry::Long(v)) => v.get(),
                                    Some(ConstantPoolEntry::Double(v)) => {
                                        ldc_fp_pcs_early.insert(pc_ldc);
                                        v.to_bits() as i64 // Cast: JIT ABI -- float bits to i64
                                    }
                                    _ => continue,
                                };
                                ldc2w_info_early.push((pc_ldc, val));
                            }
                        }
                    }

                    // Unsupported ldc kind present — fall through to the interpreter.
                    if has_unsupported_ldc {
                        return None;
                    }

                    // Category-2 (long/double) PARAMETER guard.
                    //
                    // This early-compile path passes `param_slots = args.len()` to the
                    // bare `x64::compile`, which lays parameters out sequentially by
                    // argument index (one JVM slot each). That is wrong whenever a
                    // parameter is a long/double: those occupy TWO JVM local slots, so
                    // every parameter after the first category-2 one is read from the
                    // wrong (un-populated) slot. Concrete symptom: a `(long,long)long`
                    // lambda body (`lload_0; lload_2; ladd`) read its 2nd argument from
                    // the empty `locals[2]`, so e.g. `Long::sum(100,23)` returned 100.
                    //
                    // The hot-path `jit::try_compile` handles this correctly via
                    // `compute_param_jvm_slots` + `compile_with_param_slots`. Bail here
                    // (fall through to the interpreter); the method still JIT-compiles
                    // later through the correct path once it goes hot.
                    //
                    // That last promise did not hold: this `None` reaches the
                    // `early-backend-bail` seal after the closure, and the seal
                    // shuts `execute()`'s own cache consult for the method. So
                    // since round 12 wave 2 such a method is handed to
                    // `jit::try_compile` before the admission above (see the
                    // ordinary-door route there); this guard is reached only
                    // under `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`.
                    if crate::jit::count_param_slots_jvm_spec(method_descriptor)
                        != crate::jit::count_param_slots(method_descriptor)
                    {
                        return None;
                    }

                    // Try to compile
                    let param_slots = args.len();
                    let helpers = crate::jit::helpers::build_helpers_for(shared);
                    // HIB-CV-20 — like the OSR path (and unlike the legacy
                    // `x64::compile` wrapper, which hardcodes `param_oop_mask = 0`),
                    // seed the local-oop dataflow with this method's reference
                    // PARAMETERS so an oop param living in a callee-saved register
                    // across an early safepoint is reloaded after a moving GC
                    // (`emit_post_safepoint_reload`). Without it the register stays
                    // stale → heap corruption. The category-2 slot-layout guard
                    // above guarantees "arg index == slot", so the descriptor-derived
                    // mask lines up with the `&[]`/`0` legacy layout. Gate on the
                    // precise-maps flag so the gate-off path stays byte-identical.
                    let early_is_static = shared
                        .classes.class_manager
                        .read()
                        .get_class(class_id)
                        .and_then(|class| {
                            class
                                .methods
                                .iter()
                                .find(|m| {
                                    &*m.name == method_name && &*m.descriptor == method_descriptor
                                })
                                .map(|m| m.is_static())
                        })
                        .unwrap_or(false);
                    // Also seed under `deopt_real_enabled()` — see the matching
                    // comment at the hot-path `jit::try_compile` seed site
                    // (jit/src/lib.rs) for why an unseeded mask makes every
                    // deopt at this compile silently double-execute side
                    // effects via the whole-method re-run fallback.
                    let param_oop_mask = if crate::jit::x64::precise_jit_maps_enabled()
                        || crate::jit::x64::moving_young_enabled()
                        || cratonvm_jit::deopt_real_enabled()
                    {
                        crate::jit::compute_param_oop_mask(method_descriptor, early_is_static)
                    } else {
                        0
                    };
                    // Read once: it arms both `debugger_observes_locals` and
                    // the parameter kinds of the backend request below.
                    let early_debugger_observes_locals =
                        crate::runtime::jvmti::debugger_observes_locals(shared);
                    // -- The String-intrinsic pin, ASKED at this door -- D1, 2026-09-01
                    //
                    // The third door. The measurement that found the pin
                    // installed at ONE of three, and why the answer is inert at
                    // the two that reach the single-pass backend directly, is
                    // written out at the OSR door (`jit_bridge.rs`, at
                    // `osr_string_pin_declines`); the topology is in
                    // `compile_gate`'s "installed at ONE door" section.
                    //
                    // This door passes `string_layout: None` below -- "String
                    // intrinsics land in a later wave" -- so the pin's honest
                    // verdict here is `BlindNoLayout`: a site declared on a
                    // String-family receiver, no layout resolved at this door,
                    // and the rule FAILS OPEN (`false`, do not pin). That is
                    // not a shortcut. Without a layout the single-pass backend
                    // emits no intrinsic either, so pinning would cost the
                    // method a C2 body and buy nothing back. The `None` is
                    // passed deliberately rather than papered over: the whole
                    // gain is that `blind-no-layout` becomes a MEASURED number
                    // for this door instead of an invisible absence.
                    //
                    // Consequence, so nobody reads more into the counter than
                    // it carries: with `layout == None` the pin can only answer
                    // `false` here. `NoSite` and `BlindNoLayout` are `false` by
                    // rule, and the `BlindNoResolver` fail-closed arm requires
                    // `layout.is_some()`. So this ask changes no machine code
                    // today; it is a census entry, not a decision. It becomes a
                    // real decision the moment the `string_layout: None`
                    // argument below becomes a resolved layout -- the `if` is
                    // the tripwire for exactly that.
                    //
                    // A resolver IS supplied even though the layout is not,
                    // because without one the verdict would be
                    // `BlindNoResolver` -- the STRONGER blindness, which says
                    // nothing about whether a layout would have mattered -- and
                    // the narrower fact is the whole reason to ask here.
                    //
                    // COST: once per eager first-call compile, off any loop.
                    // The resolver takes the `class_manager` read lock per site,
                    // matching `c_invoke_resolver` in `jit_bridge.rs` and the
                    // field-op loop above, which already takes it per field op;
                    // `string_intrinsic_pin_verdict` asks nothing at all for a
                    // method with no `invokevirtual`/`invokeinterface` site and
                    // stops at the first String-family receiver otherwise. No
                    // lock is held here -- the `early_is_static` probe above
                    // took and released its own.
                    let eager_pin_invoke_resolver =
                        |cp_idx: u16| -> Option<(String, String, String)> {
                            let cm = shared.classes.class_manager.read();
                            let class = cm.get_class(class_id)?;
                            let (class_idx, nat_idx) = match class.constant_pool.get(cp_idx) {
                                Some(ConstantPoolEntry::MethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                Some(ConstantPoolEntry::InterfaceMethodReference {
                                    class_index,
                                    name_and_type_index,
                                    ..
                                }) => (*class_index, *name_and_type_index),
                                _ => return None,
                            };
                            let target_class = class.constant_pool.get_class_name(class_idx)?;
                            let (mn, desc) = class.constant_pool.get_name_and_type(nat_idx)?;
                            Some((target_class.to_string(), mn.to_string(), desc.to_string()))
                        };
                    let eager_string_pin_declines = admission.string_intrinsic_pin_declines(
                        &scan.invoke_ops,
                        Some(&eager_pin_invoke_resolver),
                        // The SAME value handed to `compile_with_param_slots`
                        // below as its `string_layout` argument. Asking about a
                        // layout this compile does not use would make the
                        // census describe a compile that did not happen.
                        None,
                    );
                    if eager_string_pin_declines && crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jitc] eager-first-call String-intrinsic pin declines the \
                             optimizing tier for {class_name_arc}.{method_name_arc}{descriptor_arc} \
                             -- unreachable while this door passes `string_layout: None`. If this \
                             ever fires, the later wave landed and this ask is now a decision.",
                        );
                    }
                    // GC-maintained `ldc` constant slots (ldc7 request H, early
                    // half -- round 9 wave 8, vmrt8), mirroring the OSR door in
                    // `jit_bridge.rs::compile_osr_body`. Built HERE because the
                    // two site lists are moved into the call below. No
                    // class-manager guard is live at this point (every lock
                    // above is scoped to its own block, and the pin resolver
                    // only locks while it runs), which `compiled_ldc_slot_for`
                    // requires. A `0` answer (kill switch, redefinition,
                    // unresolved site) leaves that site on its helper CALL.
                    let mut early_ldc_slots: std::collections::HashMap<(u32, u16), usize> =
                        std::collections::HashMap::new();
                    for &(_, holder, cp_idx) in
                        ldc_string_info_early.iter().chain(ldc_class_info_early.iter())
                    {
                        let slot_addr = crate::jit::helpers::compiled_ldc_slot_for(
                            shared,
                            ClassId::new(holder),
                            cp_idx,
                        );
                        if slot_addr != 0 {
                            early_ldc_slots.insert((holder, cp_idx), slot_addr);
                        }
                    }
                    // 2026-10-07 (item 1 of
                    // `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`):
                    // what the artifact keeps of its calls once the tables
                    // below are attached to it, as the VM's synchronized
                    // routes read it (`jit_bridge::wrapped_body_calls_nothing`).
                    // Asked here because the tables move into the call.
                    let early_keeps_a_call_record = !owned_jit_invoke_infos.is_empty()
                        || !owned_mic_slots_early.is_empty()
                        || !owned_pic_slots_early.is_empty();
                    let Some(mut cm) = crate::jit::x64::compile_with_param_slots(
                        &admission,
                        &padded,
                        code_len,
                        param_slots,
                        code_attr.max_locals as usize, // Widening: u16 to usize
                        // A BRIDGED indy site calls a helper, which loads the
                        // hidden `SharedVm` pointer from the frame slot
                        // `heap_local_offset` names — and that slot only EXISTS
                        // when `needs_heap` is set. `scan.needs_heap` does not
                        // know about it, because an `invokedynamic` used to
                        // lower to a trap that calls nothing. See the same
                        // one-line remedy at the other two compile doors.
                        scan.needs_heap
                            || indy_info
                                .iter()
                                .any(|&(_, _, _, _, bridge_site)| bridge_site != 0),
                        mna_info,
                        field_info,
                        typecheck_info,
                        static_field_info,
                        new_info,
                        new_deferred_info,
                        anewarray_info,
                        anewarray_deferred_info,
                        invoke_info,
                        direct_calls_early,
                        mic_slots_early,
                        pic_slots_early,
                        ldc_info_early,
                        ldc_string_info_early, // wired 2026-07-18 — see the
                        // StringReference arm in the ldc resolver above.
                        ldc_class_info_early,
                        ldc2w_info_early,
                        ldc_fp_pcs_early,
                        std::collections::HashMap::new(), // branch_hints
                        std::collections::HashMap::new(), // loop_unroll_hints
                        &helpers,
                        scan.non_escaping_new.clone(), // escape analysis results
                        std::collections::HashMap::new(), // inline_sites
                        std::collections::HashMap::new(), // inline_guard_variants (PGO-02, no guarded plan from this scan-based fast path)
                        None, // string_layout — String intrinsics land in a later wave
                        &[],  // param_jvm_slots — legacy "arg index == slot" layout
                        0,    // param_slot_span — legacy layout
                        param_oop_mask,
                        compact_field_info,
                        // method_key — bakes this method's identity into its
                        // deopt snapshots (resume sinks verify it before
                        // resuming a stashed frame). Also enables the per-bci
                        // de-spec consult against this VM's registry.
                        &format!("{class_name_arc}.{method_name_arc}:{descriptor_arc}"),
                        Some(&shared.jit.despec_registry),
                        indy_info,
                        Some(elidable_init_pcs),
                        {
                            let mut request = crate::jit::x64::BackendRequest {
                                direct_helpers: crate::jit::helpers::direct_helper_table_for(shared),
                                volatile_field_pcs: early_volatile_field_pcs,
                                ldc_slots: early_ldc_slots,
                                // Interpreter round i1 wave 19, lane L1.
                                debugger_observes_locals: early_debugger_observes_locals,
                                // t1 wave 2, lane M: no elided lock under a
                                // monitor observer.
                                monitor_observer_at_compile:
                                    crate::runtime::jvmti::monitor_observer_may_exist(shared),
                                // t1 wave 3, lane M: no scalar replacement
                                // under an object observer.
                                scalar_observer_at_compile:
                                    crate::runtime::jvmti::object_observer_may_exist(shared),
                                // Wave 22, lane L1: read only under a debugger.
                                param_slot_tags: if early_debugger_observes_locals {
                                    crate::jit::param_slot_kind_tags(
                                        method_descriptor,
                                        early_is_static,
                                    )
                                } else {
                                    Vec::new()
                                },
                                // Wave 28, lane L2: the RBC.6 verdict above.
                                precise_exception_frames: early_precise_exception_frames,
                                // Wave 50, lane L1: this door binds its sites
                                // from the constant pool without resolving
                                // them, so a site this body runs first is
                                // resolved there, by its one-shot guard
                                // (`cratonvm_jit::JitFirstCallCell`).
                                first_call_cells: scan
                                    .invoke_ops
                                    .iter()
                                    .filter(|&&(_, _, op)| matches!(op, 0xb6..=0xb9))
                                    .filter_map(|&(pc, cp_idx, _)| {
                                        let cell =
                                            jit_bridge::first_call_cell_for(shared, class_id, cp_idx);
                                        (cell != 0).then_some((pc, cell))
                                    })
                                    .collect(),
                                // 2026-10-07 (item 1 of the i24-L6 page): the
                                // two facts the method-entry door hands the
                                // backend for a post-call site's first exit
                                // map, which this door used to leave unsaid.
                                method_known_unsynchronized: !is_synchronized,
                                artifact_keeps_a_call_record: early_keeps_a_call_record,
                                // t1 wave 3, lane X: every raw static
                                // self-call above carries its marker row.
                                static_self_calls_registered:
                                    cratonvm_jit::static_self_call_marker_enabled(),
                                // The self-call service's info, staged above.
                                self_call_invoke_info: eager_self_call_invoke_info,
                                ..Default::default()
                            };
                            // Wave 28, lane L2: the table, as every door stages it.
                            request.stage_exception_table(&code_attr.exception_table);
                            request
                        },
                    ) else {
                        cratonvm_jit::emit_direct_door_jit_decision(
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                            cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                            "eager-backend-refused",
                            cratonvm_jit::take_jit_bail_site(),
                            code_len,
                            eager_decision_started,
                        );
                        return None;
                    };
                    // Attach owned metadata to compiled method
                    cm._jit_strings = owned_jit_strings;
                    cm._jit_invoke_infos = owned_jit_invoke_infos;
                    cm._jit_mic_slots.extend(owned_mic_slots_early);
                    cm._jit_pic_slots.extend(owned_pic_slots_early);
                    stamp_compilation_epoch(
                        shared,
                        &class_name_arc,
                        &method_name_arc,
                        &descriptor_arc,
                        &mut cm,
                    );
                    // Round 13 wave 3 (lane replay2; proposal R13-1): the
                    // bytecode this body was compiled from (`padded`, the
                    // very allocation the backend read), so a trap after
                    // its class is redefined resumes the activation in its
                    // own code (`deopt_resume::obsolete_activation_source`).
                    stamp_compiled_source_of(
                        &cm,
                        Arc::new(CachedBytecodeMethod::from_parts(
                            cratonvm_jit_api::CachedMethodParts {
                                declaring_class_id: class_id,
                                class_name: Arc::clone(&class_name_arc),
                                method_name: Arc::clone(&method_name_arc),
                                method_descriptor: Arc::clone(&descriptor_arc),
                                source_file: source_file.get(shared),
                                code: Arc::clone(&padded),
                                exception_table: Arc::from(&code_attr.exception_table[..]),
                                max_stack: code_attr.max_stack,
                                max_locals: code_attr.max_locals,
                                // Cast: at most 255 parameters (JVMS §4.3.3)
                                num_params: count_method_params(method_descriptor) as u16,
                                is_synchronized,
                                is_static,
                            },
                        )),
                    );
                    let code_size = 0usize; // TODO: expose compiled code size
                                            // Round-7 HIGH-2 fix: do NOT hold `jit_cache.write()` across
                                            // `flight_recorder.lock()`.  The global JIT cache writer
                                            // would otherwise serialise JFR formatting for every
                                            // sibling compiler thread.  Two-phase: (1) put + get under
                                            // the JIT cache write, drop it; (2) JFR event emitted with
                                            // only the flight_recorder lock held.
                    let cached_result = {
                        let mut jit_cache = shared.jit.jit_cache.write();
                        // Interpreter round i1 wave 41, lane L2: a publication
                        // refused because a multi-class redefinition call is
                        // installing (`JitCache::hold_publications`) is a
                        // transient `None`, like a deferral: the seal below
                        // must not make it permanent (it is name-keyed, and
                        // nothing but a class unload lifts it). Asked on both
                        // sides of the `put`, so a hold that comes or goes
                        // around it is seen.
                        let held_before = jit_cache.publications_held();
                        // JIT round 15 wave 3 (VH3-1,
                        // `r15w3-vh4-thin-bound-publication-doors-patch`): a
                        // thin-`VarHandle`-bound body built before the first
                        // exact mint is dropped, transiently like a held
                        // publication (the next compile binds nothing).
                        let stale = crate::jit::helpers::varhandle_thin_bound_artifact_is_stale(
                            shared, &cm,
                        )
                            // JIT round 21 wave 14 (lane finalize; item 2 of
                            // `r21-review-w14-findings-20261019.md`): the gate
                            // asked the finalizer bar before this compile's
                            // epoch witness opened, so a finalizable define
                            // that recorded this constructor's class in
                            // between would not refuse the publication. Asked
                            // again now, after the build: a record made before
                            // this read refuses here, one made after it logs
                            // its `FinalizableSubtypeOf` after the witness
                            // opened, which refuses the `put`. Transient like
                            // a stale thin bind (no seal here).
                            || finalizer_bars_compiling(
                                shared,
                                class_id,
                                method_name,
                                method_descriptor,
                            );
                        if !stale {
                            jit_cache.put(
                                class_name_arc.clone(),
                                method_name_arc.clone(),
                                descriptor_arc.clone(),
                                class_id,
                                cm,
                            );
                        }
                        // Interpreter round i6 wave 117, lane L2: `execute` runs
                        // this method's own bytecode, so it may run a
                        // re-entry-only body (`JitCache::get_for_reentry`;
                        // identical to `get` with the switch off).
                        let published = jit_cache.get_for_reentry(
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            class_id,
                        );
                        if published.is_none()
                            && (stale || held_before || jit_cache.publications_held())
                        {
                            eager_deferred = true;
                        }
                        published
                        // `jit_cache` write-lock dropped here at end of scope.
                    };
                    // This first-call path compiles thousands of methods per run
                    // (everything reached via `invoke_method_shared`, incl. the
                    // reflective-invoke chain), and was the only compile site with
                    // NO success logging — which made the Bug-4 testAdHocData JIT
                    // execution invisible to CRATONVM_DBG_JITC-based bisection.
                    if let Some(c) = &cached_result {
                        // SBR5 lane jfr: jdk.Compilation + CompilerStatistics counters
                        // (no compile timer on this path: the duration is not tracked).
                        cratonvm_native_builtins_jfr::jfr_vm_more::compilation(
                            shared.vm_identity,
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64,
                            0,
                            i64::try_from(c.code_bytes().len()).unwrap_or(i64::MAX),
                            false,
                            true,
                            4,
                        );
                        if crate::runtime::env_cache::dbg_jitc() {
                            eprintln!(
                                "[cratonvm-jitc] first-compile {}.{}{} entry={:p} len={}",
                                class_name_arc,
                                method_name_arc,
                                descriptor_arc,
                                c.entry_ptr(),
                                c.code_bytes().len()
                            );
                        }
                        crate::jit::disasm::maybe_dump_annotated(
                            if c.used_ir_backend { "first/ir" } else { "first/sp" },
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            c.entry_ptr(),
                            c.code_bytes(),
                            c.osr_pc_to_native.as_deref(),
                            c.osr_local_assignments.as_deref(),
                        );
                        cratonvm_jit::emit_direct_door_jit_decision(
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                            cratonvm_jfr::jit_decision::CompileOutcome::SinglePass,
                            "compiled",
                            None,
                            code_len,
                            eager_decision_started,
                        );
                    } else {
                        cratonvm_jit::emit_direct_door_jit_decision(
                            &class_name_arc,
                            &method_name_arc,
                            &descriptor_arc,
                            cratonvm_jit::compile_gate::CompileDoor::EagerFirstCall,
                            cratonvm_jfr::jit_decision::CompileOutcome::Refused,
                            "eager-publish-refused",
                            None,
                            code_len,
                            eager_decision_started,
                        );
                    }
                    // Record JFR compilation event — flight_recorder lock taken
                    // *after* the JIT cache write has been released.
                    //
                    // Round-9 cross-cutting LOW-11: the previous implementation
                    // called `format!("{}.{}:{}", ...)` here which allocated a
                    // fresh `String` on every JIT compile. A thread-local scratch
                    // buffer lets us `write!` the formatted key into a reused
                    // allocation, then borrow it as `&str` for the emit call.
                    // The buffer is cleared (not freed) on each entry so capacity
                    // carries across compiles.
                    {
                        let now_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64; // Cast: duration to u64 nanoseconds
                        thread_local! {
                            static METHOD_KEY_SCRATCH: std::cell::RefCell<String> =
                                std::cell::RefCell::new(String::with_capacity(256));
                        }
                        METHOD_KEY_SCRATCH.with(|cell| {
                            use std::fmt::Write as _;
                            let mut buf = cell.borrow_mut();
                            buf.clear();
                            // `write!` into a `String` is infallible; discard the
                            // formatter `Result` rather than `unwrap`-ing it so we
                            // stay clean under the file-level
                            // `clippy::unwrap_used = deny`.
                            let _ = write!(
                                &mut *buf,
                                "{}.{}:{}",
                                class_name_arc, method_name_arc, descriptor_arc,
                            );
                            let mut jfr = shared.debug.flight_recorder.lock();
                            // Round-9 HIGH-5: convert the scratch buffer to an
                            // `Arc<str>` once and pass it to the `_arc` variant so
                            // the emit path doesn't re-do `Arc::from(&str)`.
                            let method_arc: Arc<str> = Arc::from(buf.as_str());
                            cratonvm_jfr::builtin::emit_compilation_event_arc(
                                &mut jfr,
                                method_arc,
                                1,     // compile_id
                                2,     // tier (C2-equivalent)
                                true,  // success
                                false, // not OSR
                                // Truncation-checked: code_size to i32; JVM method code limited to 64K
                                i32::try_from(code_size).unwrap_or(i32::MAX),
                                0, // code_size, inlined_bytes
                                now_ns,
                                0, // start_time, duration (not tracked)
                            );
                        });
                    }
                    cached_result
                };
                let compiled = if bg_compile_arm {
                    None
                } else {
                    compiled.or_else(eager_compile)
                };

                if compiled.is_none() && !c2_not_hot && !eager_deferred {
                    // RBC.4 — seal first-call compile failures for the same
                    // reason as the scan-reject seal above: without it every
                    // uncached invocation of a backend-bailing method re-ran
                    // resolution + x64 codegen here. (A method sealed by a
                    // transient resolver miss can still be compiled later by
                    // the invocation-counter upgrade path; the cache fast-path
                    // then routes calls to it.)
                    //
                    // `!c2_not_hot`: under CRATONVM_JIT_C2_FIRST_CALL a not-yet-hot
                    // method returned None on purpose (to interpret until its
                    // invocation counter crosses the threshold) — sealing it here
                    // would set `already_skipped` and permanently stop that counter.
                    note_jit_skip_seal("early-backend-bail", &skip_key);
                    shared.jit.jit_skip_set.write().insert(skip_key.clone());
                }
                // `RetainedCode`: this handle is regularly the LAST owner of
                // a body that has since been superseded, and it is released on
                // the mutator, so a bare `Arc` drop here unmaps executable
                // memory with no quiescence proof behind it. See
                // `cratonvm_jit::RetainedCode`.
                if let Some(compiled) = compiled.map(cratonvm_jit::RetainedCode::new) {
                    if crate::runtime::env_cache::jit_entry_dbg() {
                        eprintln!(
                            "[JIT_ENTRY] {}.{}{}",
                            class_name_str, method_name, method_descriptor
                        );
                    }
                    // No static-owner check before entry (JVMS §5.5). This door
                    // once initialized every class in the artifact's
                    // `static_init_classes` before bytecode 0 (branches never
                    // taken included, in an order HotSpot never uses, with an
                    // initializer failure routed through the throw-site-unknown
                    // handler search), and then (wave 2) interpreted any call
                    // while one of them was uninitialized — which kept a method
                    // that references an error-path-only constant holder off
                    // its compiled body for good. Neither is needed: a compiled
                    // static access is either inline, admitted only for a class
                    // `class_init_memo` records as initialized at compile time
                    // (`jit_resolve_static_base`; initialization is monotonic),
                    // or a `jit_getstatic`/`jit_putstatic_*` helper call that
                    // initializes at the bytecode and raises at that bci — the
                    // same contract every other door already enters bodies
                    // under (`i1-L3-compiled-entry-initializes-static-owners-eagerly`,
                    // interpreter round i1 wave 7).
                    if jit_early_exception.is_some() {
                        // Fall through to interpreter to handle via exception table
                    } else {
                        // Convert Value args to i64 for JIT calling convention
                        let ret_type = crate::jit::return_type(method_descriptor);
                        // The JIT ABI limit and encoder every door shares
                        // (`jit_bridge::jit_abi_max_java_params` /
                        // `jit_abi_arg_bits`). This door used to cap at 4 args
                        // on Windows and 6 elsewhere, against the other doors'
                        // 8 - needs_heap, so a 5-7-arg compiled body reached
                        // through `invoke_method_shared` ran interpreted
                        // (i5-L7 ABI-limit page). `run_jit_body_raw` enters
                        // every door's body the same way.
                        let max_jit_args =
                            jit_bridge::jit_abi_max_java_params(compiled.needs_heap());
                        let mut jit_args = [0i64; jit_bridge::JIT_ABI_MAX_JAVA_ARGS];
                        let mut jit_count = 0;
                        // Interpreter round i7 wave 143, lane L2 (item 1 of
                        // `i136-L2-door-boundary-suspects-of-wave-135`): this
                        // door's `args` come from every Rust caller of
                        // `execute` (natives, reflection, JNI `Call*Method`,
                        // the method-handle leaves), not from a verified
                        // operand stack, so it is the one door where a
                        // reference in a primitive parameter can arrive. The
                        // ABI encoding would hand its address to the compiled
                        // body as the number; the interpreter below reads the
                        // same local as the same `Value` every time. So such a
                        // call takes the interpreter (the `usize::MAX`
                        // sentinel), and a debug build names the producer.
                        let mut arg_tags = cratonvm_jit::DescriptorParamIter::new(method_descriptor);
                        let mut receiver_pending = !is_static;
                        // `live_args!`: the first-call compile above can
                        // reach a safepoint.
                        for arg in live_args!() {
                            // Bail out before writing past the end of `jit_args`
                            // with the existing usize::MAX sentinel, so the
                            // interpreter handles the call instead.
                            if jit_count >= max_jit_args {
                                jit_count = usize::MAX;
                                break;
                            }
                            let tag = if std::mem::take(&mut receiver_pending) {
                                b'L'
                            } else {
                                arg_tags.next().unwrap_or(b'L')
                            };
                            if !jit_bridge::jit_abi_arg_kind_agrees(arg, tag) {
                                debug_assert!(
                                    false,
                                    "execute's compiled door: argument {jit_count} of {}.{}{} is {arg:?} for tag {}",
                                    class_name_str,
                                    method_name,
                                    method_descriptor,
                                    tag as char,
                                );
                                jit_count = usize::MAX;
                                break;
                            }
                            match arg {
                                Value::Int(_)
                                | Value::Long(_)
                                | Value::Object(_)
                                | Value::Float(_)
                                | Value::Double(_) => {
                                    jit_args[jit_count] = jit_bridge::jit_abi_arg_bits(arg);
                                    jit_count += 1;
                                }
                                _ => {
                                    // Unsupported arg type: can't use JIT
                                    jit_count = usize::MAX;
                                    break;
                                }
                            }
                        }
                        if jit_count != usize::MAX {
                            // The body run, the signal drain and the sentinel
                            // classification are `jit_bridge::run_jit_body_raw`,
                            // the one copy the three `jit_bridge` doors share
                            // (interpreter round i1 wave 5, L7). This door kept a
                            // fourth, hand-written copy, and it drifted: it took
                            // the deopt flag only from wave 4 on, drained each
                            // implicit signal separately (two could build two
                            // throwables), routed an implicit exception without
                            // its stamped bci, and let the stash sink below resume
                            // a stashed frame over an implicit exception it had
                            // just built. Only where each outcome GOES is decided
                            // here. The entry hygiene (stale deopt flag), the
                            // `JIT_THREAD` TLS and the `JitEntryGuard` live there
                            // too; the guard states this activation's interpreter
                            // depth rather than inheriting it, because `execute`
                            // is entered from outside the interpreter loop
                            // (`tools/probes/StackTraceAfterOsr.java`: an inherited
                            // depth put a compiled `NullPointerException.<init>`
                            // chain between `main` and `probe` in the trace).
                            //
                            // gch h3/fd (d9d P2, opt-in
                            // `CRATONVM_JIT_FIRST_CALL_DOOR_ARGS_WEAK`): a body
                            // that owns its arguments runs with them held as weak
                            // handles instead of `args_root_guard`'s pins, so a
                            // parameter it dropped can be collected, as on
                            // HotSpot (`jit_bridge::WeakExecuteArgs`). Never for
                            // a synchronized method. Nothing between the release
                            // and the callee's prologue can collect.
                            let weak_args = if !is_synchronized
                                && shared.jit.jit_cache.first_call_door_args_weak()
                                && cratonvm_jit::callee_body_owns_its_arguments(&compiled)
                            {
                                jit_bridge::WeakExecuteArgs::take(
                                    shared,
                                    thread,
                                    &args_root_guard,
                                    &args_buf,
                                )
                            } else {
                                None
                            };
                            // Wave 81, lane L3: the door census's `call` split.
                            crate::vm::reflect_door_gate_stamp(
                                shared,
                                cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteBody,
                            );
                            // Wave 114, lane L7: the census's prologue cut.
                            if let Some(clock) = census_reentry.as_ref() {
                                clock.mark_body();
                            }
                            let body_name = jit_bridge::JitBodyName {
                                class_name: &class_name_str,
                                method_name,
                                descriptor: method_descriptor,
                            };
                            // gci i14/od (oomjit10 item 1, opt-in
                            // `CRATONVM_JIT_INTERP_DOOR_ARGS_PROMISE`): with the
                            // arguments held weakly, this door publishes the
                            // words it handed over, as the `jit_bridge` doors
                            // do (`jit_bridge::run_jit_body_raw_promised`). It
                            // reads them after the call only through
                            // `WeakExecuteArgs::restore`, off the return arm.
                            let raw_outcome = match weak_args.as_ref() {
                                Some(weak) if jit_bridge::door_promise_enabled(thread) => {
                                    jit_bridge::run_jit_body_raw_promised(
                                        shared,
                                        thread,
                                        &compiled,
                                        body_name,
                                        compiled.needs_heap(),
                                        &jit_args[..jit_count],
                                        weak,
                                    )?
                                }
                                _ => jit_bridge::run_jit_body_raw(
                                    shared,
                                    thread,
                                    &compiled,
                                    body_name,
                                    compiled.needs_heap(),
                                    &jit_args[..jit_count],
                                )?,
                            };
                            // Wave 83, lane L3: the door census's `call_body`
                            // cut (the body's run / `execute`'s tail).
                            crate::vm::reflect_door_gate_stamp(
                                shared,
                                cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteReturned,
                            );
                            if let Some(clock) = census_reentry.as_ref() {
                                clock.mark_returned();
                            }
                            // The survivors back into the guard's window for
                            // every reader below (none after a genuine return,
                            // whose arm hands the value back at once).
                            if let Some(weak) = weak_args {
                                let returns = matches!(
                                    &raw_outcome,
                                    jit_bridge::RawJitBodyOutcome::Returned(result)
                                        if *result != i64::MIN
                                            || matches!(ret_type, b'J' | b'D' | b'V')
                                );
                                if !returns {
                                    first_call_weak_arg_collected =
                                        weak.restore(thread, &mut args_root_guard, &mut args_buf);
                                }
                            }
                            let stashed = match raw_outcome {
                                // For a `J`/`D` (or `void`) body, `i64::MIN` is a
                                // sentinel only with a signal beside it — the deopt
                                // flag, or a frame stashed by `x64_deopt_entry` /
                                // `ir_deopt_entry` — and `run_jit_body_raw` has
                                // already classified both. Every other return type
                                // cannot produce that word as a value, so it keeps
                                // the historical re-run rather than hand back a zero
                                // `int` or a wild reference.
                                jit_bridge::RawJitBodyOutcome::Returned(result)
                                    if result != i64::MIN
                                        || matches!(ret_type, b'J' | b'D' | b'V') =>
                                {
                                    // Narrowed by the descriptor tag (JVMS §6.5
                                    // `ireturn`), as the other three doors do.
                                    return Ok(jit_bridge::jit_return_value(ret_type, result));
                                }
                                jit_bridge::RawJitBodyOutcome::Returned(_)
                                | jit_bridge::RawJitBodyOutcome::Deopt => {
                                    // Round 12 wave 4 (lane replay2): the
                                    // re-run below is counted and, when the
                                    // replay rule calls it unsound, retires
                                    // the method, as at the other doors.
                                    let (cause, sound) = jit_bridge::note_door_rerun_of_method(
                                        shared,
                                        &compiled,
                                        &code_attr.code,
                                        jit_bridge::DoorMethod {
                                            class_id,
                                            class_name: &class_name_str,
                                            method_name,
                                            descriptor: method_descriptor,
                                        },
                                        None,
                                        "execute-first-call",
                                    );
                                    // Round 13 wave 8 (lane replay4): and
                                    // refused, as the `jit_bridge` doors'
                                    // frameless arms refuse it.
                                    if unsound_rerun_refused(cause, sound, || {
                                        class_was_redefined(shared, class_id)
                                    }) {
                                        return Err(refuse_tierup_sink_rerun(
                                            shared,
                                            thread,
                                            cause,
                                            &class_name_str,
                                            method_name,
                                            method_descriptor,
                                            "after a trap with no frame",
                                        ));
                                    }
                                    None
                                }
                                jit_bridge::RawJitBodyOutcome::Stashed(rframe, cause, point) => {
                                    Some((rframe, cause, point))
                                }
                                jit_bridge::RawJitBodyOutcome::Throw { exc, athrow_bci } => {
                                    // Two sinks. The frame-push sink below routes
                                    // against a freshly pushed method-entry frame
                                    // whose locals are the ARGUMENTS: right for a
                                    // handler that reads nothing else, and a
                                    // silent wrong answer for one that reads a
                                    // local the body assigned (every other local
                                    // resumes zeroed — the `BindConverter` defect
                                    // `run_jit_callee_handler` documents). Such a
                                    // method goes through `run_jit_callee_handler`,
                                    // which resumes on the precise frame the body
                                    // published at its throw site, or declines and
                                    // propagates — the fail-closed answer
                                    // `route_jit_signal_exception` gives the three
                                    // `jit_bridge` doors. A lambda dispatch always
                                    // did (it has no frame of its own to push).
                                    let precise_route = lambda_dispatch_active()
                                        || cratonvm_jit::handler_resume_requires_precise_locals(
                                            &code_attr.code,
                                            code_attr.code.len(),
                                            &code_attr.exception_table,
                                            method_descriptor,
                                            is_static,
                                        );
                                    if precise_route {
                                        let cached = Arc::new(CachedBytecodeMethod::from_parts(
                                            cratonvm_jit_api::CachedMethodParts {
                                                declaring_class_id: class_id,
                                                class_name: Arc::from(&*class_name_str),
                                                method_name: Arc::from(method_name),
                                                method_descriptor: Arc::from(method_descriptor),
                                                source_file: source_file.get(shared),
                                                // Interned per method identity, as
                                                // the frame this method would get is.
                                                code: crate::runtime::frame::padded_bytecode_for_method(
                                                    class_id,
                                                    method_name,
                                                    method_descriptor,
                                                    &code_attr.code,
                                                    &code_attr.exception_table,
                                                ),
                                                exception_table: Arc::from(
                                                    &code_attr.exception_table[..],
                                                ),
                                                max_stack: code_attr.max_stack,
                                                max_locals: code_attr.max_locals,
                                                // Cast: at most 255 parameters (JVMS §4.3.3)
                                                num_params: count_method_params(method_descriptor)
                                                    as u16,
                                                is_synchronized,
                                                is_static,
                                            },
                                        ));
                                        // A stamp of this method's own that no
                                        // `try` covers is passed LITERALLY, so the
                                        // search answers "not caught" instead of
                                        // the pc-unknown search matching a typed
                                        // row by exception class alone (the
                                        // swallow `JitThrowPc::OutsideAllRanges`
                                        // exists to prevent). A precise frame's
                                        // own bci still wins inside the callee.
                                        let throw_pc =
                                            match jit_local_athrow_pc_kind(&cached, athrow_bci) {
                                                JitThrowPc::InRange(pc)
                                                | JitThrowPc::OutsideAllRanges(pc) => pc,
                                                JitThrowPc::Unknown => usize::MAX,
                                                // Wave 103, L2: the throwable already
                                                // left this method through a
                                                // not-entrant re-dispatch; its table
                                                // was searched there.
                                                JitThrowPc::EscapedRedispatch => {
                                                    return Err(
                                                        MethodCallFailed::ExceptionThrown(exc),
                                                    );
                                                }
                                            };
                                        // A miss hands back the throwable to
                                        // propagate: possibly a catch type's
                                        // resolution error, and re-read from a pin.
                                        return match run_jit_callee_handler(
                                            shared,
                                            thread,
                                            &cached,
                                            throw_pc,
                                            exc,
                                            live_args!(),
                                        ) {
                                            Ok(result) => result,
                                            Err((_, exc)) => {
                                                Err(MethodCallFailed::ExceptionThrown(exc))
                                            }
                                        };
                                    }
                                    // The frame-push sink never consumes a
                                    // precise frame, so one naming THIS method
                                    // is dropped rather than left for a later
                                    // invocation to mis-claim. A callee's frame is
                                    // left alone: its exception is still in flight.
                                    // JIT round 17 wave 4, lane stashdrop: under
                                    // the exceptional hand-over's switches the
                                    // dropped frame's holds (a self-locking body's
                                    // handed method monitor, on top of this door's
                                    // own hold) are released, and so are those of
                                    // the frames the take discards; off, the old
                                    // unreleasing drop.
                                    CompiledLocksOfAStash::drop_own_exceptional_frame_releasing(
                                        shared,
                                        thread,
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                        &|_, _| false,
                                        "first-call frame-push sink",
                                    );
                                    jit_early_throw_bci = athrow_bci;
                                    jit_early_exception = Some(exc);
                                    None
                                }
                            };
                            // FIX (2026-07-07, jit-invokedynamic-groovy-regression,
                            // FOURTH-pass root cause): this is the OLDEST JIT call-dispatch
                            // site in the interpreter (the first-call tier-up path in
                            // `execute()` itself), predating the real-frame-deopt resume
                            // machinery. It never consumed `take_last_deopt()`, so a
                            // reconstructed frame stashed by `x64_deopt_entry` here (which,
                            // since the invokedynamic-trap snapshot became unconditional,
                            // happens on every reason-8 trap reached through THIS path) was
                            // silently left behind instead of cleared -- a LATER, UNRELATED
                            // deopt check on the same thread could then pick up this STALE
                            // frame and resume with the wrong locals/stack.
                            //
                            // That alone would not be sufficient: unlike the safe-reject
                            // helper (`jit_uncommon_trap`, the `frame_box_ptr == None` arm of
                            // `emit_deopt_stubs`), which synchronously calls
                            // `DeoptimizationController::deoptimize` and so blacklists
                            // (`MakeNotCompilable`) an `UnreachedCode` trap on FIRST
                            // occurrence regardless of which VM call site is running, the
                            // precise-resume path (`x64_deopt_entry`) relies on the CALLER to
                            // drive de-speculation via `real_frame_deopt_resume_and_
                            // despeculate` -- which only the three newer call sites do. This
                            // legacy site fell through to "re-run the whole method
                            // interpreted" WITHOUT EVER BLACKLISTING the method, so the next
                            // call re-entered the JIT-compiled artifact and re-hit the SAME
                            // trap again -- for Groovy's `IndyInterface`-heavy dispatch
                            // (essentially every call), that meant re-running a script's own
                            // class/method-generation bytecode from entry ~2000 times in a
                            // single test, each re-run re-registering a `doCall` method
                            // Groovy's own compiler had already registered -- exactly the
                            // "doCall duplicates another method" symptom. Fixed by driving the
                            // same de-speculation call here that `jit_uncommon_trap` does,
                            // using this method's own identity (`class_name_str`/
                            // `method_name`/`method_descriptor` are already in scope, since
                            // this whole block IS that method's own first-call JIT tier-up)
                            // and the reason/bci recovered from the stashed frame itself
                            // before discarding it.
                            if let Some((rframe_for_despec, stashed_cause, stashed_point)) = stashed
                            {
                                // HANDLER11-2 (JIT round 19 wave 11, lane
                                // handler11): the body a not-entrant stub
                                // forwarded this call to, when that is the
                                // body that trapped; judged with its facts
                                // (`jit_bridge::forwarded_trap_body`).
                                let forwarded =
                                    jit_bridge::forwarded_trap_body(&compiled, stashed_point);
                                let compiled: &crate::jit::CompiledMethod =
                                    forwarded.as_deref().unwrap_or(&compiled);
                                dbg_deopt_sink(
                                    "execute-first-call-tierup",
                                    &rframe_for_despec,
                                    &format!("{class_name_str}.{method_name}{method_descriptor}"),
                                );
                                // Round 15 wave 4 (lane chain5, chain4's patch
                                // page): a forced chain exit of this body,
                                // claimed before anything can refuse; its
                                // chain carries the claim to the push, and
                                // it is left uncharged below under
                                // `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED`.
                                let forced_trip =
                                    claim_forced_chain_trip(shared, &compiled, stashed_point);
                                let uncharged_forced =
                                    leave_forced_trip_uncharged(shared, forced_trip);
                                // Interpreter round i1 wave 23, lane L2: the
                                // locks the stashed frame's compiled code
                                // took. A resumed frame keeps them; every
                                // other arm below abandons the frame and
                                // releases them (`CompiledLocksOfAStash`).
                                let compiled_locks = CompiledLocksOfAStash::pin_compiled_locks(
                                    thread,
                                    &rframe_for_despec,
                                );
                                // Round 9 wave 11 (osr11): the loop-header `OsrExit` map
                                // is first in the list, so the first match charged a
                                // speculative-BCE guard trap there as `OsrExit`.
                                // Round 11 wave 2: the attribution all four
                                // sinks share (`trap_reason_for_frame`), which
                                // also names the array-access NullCheck /
                                // BoundsCheck pair from the frame. Round
                                // 11 wave 5: the stash's own cause first.
                                // Round 13 wave 6 (lane chain2): an inlined
                                // chain is charged at its call site in THIS
                                // method (`stash_charge_site`, as
                                // `real_frame_deopt_resume_or_throw_and_despeculate`).
                                let (deopt_reason, charge_bci) = stash_charge_site(
                                    &compiled,
                                    &code_attr.code,
                                    &rframe_for_despec,
                                    stashed_cause,
                                );
                                // Which of the three independent gates
                                // refused is the whole diagnosis, and they
                                // need completely different fixes — record
                                // it for the message below.
                                let resume_gate_ok = compiled.can_deopt_resume;
                                // Wave 10 (L4): by name AND by the body that
                                // fired it (`deopt_stash_is_from_artifact`).
                                let key_matches = deopt_frame_matches_artifact(
                                    stash_identity_scope(&rframe_for_despec),
                                    &compiled,
                                    stashed_point,
                                    &class_name_str,
                                    method_name,
                                    method_descriptor,
                                );
                                // De-speculate THIS method only when the
                                // frame is this method's. It used to run
                                // unconditionally, so a stash belonging to
                                // a nested compiled callee blacklisted the
                                // innocent method whose tier-up happened to
                                // be running — while the `!key_matches`
                                // branch below ALSO de-speculated the
                                // frame's real owner. Two methods made
                                // not-compilable per foreign frame, one of
                                // them for no reason: on `TestScript` that
                                // is `StringFunction1.getValue`, a
                                // per-row expression evaluator.
                                //
                                // Through `despeculate_trapped_method` and
                                // not `deoptimize` directly, because the
                                // reason a bare `deoptimize` records here is
                                // the DEOPT POINT'S: for a trapping bci that
                                // carries a safepoint that reason is
                                // `TransferToInterpreter`, whose
                                // `recommend_action` is `Reinterpret` - the
                                // artifact stays live, the next call
                                // re-enters it, and it traps again, forever.
                                // The sibling sink
                                // (`try_resume_trapped_callee`) has always
                                // known that and asked for the site-trap
                                // policy instead; both now ask the same
                                // function, so they cannot answer
                                // differently again.
                                // JIT round 19 wave 4 (lane splice4,
                                // SPLICE3-2,
                                // `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS`):
                                // a marked expected-throw trip of this body
                                // is left uncharged, and a chain's implicit
                                // exception escalates to the named innermost
                                // guard first, as at the door sink. Off:
                                // exactly the charge below.
                                let expected_throw = key_matches
                                    && !uncharged_forced
                                    && stash_sink_expected_throw_uncharged(
                                        Some(&*compiled),
                                        &rframe_for_despec,
                                    );
                                if key_matches && !uncharged_forced && !expected_throw {
                                    crate::jit::helpers::despeculate_trapped_method_of_frame(
                                        shared,
                                        &class_name_str,
                                        method_name,
                                        method_descriptor,
                                        class_id,
                                        deopt_reason,
                                        charge_bci,
                                        &rframe_for_despec,
                                    );
                                    // JIT round 19 wave 14 (TAIL12-1, opt-in
                                    // `CRATONVM_JIT_DIV_GUARD_RETIER_SINKS`):
                                    // a hot zero divisor of a single frame
                                    // re-tiers this body, as at the
                                    // call-site service (a chain's opcode is
                                    // its spliced scope's, not read here).
                                    if rframe_for_despec.caller_frames.is_empty() {
                                        crate::jit::helpers::retier_division_guard_trap_at_sink(
                                            shared,
                                            &class_name_str,
                                            method_name,
                                            method_descriptor,
                                            class_id,
                                            deopt_reason,
                                            code_attr.code.get(charge_bci as usize).copied(),
                                            charge_bci,
                                        );
                                    }
                                }
                                // This first-call tier-up sink historically
                                // discarded the reconstructed frame and
                                // restarted the method at bci 0. That
                                // duplicates every side effect committed
                                // before the guard. Build the same cached
                                // metadata the hot callsites use and resume
                                // the captured frame directly.
                                // The stash belongs to a DIFFERENT method — a
                                // nested compiled callee whose sentinel bubbled
                                // out to here because the call site that invoked
                                // it emitted no callee-deopt service check (the
                                // statically-bound JIT-to-JIT direct-call gap
                                // fixed in jit/src/lib.rs; it was NOT inlining —
                                // the x64 inliner rolls back any body that
                                // publishes deopt metadata, and an inlined deopt
                                // point carries the CALLER's method_key anyway).
                                // THIS method never trapped, so the
                                // "refusing side-effecting replay" arm below does
                                // not apply to it: that refusal is about a frame
                                // proving OUR OWN native code ran past bci 0, and
                                // a foreign frame proves nothing of the sort.
                                //
                                // Raising a fatal `InternalError` here also made
                                // an orphan permanent in a second way: the frame
                                // had already been TAKEN, so every later
                                // `has_last_deopt()` was clean, but only because
                                // the VM had died. Before the take it poisoned the
                                // sentinel disambiguation
                                // (`jit_dispatch_threw`) for unrelated call sites.
                                //
                                // Do what the sibling tier-up sink
                                // (`jit-callsite-b`, jit_bridge.rs) already does
                                // for exactly this case: de-speculate the frame's
                                // real owner so it stops re-trapping, drop the
                                // frame, and fall through to interpreted
                                // execution of this (innocent) method. Measured on
                                // `org.h2.test.scripts.TestScript`, which the
                                // fatal error killed at ~212 s with
                                // `stashed key "org/h2/util/StringUtils.cache:..."`
                                // while running `StringFunction1.getValue`.
                                //
                                // Three refusals the RECONSTRUCTED FRAME
                                // genuinely cannot answer, and they are the
                                // ones the sibling sink makes or the
                                // emission side names:
                                //
                                //  * an `ACC_SYNCHRONIZED` method - the
                                //    method monitor is not in the frame
                                //    (`try_resume_trapped_callee` refuses
                                //    the same shape, for the same reason);
                                //  * a body that takes a monitor at all.
                                //    Every `FrameState` `ir_lower` builds
                                //    hard-codes `monitors: Vec::new()`, so
                                //    a resumed frame for such a body
                                //    believes it holds no lock and its
                                //    `monitorexit` unbalances. `ir_lower`'s
                                //    own comment says the sink "cannot fire
                                //    on information that was never
                                //    recorded" - `bytecode_holds_monitor`
                                //    is that information, read off the
                                //    bytecode instead;
                                //  * a resume bci past this method's code.
                                // The predicate's own terms, kept apart
                                // here only so the refusal message can name
                                // WHICH of them declined. The decision
                                // itself is `sink_precise_resume_allowed`,
                                // which all four deopt sinks ask.
                                let body_holds_monitor = cratonvm_jit::bytecode_holds_monitor(
                                    &code_attr.code,
                                    code_attr.code.len(),
                                );
                                let bci_in_code = (charge_bci as usize) < code_attr.code.len();
                                // `can_deopt_resume` is very nearly not
                                // the question this sink has to ask, and
                                // asking it is what turned an ordinary
                                // trap into a hard process abort. "Nearly"
                                // because its elided-monitor conjunct DOES
                                // carry weight on the single-pass side --
                                // which is why `body_holds_monitor` above
                                // replaces it rather than dropping it. See
                                // `cratonvm_jit::deopt_sink_resume_enabled`
                                // for the whole argument and the measured
                                // population. The frame is already
                                // reconstructed and stashed; the sibling
                                // sink resumes exactly this stash, with
                                // exactly this builder, and has never
                                // consulted that flag.
                                //
                                // STRICTLY ADDITIVE. The `resume_gate_ok` arm is
                                // the pre-2026-09-07 condition, unchanged: when the
                                // backend set `can_deopt_resume` it has ALREADY
                                // vouched that no monitor was elided, so hanging the
                                // new guards on that arm too would turn a resume
                                // that works today - a single-pass body with a real
                                // `synchronized` block, trapping where no lock is
                                // held - into the very abort this change exists to
                                // remove. The guards belong to the NEW arm, the one
                                // running without the backend's word for it.
                                let resume_allowed = key_matches
                                    && (resume_gate_ok
                                        || sink_precise_resume_allowed(
                                            &code_attr.code,
                                            code_attr.code.len(),
                                            // Round 12 wave 7 (lane replay4): this
                                            // door holds the method monitor
                                            // (`sync_monitor`) around the resumed
                                            // frame's whole run, so ACC_SYNCHRONIZED
                                            // is a refusal only when it does not.
                                            is_synchronized
                                                && (sync_monitor.is_none()
                                                    || !jit_bridge::door_sync_additive_resume()),
                                            charge_bci,
                                        ));
                                let mut materialize_failed = false;
                                if resume_allowed {
                                    let cached = Arc::new(CachedBytecodeMethod::from_parts(
                                        cratonvm_jit_api::CachedMethodParts {
                                            declaring_class_id: class_id,
                                            class_name: Arc::from(&*class_name_str),
                                            method_name: Arc::from(method_name),
                                            method_descriptor: Arc::from(method_descriptor),
                                            source_file: source_file.get(shared),
                                            code: crate::runtime::frame::padded_bytecode(
                                                &code_attr.code,
                                            ),
                                            exception_table: Arc::from(
                                                &code_attr.exception_table[..],
                                            ),
                                            max_stack: code_attr.max_stack,
                                            max_locals: code_attr.max_locals,
                                            num_params: count_method_params(method_descriptor)
                                                as u16,
                                            is_synchronized,
                                            is_static,
                                        },
                                    ));
                                    let pin_base = thread.native_pin_roots.len();
                                    // M2 — the stash may name an INLINED CALLER
                                    // CHAIN: the trap happened inside a body this
                                    // artifact spliced, so the trapping scope sits
                                    // beneath one or more inlined callers.
                                    // `build_deopt_frame_inner` returns ONE `Frame`
                                    // by value and cannot express that, so it
                                    // declines a non-empty chain and counts the
                                    // refusal as `DeoptFrameBail::InlinedChain` —
                                    // "a sink that was not taught", as against
                                    // `InlinedChainUnmaterialisable`, "we asked and
                                    // the VM could not". This is the third sink
                                    // being taught, after `resume_real_ir_deopt`
                                    // and `try_resume_trapped_callee`; it was the
                                    // last one still counted as untaught.
                                    //
                                    // NOT LIVE YET. `jit/src/x64/inlining.rs`'s
                                    // splice postcondition rolls back any splice
                                    // that published a deopt point, so no artifact
                                    // carries a chain. M1 made well-formed splice
                                    // metadata PRODUCIBLE; this is the consumer
                                    // half for this door, so relaxing that
                                    // postcondition does not hand a chain to a sink
                                    // that would refuse it.
                                    enum Rebuilt {
                                        Single(crate::runtime::frame::Frame),
                                        /// Outermost first. Opaque: only
                                        /// `run_deopt_frame_chain_to_completion`
                                        /// may spend it.
                                        Chain(DeoptFrameChain),
                                    }
                                    let rebuilt = if rframe_for_despec.caller_frames.is_empty() {
                                        // Round 12 wave 5 (lane replay3): a
                                        // full heap is the trapped method's
                                        // `OutOfMemoryError`, thrown at its
                                        // caller as HotSpot pops the frame;
                                        // the build released its compiled
                                        // locks.
                                        match build_deopt_frame_or_refusal(
                                            shared,
                                            thread,
                                            &cached,
                                            &rframe_for_despec,
                                            false,
                                            true,
                                        ) {
                                            Ok(frame) => Some(Rebuilt::Single(frame)),
                                            Err(why)
                                                if frame_build_heap_failure_is_answered(why) =>
                                            {
                                                thread.native_pin_roots.truncate(pin_base);
                                                // Released by the build
                                                // already: only the pins go.
                                                compiled_locks.unpin_keeping_the_locks(thread);
                                                return Err(rematerialisation_out_of_memory(
                                                    shared, thread,
                                                ));
                                            }
                                            Err(_) => None,
                                        }
                                    } else if chain_inner_scope_redefined_since_compile(
                                        shared,
                                        &compiled,
                                        class_id,
                                        &rframe_for_despec,
                                    ) || chain_outer_scope_redefined_since_compile(
                                        shared,
                                        &compiled,
                                        class_id,
                                        &rframe_for_despec,
                                    ) {
                                        // Round 13 wave 6 (lane chain2): never
                                        // from a spliced callee's class
                                        // redefined since the compile. Wave 8
                                        // (lane chain3): nor from this
                                        // method's own CURRENT code. Round 14
                                        // wave 2 (lane deopt): when the method's
                                        // own class is the only one redefined,
                                        // its frame is rebuilt from the body's
                                        // own template and restamped
                                        // (`CRATONVM_DEOPT_CHAIN_OUTER_OWN_SOURCE`).
                                        // Round 14 wave 4 (lane resume2,
                                        // CH3W-3): a redefined spliced
                                        // callee's frame from the bytecode the
                                        // compile spliced, restamped at the
                                        // push (as the doors do, R14DP-5).
                                        build_stale_chain_from_own_sources(
                                            shared,
                                            &compiled,
                                            &cached,
                                            &rframe_for_despec,
                                            &compiled.splice_scope_class_ids,
                                        )
                                        // Round 15 wave 4 (lane chain5): the claim.
                                        .map(|chain| chain.with_forced_trip(forced_trip))
                                        .map(Rebuilt::Chain)
                                    } else {
                                        // By the artifact's scope class ids.
                                        build_deopt_frame_chain_hinted(
                                            shared,
                                            &cached,
                                            &rframe_for_despec,
                                            &compiled.splice_scope_class_ids,
                                        )
                                        // The outermost frame reads the body's
                                        // own pool generation (interpreter round
                                        // i1 wave 37, lane L3).
                                        .map(|chain| {
                                            chain.with_outermost_cp_stamp(
                                                compiled.compile_cp_stamp(),
                                            )
                                        })
                                        // Round 15 wave 4 (lane chain5): the claim.
                                        .map(|chain| chain.with_forced_trip(forced_trip))
                                        .map(Rebuilt::Chain)
                                    };
                                    materialize_failed = rebuilt.is_none();
                                    if let Some(rebuilt) = rebuilt {
                                        // The single-frame arm releases the
                                        // materialization pins at the push
                                        // (gen r5w4/jit8): the frame is
                                        // authoritative from then on, and a pin
                                        // kept for the whole resumed run rooted
                                        // whatever the frame later dropped. The
                                        // chain arm still keeps them to the end;
                                        // the truncate below cleans up both.
                                        // ACC_SYNCHRONIZED: the method monitor is
                                        // `sync_monitor`, taken above for this whole
                                        // `execute` activation; the resumed frame runs
                                        // to completion inside it and the guard's drop
                                        // releases it once (round 11 wave 2, checked
                                        // for the IR monitor-body `can_deopt_resume`).
                                        let resumed = match rebuilt {
                                            Rebuilt::Single(frame) => {
                                                // No MethodEntry: entered compiled.
                                                // gen r5w4/jit8: the materialization
                                                // pins above `pin_base` end at the
                                                // push (the pushed frame is the root
                                                // from then on); the compiled locks
                                                // are pinned below `pin_base`.
                                                // Round 13 wave 1 (lane replay):
                                                // stamped with the body's pool
                                                // generation before it runs, as
                                                // the `jit_bridge` sinks stamp it.
                                                execute_rebuilt_frame_releasing_pins(
                                                    shared,
                                                    thread,
                                                    frame,
                                                    pin_base,
                                                    compiled.compile_cp_stamp(),
                                                )
                                            }
                                            // `run_deopt_frame_chain_to_completion`,
                                            // NOT `execute_prebuilt_frame`, and the
                                            // distinction is load-bearing: the
                                            // latter measures the frame-stack depth
                                            // AFTER the outer frames are already
                                            // pushed, so it would return the moment
                                            // the INNERMOST frame returned and
                                            // strand the rest of the chain
                                            // mid-method.
                                            Rebuilt::Chain(chain) => {
                                                let depth = chain.frame_count();
                                                match run_deopt_frame_chain_to_completion_releasing_pins(
                                                    shared, thread, chain, pin_base,
                                                ) {
                                                    Some(res) => res,
                                                    None => {
                                                        // `push_inlined_chain` fails
                                                        // only on an operand-stack
                                                        // push that materialisation
                                                        // already bounded against
                                                        // each frame's own
                                                        // `max_stack`, so this is
                                                        // unreachable by
                                                        // construction. If it ever
                                                        // happens, SOME frames are
                                                        // already on the stack:
                                                        // falling through to the
                                                        // whole-method replay below
                                                        // would run them a second
                                                        // time. REFUSE instead — the
                                                        // interpreter unwinds pushed
                                                        // frames the way it unwinds
                                                        // any error, and no side
                                                        // effect is replayed.
                                                        thread.native_pin_roots.truncate(pin_base);
                                                        compiled_locks
                                                            .unpin_keeping_the_locks(thread);
                                                        return Err(
                                                            MethodCallFailed::InternalError(
                                                                VmError::Internal {
                                                                    message: format!(
                                                                        "deopt resume could not push a {depth}-frame inlined chain for \
                                                                         {}.{}{} at bci {}; refusing to re-execute",
                                                                        class_name_str,
                                                                        method_name,
                                                                        method_descriptor,
                                                                        rframe_for_despec.bci,
                                                                    ),
                                                                },
                                                            ),
                                                        );
                                                    }
                                                }
                                            }
                                        };
                                        thread.native_pin_roots.truncate(pin_base);
                                        compiled_locks.unpin_keeping_the_locks(thread);
                                        return resumed;
                                    }
                                    thread.native_pin_roots.truncate(pin_base);
                                }
                                // Not resumed: the frame is abandoned (a
                                // foreign frame dropped, a harmless replay, or
                                // the refusal below), so what its compiled
                                // code locked is released now.
                                compiled_locks.release_for_a_rerun(shared, thread);
                                if !key_matches {
                                    // The stash is a DIFFERENT method's — a
                                    // nested compiled callee whose sentinel
                                    // bubbled out to here because the call site
                                    // that invoked it had no callee-deopt service
                                    // check, so `try_resume_trapped_callee` never
                                    // ran there and the frame kept travelling
                                    // outward looking for the "outer consumer that
                                    // CAN attribute it" — which, once the sentinel
                                    // has passed through the callee's own caller,
                                    // no longer exists. This arm is now defence in
                                    // depth: the emission gap it was written for
                                    // is fixed in jit/src/lib.rs.
                                    //
                                    // The refusal below does NOT apply to it: it
                                    // exists because a frame belonging to THIS
                                    // method proves this method's native code ran
                                    // past bci 0, and a foreign frame proves
                                    // nothing about this method at all. Raising a
                                    // fatal `InternalError` for someone else's
                                    // orphan killed the whole VM run — measured on
                                    // `org.h2.test.scripts.TestScript`, dead at
                                    // ~212 s with `stashed key
                                    // "org/h2/util/StringUtils.cache:(...)"`
                                    // while running
                                    // `StringFunction1.getValue`, and on
                                    // `TestCrashAPI` the same way.
                                    //
                                    // Do exactly what the sibling tier-up sink
                                    // already does for this case
                                    // (`jit-callsite-b`, jit_bridge.rs): the
                                    // frame's real owner is de-speculated above
                                    // via `DeoptimizationController::deoptimize`
                                    // so it stops re-trapping, the orphan is
                                    // dropped (it was taken at the top of this
                                    // block, which is also what stops it
                                    // poisoning `has_last_deopt`'s sentinel
                                    // disambiguation at unrelated later call
                                    // sites), and this innocent method falls
                                    // through to interpreted execution.
                                    despeculate_stashed_frame_method_with_cause(
                                        shared,
                                        &rframe_for_despec,
                                        stashed_cause,
                                        stashed_point,
                                    );
                                    // Round 12 wave 5 (lane replay3): the
                                    // census's `foreign-stash` row, as at the
                                    // other method-entry doors.
                                    let (cause, sound) = jit_bridge::note_door_rerun_of_method(
                                        shared,
                                        &compiled,
                                        &code_attr.code,
                                        jit_bridge::DoorMethod {
                                            class_id,
                                            class_name: &class_name_str,
                                            method_name,
                                            descriptor: method_descriptor,
                                        },
                                        Some((&rframe_for_despec, stashed_point)),
                                        "execute-first-call",
                                    );
                                    // Round 13 wave 8 (lane replay4): the
                                    // doors' answer to an unsound foreign
                                    // re-run. A Java `InternalError` the
                                    // caller can catch, not the fatal
                                    // VM-internal error the H2 note above is
                                    // about; `CRATONVM_JIT_DOOR_UNSOUND_FOREIGN_RERUN_RAISES=0`
                                    // keeps the re-run.
                                    if unsound_rerun_refused(cause, sound, || {
                                        class_was_redefined(shared, class_id)
                                    }) {
                                        return Err(refuse_tierup_sink_rerun(
                                            shared,
                                            thread,
                                            cause,
                                            &class_name_str,
                                            method_name,
                                            method_descriptor,
                                            &format!(
                                                "after a trap of another method ({} bci {})",
                                                rframe_for_despec.method_key, rframe_for_despec.bci,
                                            ),
                                        ));
                                    }
                                } else if replay_from_entry_is_observably_equivalent_for_stash(
                                    &code_attr.code,
                                    &compiled,
                                    &rframe_for_despec,
                                    stashed_point,
                                ) {
                                    // The refusal below is about DUPLICATED
                                    // SIDE EFFECTS, and this method has none
                                    // to duplicate: no store outside the
                                    // frame, no call, no monitor action. Its
                                    // locals are rebuilt from the same
                                    // arguments, so a re-run from bci 0 is
                                    // observably the abandoned attempt.
                                    //
                                    // This arm is not a relaxation, it is the
                                    // other half of a rule that was only ever
                                    // written down once.
                                    // `ir_unresumable_protected_trap` ADMITS
                                    // the optimizing tier for a protected
                                    // range carrying an unresumable trap when
                                    // the range is read-only, on the stated
                                    // ground that "a read-only
                                    // `try { return a[i]; } catch (...)`
                                    // replays harmlessly, so the refusal would
                                    // buy nothing and cost the compile" — and
                                    // then this sink refused every replay,
                                    // harmless or not. The compiler's
                                    // narrowing rested on a behaviour the
                                    // consumer did not have, so exactly the
                                    // shape it deliberately let through was
                                    // the shape that died here.
                                    //
                                    // `probes/EscapeKindProbe.java` is the
                                    // witness: its `virtual` / `iface` /
                                    // `special` arms are
                                    // `try { return 10 / (i - i); }
                                    // catch (IllegalStateException e) { ... }`
                                    // reached through the dispatch helper, and
                                    // all three died with `InternalError:
                                    // precise deoptimization unavailable ...
                                    // refusing side-effecting replay` where
                                    // HotSpot and `--nojit` return the
                                    // ArithmeticException the caller catches.
                                    //
                                    // The two ends now ask ONE predicate
                                    // (`opcode_commits_side_effect`) so they
                                    // cannot drift apart again.
                                    if None::<std::ffi::OsString>
                                        .is_some()
                                    {
                                        eprintln!(
                                            "[cratonvm-deopt] replaying {}.{}{} from entry: \
                                             unresumable at bci {} but the body commits no \
                                             side effect",
                                            class_name_str,
                                            method_name,
                                            method_descriptor,
                                            rframe_for_despec.bci,
                                        );
                                    }
                                    // Round 12 wave 5 (lane replay3): counted
                                    // as the census's exact `own-stash` row;
                                    // the verdict asks the predicate above
                                    // again, so it never retires from here.
                                    jit_bridge::note_door_rerun_of_method(
                                        shared,
                                        &compiled,
                                        &code_attr.code,
                                        jit_bridge::DoorMethod {
                                            class_id,
                                            class_name: &class_name_str,
                                            method_name,
                                            descriptor: method_descriptor,
                                        },
                                        Some((&rframe_for_despec, stashed_point)),
                                        "execute-first-call",
                                    );
                                } else {
                                    // Precise reconstruction is a correctness
                                    // requirement once native code has executed
                                    // past bci 0. Refuse a whole-method replay:
                                    // it is observably wrong for methods with
                                    // stores, I/O, monitor actions, or callbacks.
                                    // WHICH gate declined is the whole
                                    // diagnosis, and they need completely
                                    // different fixes. `can_deopt_resume`
                                    // is no longer one of them unless the
                                    // resume was switched off, so naming it
                                    // bare would send the next reader after
                                    // a flag that is not the cause.
                                    // The monitor and bci refusals belong to the NEW arm
                                    // only, so ask them only when that arm was in play:
                                    // with `resume_gate_ok` set the resume was attempted
                                    // regardless, and naming one of them would describe a
                                    // decision that never happened.
                                    let why = if !resume_gate_ok
                                        && is_synchronized
                                        && (sync_monitor.is_none()
                                            || !jit_bridge::door_sync_additive_resume())
                                    {
                                        "the method is ACC_SYNCHRONIZED, so a resumed frame \
                                     would not hold the method monitor"
                                    } else if !resume_gate_ok && body_holds_monitor {
                                        "the body enters a monitor and the IR frame states \
                                     record none"
                                    } else if !resume_gate_ok && !bci_in_code {
                                        "the resume bci is past this method's code"
                                    } else if materialize_failed {
                                        // Asked BEFORE the switch: a resume that was
                                        // attempted and FAILED is the real reason; the
                                        // switch is only the reason when no attempt was
                                        // made at all.
                                        "the frame could not be materialised from its map"
                                    } else if !cratonvm_jit::deopt_sink_resume_enabled() {
                                        "can_deopt_resume=false (no deopt points, or an \
                                     elided monitor) and CRATONVM_JIT_DEOPT_SINK_RESUME=0"
                                    } else {
                                        "unknown"
                                    };
                                    // `deopt_reason` DEFAULTS to `UnreachedCode` when no
                                    // point carries this bci, so printing it bare names a
                                    // reason nothing ever requested — which is how the
                                    // spliced-bci defect read as an `UnreachedCode` trap
                                    // for a day. Say which of the two this is.
                                    let reason_is_real = compiled
                                        .deopt_points
                                        .iter()
                                        .any(|dp| dp.bci == rframe_for_despec.bci);
                                    let reason_note = if reason_is_real {
                                        "reason"
                                    } else {
                                        "NO deopt point carries this bci, so the reason \
                                     below is this sink's default rather than a \
                                     request — reason"
                                    };
                                    let message = format!(
                                        "precise deoptimization unavailable for \
                                         {}.{}{} at bci {} ({}, stashed key {:?}, \
                                         inline callers {}, {} {:?}); \
                                         refusing side-effecting replay",
                                        class_name_str,
                                        method_name,
                                        method_descriptor,
                                        rframe_for_despec.bci,
                                        why,
                                        rframe_for_despec.method_key,
                                        rframe_for_despec.caller_frames.len(),
                                        reason_note,
                                        deopt_reason,
                                    );
                                    // Round 13 wave 1 (lane replay,
                                    // `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`):
                                    // the doors' answer (`jit_bridge::door_rerun_or_refuse`).
                                    // Counted and retired like every door re-run;
                                    // a redefined class re-runs as at the doors;
                                    // anything else is a Java `InternalError`
                                    // the caller can catch, where this used to
                                    // be a VM-internal error that ended the
                                    // thread.
                                    if !tierup_sink_refusal_throws() {
                                        return Err(MethodCallFailed::InternalError(
                                            VmError::Internal { message },
                                        ));
                                    }
                                    let (cause, sound) = jit_bridge::note_door_rerun_of_method(
                                        shared,
                                        &compiled,
                                        &code_attr.code,
                                        jit_bridge::DoorMethod {
                                            class_id,
                                            class_name: &class_name_str,
                                            method_name,
                                            descriptor: method_descriptor,
                                        },
                                        Some((&rframe_for_despec, stashed_point)),
                                        "execute-first-call",
                                    );
                                    // Round 13 wave 5 (lane resume): the one
                                    // answer the doors and the lambda arm ask
                                    // (`CRATONVM_JIT_DOOR_UNSOUND_RERUN_RAISES`
                                    // included).
                                    // Round 13 wave 8 (lane replay4): the
                                    // every-cause form (the own stash is its
                                    // first arm), counted as a refusal.
                                    if unsound_rerun_refused(cause, sound, || {
                                        class_was_redefined(shared, class_id)
                                    }) {
                                        note_door_rerun_refused(cause);
                                        return Err(
                                            crate::runtime::exceptions::throw_runtime_error(
                                                shared,
                                                thread,
                                                RuntimeError::InternalError { message },
                                            ),
                                        );
                                    }
                                }
                            }
                            // Deoptimized with no resumable frame, or a throwable
                            // for the frame-push sink below: fall through to
                            // interpreter execution.
                        }
                    } // end else (jit_early_exception.is_none())
                }
            }
        } // end if !already_skipped
    } // end JIT block

    // Check stack overflow before pushing frame
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        dump_stack_on_soe(thread);
        // A deferred entry's `MethodEntry` (the i54-L6 retirement) has no frame.
        thread.deferred_method_entry_depth = 0;
        // t1 wave 11, lane INTA: a constructor ENTRY that cannot push its
        // frame withdraws the receiver's allocation-time finalizer
        // registration, as the interpreter's own `invokespecial` doors do
        // (`CRATONVM_FINALIZER_WITHDRAW_AT_CTOR_ENTRY_PUSH_FAILURE`). Not for
        // the throw sink of a compiled run (`jit_early_exception`): its body
        // already ran, so the old answer (keep) stands.
        if jit_early_exception.is_none() && method_name == "<init>" {
            withdraw_ctor_receiver_at_entry_trip(shared, thread, method_name, live_args!());
        }
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::StackOverflowError,
        )));
    }

    // Record the frame depth before we push so that on any error path we can
    // truncate the stack back to exactly this depth (plus the one frame we push).
    // This prevents orphaned inner frames when execute_frame returns early via
    // `return Err(e)` without unwinding the frames it pushed for callees.
    let frames_depth_before_push = thread.frames.len();

    // Create the frame.
    // PERF (round-5 vm #9 fix): use `Frame::new_from_arcs` so that the
    // bytecode is taken straight from `code_attr.code: ByteView` (deref
    // to `&[u8]`) through `padded_bytecode(&[u8])` (one alloc for the +2
    // zero-padding) instead of first cloning the bytes into an owned
    // `Vec<u8>` (`.to_vec()`) and then copying *that* into the padded Arc
    // inside `Frame::new` — two full bytecode mem-copies per method entry.
    // The synthetic test frames keep the convenient `Frame::new` entry
    // point that owns its `Vec<u8>`.
    //
    // NOTE (P4 reverted): an attempt to pool locals/stack here via
    // `Frame::new_pooled` caused execution hangs across the BouncyCastle
    // regression suites (bc-asn1/crypto/crypto-prng went rc=0 -> rc=124).
    // The uncached invoke path is not a safe consumer of the thread-local
    // frame pools as-is; reverted to the proven `new_from_arcs` path.
    //
    // gch h3/fd (d9d P2): a re-run from entry after the compiled run let an
    // owned argument be collected cannot be exact (that argument is now null
    // in `args_buf`): refused, as the `jit_bridge` doors refuse it. The
    // throw sink (`jit_early_exception`) is not a re-run: an owning body
    // declares no handler, so the pushed frame only propagates.
    if first_call_weak_arg_collected && jit_early_exception.is_none() {
        return Err(
            jit_bridge::refuse_first_call_rerun_with_a_collected_owned_arg(
                shared,
                thread,
                &class_name_str,
                method_name,
                method_descriptor,
            ),
        );
    }
    // `CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` (interpreter round i3 wave 79,
    // lane L7, default off): a CACHED frame from the per-VM record of this
    // method (`execute_reentry_record`), the kind the interpreter's own
    // invoke pushes; off (no key), the owned frame below, as before.
    // `CRATONVM_EXECUTE_LEAN_REENTRY` (wave 114, L7): a deferred source file is
    // copied now, for the frame that names it. Wave 115 (interpreter round
    // i6, lane L7): a cached frame names its record's copy, so the record
    // reads the deferred file only when it is (re)built, not on every hit.
    let mut frame = if let Some(key) = reentry_key {
        let cached = execute_reentry_record(
            shared,
            class_id,
            key,
            &class_name_str,
            &source_file,
            &code_attr,
            is_synchronized,
            is_static,
        );
        Frame::new_cached_unpooled(cached, live_args!())
    } else {
        let source_file: Option<Arc<str>> = source_file.into_value(shared);
        // `CRATONVM_EXECUTE_SHARED_METHOD_VIEW` (wave 77, L4): the names and
        // the tables are this call's own shared views (`ExecuteMethodView`);
        // off, the fresh copies this has always made.
        let frame_source_file = if view.shared {
            source_file
        } else {
            source_file.map(|s| std::sync::Arc::from(&*s))
        };
        Frame::new_from_arcs(
            class_id,
            view.class_name(&class_name_str),
            view.method_name(method_name),
            view.descriptor(method_descriptor),
            frame_source_file,
            // Interned per *method identity* (see `frame-arena.md` §5.2), not
            // content-addressed: `local_liveness.rs` keys a per-method
            // liveness table on `Arc::as_ptr(code)` and derives it from the
            // method's exception table, so two byte-identical methods with
            // different handler ranges must keep distinct Arcs. This ends a
            // full method-body memcpy plus an allocation per uncached method
            // entry, and lets `quickened.rs::intern` hit across frames of the
            // same method instead of rebuilding under a fresh address.
            crate::runtime::frame::padded_bytecode_for_method(
                class_id,
                method_name,
                method_descriptor,
                &code_attr.code,
                &code_attr.exception_table,
            ),
            // Built once under the lock (`execute_exception_table`).
            code_attr.exception_table,
            code_attr.max_stack,
            code_attr.max_locals,
            live_args!(),
        )
    };
    // gci i4/il: the owned frame above does not see the access flags (a no-op
    // for the cached one); a static method's slot 0 is then filtered by
    // liveness like every other parameter (`Frame::liveness_keeps_slot0`).
    frame.note_method_is_static(is_static);

    // GC-safety: if the JIT early-compile path produced an exception, root its
    // oop on the operand stack BEFORE `push_frame_and_fire_entry` fires the
    // JVMTI MethodEntry callback. That callback may allocate Java heap and
    // trigger a moving young-gen GC; an oop reachable only through the
    // `jit_early_exception` Rust local across the fire would be unrooted and
    // could be relocated, leaving a stale pointer for the handler walk /
    // propagation below. The frame's operand stack is GC-scanned, so we read
    // the (possibly relocated) reference back from it after the fire. The
    // pre-fire push is best-effort (`let _ =`): if it does not take — e.g. a
    // `max_stack == 0` method, which by definition has no operand-using
    // handler — we fall back to the original local, no worse than before.
    // Mirrors `route_jit_exception_through_method` / `resume_from_ir_deopt`.
    if let Some(exc) = jit_early_exception {
        let _ = frame.stack.push(Value::Object(Some(exc)));
    }
    // The ACC_SYNCHRONIZED monitor moves to the frame: from here on
    // `pop_and_recycle_frame_with_reason` releases it (normal return,
    // exception unwind, and the orphan sweep below). Nothing between here and
    // the push can reach a safepoint; after it the GC remaps the slot.
    if let Some(obj) = sync_monitor.as_mut().and_then(|g| g.transfer_to_frame()) {
        frame.monitor_on_exit = Some(obj);
    }

    // Push frame onto thread
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/invoke_method_shared] depth={} {}.{}{}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor()
        );
    }
    // Interpreter round i7 wave 135, lane L1 (i133-L8 shape 2,
    // `CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL`, parity, default ON): this
    // frame is the entry frame of the loop `execute_frame` runs below, so a C
    // agent's `ForceEarlyReturn*` from its `MethodEntry` callback is recorded
    // at a callback point and taken at that loop's first top. Not for a frame
    // the JIT already ran (`jit_early_exception`), whose exception route below
    // may leave before any loop top.
    if jit_early_exception.is_none() {
        push_frame_and_fire_entry_as_loop_entry(shared, thread, frame);
    } else {
        push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    }
    gci_i11jd_witness::note_entry(shared, thread);
    // The pushed frame's locals are the GC-scanned copy of the arguments now.
    // Release the pins so they do not outlive the locals (a body that nulls
    // a parameter must be able to let it be collected, as on HotSpot).
    drop(args_root_guard);
    if deferred_sync_entry {
        enter_deferred_synchronized_entry(shared, thread, class_id, is_static);
    }
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(shared, thread);
    }

    // If the JIT early-compile path encountered a Java exception from a callee,
    // route it through this method's exception table before interpreter execution.
    if let Some(early_exc) = jit_early_exception {
        // The frame has been pushed. Search its exception table for a handler.
        let frame_idx = thread.frames.len() - 1;
        // Re-read the exception oop from the (GC-scanned) operand stack: a GC
        // during the MethodEntry callback may have relocated it, and only the
        // scanned frame slot was updated — not the original Rust local. Fall
        // back to the bound `early_exc` (the known-Some local) if the pre-fire
        // push did not take (e.g. a `max_stack == 0` method). Binding it in the
        // `if let` keeps this fallback panic-free (no `.expect()`), satisfying
        // the strict-zero production-panic gate.
        let exc = match thread.frames[frame_idx].stack.pop() {
            Ok(Value::Object(Some(r))) => r,
            _ => early_exc,
        };
        // The JIT executed the entire method body as native code, so there is
        // no live throw-site PC. Previously this passed the freshly-pushed
        // frame's `last_instr_pc` (always 0) to the PC-ranged search, which
        // silently missed every handler whose `try` region does not start at
        // offset 0 — e.g. a JIT-compiled `Main.main` whose `catch (Throwable)`
        // protects `[2,26)` would never catch a callee exception, leaking it
        // past the method (Jetty `start.jar` launcher). Use the PC-unknown
        // search instead: it skips catch-all `finally` entries (unsafe to
        // match without a PC) but matches typed handlers by exception class.
        // ...unless the compiled body stamped its own throw site, which it does
        // at every throwing bci inside a protected range. The PC-unknown search
        // matches typed handlers by exception CLASS ALONE, so a method with two
        // protected ranges catching the same type gets the FIRST row's handler
        // whichever range actually threw. bc-java's
        // `ProvRevocationChecker.check` is exactly that shape — `try { crl }
        // catch (Recoverable) { ...ocsp... }` and `try { ocsp } catch
        // (Recoverable) { ...crl... }` — so an OCSP failure ran the CRL branch's
        // handler, which re-called OCSP, and the exception escaped a method that
        // was supposed to fall back. The stamp makes the range check possible;
        // `jit_local_athrow_pc_in_frame` refuses it unless it lands in one of
        // THIS method's ranges, so a foreign stamp still degrades to the
        // pc-unknown search rather than picking a handler at random.
        //
        // The search resolves catch types as the interpreter unwinder does, so
        // it may run Java and may replace the throwable with a resolution error
        // (JVMS §5.4.3): it reads and writes a pin, and `exc` is re-read from
        // it afterwards.
        let pin_base = thread.native_pin_roots.len();
        thread.native_pin_roots.push(exc);
        let early_throw =
            jit_local_athrow_pc_in_frame(&thread.frames[frame_idx], jit_early_throw_bci);
        // The compiled throw site the events report (wave 21, L2); `usize::MAX`
        // when the body stamped none.
        // `EscapedRedispatch` (wave 103, L2): the re-dispatch that ran the
        // method already reported the throw at its real site.
        let early_throw_pc = match early_throw {
            JitThrowPc::InRange(pc) | JitThrowPc::OutsideAllRanges(pc) => pc,
            JitThrowPc::Unknown | JitThrowPc::EscapedRedispatch => usize::MAX,
        };
        let handler = match early_throw {
            JitThrowPc::InRange(pc) => {
                find_exception_handler_any_pc(shared, thread, frame_idx, pc, pin_base, true)
            }
            // The compiled body named a throw site of its own that no `try`
            // covers: nothing here can catch it, and the pc-unknown search
            // would match a typed row by exception class alone.
            JitThrowPc::OutsideAllRanges(_) => None,
            // Wave 103, L2: the throwable already left this method through a
            // not-entrant re-dispatch, whose search ran at the real throw pc.
            JitThrowPc::EscapedRedispatch => None,
            JitThrowPc::Unknown => {
                find_exception_handler_pc_unknown(shared, thread, frame_idx, pin_base)
            }
        };
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        let found = handler.map(|handler_pc| (handler_pc, exc));
        match found {
            Some((handler_pc, exc_ref)) => {
                thread.frames[frame_idx].stack.clear();
                let _ = thread.frames[frame_idx]
                    .stack
                    .push(Value::Object(Some(exc_ref)));
                thread.frames[frame_idx].pc = handler_pc;
                // `Exception` at the compiled throw site (wave 21, L2), then
                // `ExceptionCatch`; a caught exception is no longer "reported"
                // (wave 11, L1). The frame roots the throwable.
                report_exception_caught_by_compiled_door(
                    shared,
                    thread,
                    frame_idx,
                    early_throw_pc,
                    handler_pc,
                );
                // Fall through to execute_frame which will resume at handler_pc
            }
            None => {
                // Escaping: reported at the compiled throw site while this
                // frame still names the method (wave 21, L2), so the caller's
                // unwinder finds it reported. Only with a stamp: without one
                // the unwinder's report at the caller's invoke pc is the more
                // honest location. Pins across its callbacks.
                let exc = if early_throw_pc == usize::MAX {
                    exc
                } else {
                    deliver_exception_event_if_armed(shared, thread, frame_idx, exc, early_throw_pc)
                };
                // No handler in this method — pop frame and propagate. The pop
                // can fire JVMTI `FramePop`, whose callback may collect, and
                // `exc` is no longer on the frame or pinned: pop through the
                // helper that keeps a thrown reference pinned across it.
                let mut result: MethodCallResult = Err(MethodCallFailed::ExceptionThrown(exc));
                pop_root_frames_keeping_result(
                    shared,
                    thread,
                    frames_depth_before_push,
                    &mut result,
                );
                return result;
            }
        }
    }

    // Execute (with panic protection for stack underflow/overflow).
    //
    // NOTE(round-4-wave-3): the per-method `catch_unwind` here is load-bearing
    // and intentionally retained. The interpreter's super-instruction fast
    // path uses `pop_unchecked` / `set_local_unchecked`, which can panic if a
    // malformed or bridge-corrupted frame violates verified stack shapes.
    // Without this
    // catch_unwind, those panics would propagate past the JIT entry frame and
    // abort the process under the Windows SEH / signal-handler interop in
    // `runtime/signals.rs` (the signal handler converts SIGSEGV / SIGFPE via
    // `catch_unwind`, but a Rust panic crossing the JIT-call boundary is not
    // catchable by the OS unwinder). Global `-Xverify:none` disables the raw
    // handlers; this `catch_unwind` remains the final guard for trusted
    // per-class verification bypasses and corrupted bridge state. The ~10 ns setup cost is
    // amortized over the entire `execute_frame` invocation — hundreds to
    // thousands of bytecodes — not per-bytecode.
    //
    // gc-common w5-c: the pin watermark for the heap-exhaustion arm below.
    let pin_floor = thread.native_pin_roots.len();
    // Wave 81, lane L3: the door census's `call` split (an interpreted callee).
    crate::vm::reflect_door_gate_stamp(
        shared,
        cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteBody,
    );
    // Wave 114, lane L7: the census's prologue cut (an interpreted callee).
    if let Some(clock) = census_reentry.as_ref() {
        clock.mark_body();
    }
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_frame(shared, thread)
    })) {
        Ok(r) => r,
        // gc-common w5-c: the native allocators' heap-exhaustion unwind
        // (`crate::runtime::native_oom`), NOT a bytecode panic. Interpreted Java
        // that a native called back into inherits that native's unwind
        // permission, so an interpreter-internal `NativeContextImpl` allocation
        // under it unwinds to THIS handler first. It is the `OutOfMemoryError`
        // HotSpot throws, so report it as one: no `[PANIC_IN]` line, no
        // `NotImplemented`. Pins the unwound attempt pushed are dropped, as the
        // native funnel drops them; the orphan-frame truncation below still
        // runs. `common-w4c-native-oom-unwind-inside-a-java-callback-becomes-an-internal-error`.
        Err(panic_info)
            if crate::runtime::native_oom::is_native_alloc_oom_payload(&*panic_info) =>
        {
            thread.native_pin_roots.truncate(pin_floor);
            Err(crate::runtime::native_oom::caught_oom_as_call_failure(
                panic_info,
            ))
        }
        Err(panic_info) => {
            // Convert panics (e.g., pop_unchecked underflow) to errors
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic in bytecode execution".to_string()
            };
            if let Some(f) = thread.frames.last() {
                eprintln!(
                    "[PANIC_IN] {}.{}{} pc={} max_stack={} :: {}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc,
                    f.max_stack,
                    msg
                );
            }
            // cceres2: if this panic is the CRATONVM_DBG_STALE_OBJREF canary,
            // attribute the stale address against THIS thread's own state —
            // the heap-side holder scan runs in get_header; this covers the
            // thread-local half (frame locals/stack, pins, snapshot), which
            // is where a root-remap gap lives.
            {
                let stale = cratonvm_gc::stale_objref_debug::LAST_STALE_ADDR
                    .swap(0, std::sync::atomic::Ordering::AcqRel);
                if stale != 0 {
                    let mut found = 0usize;
                    for (fi, fr) in thread.frames.iter().enumerate() {
                        for li in 0..fr.locals_len() {
                            if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                                if o.as_ptr() as usize == stale {
                                    eprintln!(
                                        "[STALE-FRAME] frame#{fi} {}.{} pc={} local[{li}] holds stale 0x{stale:x}",
                                        fr.class_name(), fr.method_name(), fr.pc,
                                    );
                                    found += 1;
                                }
                            }
                        }
                        for si in 0..fr.stack.len() {
                            if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                                if o.as_ptr() as usize == stale {
                                    eprintln!(
                                        "[STALE-FRAME] frame#{fi} {}.{} pc={} stack[top-{si}] holds stale 0x{stale:x}",
                                        fr.class_name(), fr.method_name(), fr.pc,
                                    );
                                    found += 1;
                                }
                            }
                        }
                    }
                    for (pi, p) in thread.native_pin_roots.iter().enumerate() {
                        if p.as_ptr() as usize == stale {
                            eprintln!(
                                "[STALE-FRAME] native_pin_roots[{pi}] holds stale 0x{stale:x}"
                            );
                            found += 1;
                        }
                    }
                    {
                        let snap = thread.root_snapshot.lock();
                        for (si, r) in snap.iter().enumerate() {
                            if r.as_ptr() as usize == stale {
                                eprintln!(
                                    "[STALE-FRAME] root_snapshot[{si}] holds stale 0x{stale:x}"
                                );
                                found += 1;
                            }
                        }
                    }
                    eprintln!(
                        "[STALE-FRAME] summary: {} thread-state slot(s) hold stale 0x{stale:x} (tid={}, blocked={})",
                        found,
                        thread.thread_id.0,
                        thread
                            .gc_block_state
                            .in_blocked_region
                            .load(std::sync::atomic::Ordering::Acquire),
                    );
                }
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NotImplemented { feature: msg },
            )))
        }
    };
    // Wave 83, lane L3: the door census's `call_body` cut (an interpreted
    // callee's run / `execute`'s tail).
    crate::vm::reflect_door_gate_stamp(
        shared,
        cratonvm_native_builtins::ReflectDoorGatePoint::ExecuteReturned,
    );
    if let Some(clock) = census_reentry.as_ref() {
        clock.mark_returned();
    }

    // A continuation yield is a scheduler control transfer, not a method
    // failure. Keep every frame exactly as execute_frame left it so the
    // virtual-thread manager can freeze the complete Java stack.
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }

    // Truncate any orphaned inner frames that execute_frame may have left on the
    // stack when it returned early via `return Err(e)` without popping callees.
    // We expect exactly `frames_depth_before_push + 1` frames here (the one we
    // pushed above). Pop everything above that level before popping our own frame.
    // A reference result stays pinned across the pops' FramePop callbacks
    // (`pop_root_frames_keeping_result`, wave 4 lane L1).
    let mut result = result;
    pop_root_frames_keeping_result(shared, thread, frames_depth_before_push, &mut result);

    result
}

