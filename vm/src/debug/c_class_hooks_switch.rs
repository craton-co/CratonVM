#[allow(unused_imports)]
use super::*;

/// `CRATONVM_JVMTI_C_CLASS_HOOKS` ([`DebuggerGates::c_class_hooks`]), also
/// asked by a startup agent's env in `Agent_OnLoad` (whether
/// `can_generate_all_class_hook_events` is potential; wave 55).
pub(crate) fn c_class_hooks_switch() -> bool {
    true
}

/// `CRATONVM_JVMTI_C_RETRANSFORM` ([`DebuggerGates::c_retransform`]), also
/// asked by a startup agent's env in `Agent_OnLoad`, before its VM and its
/// gates exist (`jvmti::native_env`'s potential capabilities).
pub(crate) fn c_retransform_switch() -> bool {
    true
}

/// `CRATONVM_JVMTI_C_REDEFINE` ([`DebuggerGates::c_redefine`], wave 57), also
/// asked by a startup agent's env in `Agent_OnLoad`, as
/// [`c_retransform_switch`] is.
pub(crate) fn c_redefine_switch() -> bool {
    true
}

/// `CRATONVM_JVMTI_C_FRAME_CONTROL` ([`DebuggerGates::c_frame_control`],
/// wave 61), also asked by a startup agent's env in `Agent_OnLoad`, as
/// [`c_retransform_switch`] is.
pub(crate) fn c_frame_control_switch() -> bool {
    true
}

impl DebuggerGates {
    /// Must the D14 bridge forward `event` into the older
    /// `jvmti::EventManager` of this VM (wave 54,
    /// [`Self::bridge_listener_gate`])? `true` while the gate is off; with it
    /// on, one relaxed load: whether that manager has `event` enabled.
    #[inline]
    pub fn legacy_jvmti_listens(&self, event: crate::jvmti::JvmtiEvent) -> bool {
        !self.bridge_listener_gate
            || self
                .legacy_jvmti_events
                .load(std::sync::atomic::Ordering::Relaxed)
                & crate::jvmti::events::event_mask_bit(event)
                != 0
    }

    /// These gates, reading the older `jvmti::EventManager`'s enabled events
    /// from `mask` (`EventManager::enabled_mask_handle`; wave 54). `Vm::new`
    /// makes a VM's gates with its manager's mask.
    pub fn with_legacy_jvmti_events(mask: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            legacy_jvmti_events: mask,
            ..Self::new()
        }
    }

    /// Does the C JVMTI table grant `can_retransform_classes`
    /// ([`Self::c_retransform`], wave 54)?
    #[inline]
    pub fn c_retransform(&self) -> bool {
        self.c_retransform
    }

    /// Does the C JVMTI table grant `can_redefine_classes`
    /// ([`Self::c_redefine`], wave 57)?
    #[inline]
    pub fn c_redefine(&self) -> bool {
        self.c_redefine
    }

    /// Does the C JVMTI table serve frame control: `PopFrame`,
    /// `ForceEarlyReturn*`, `ClearAllFramePops`, `NotifyFramePop` of a
    /// blocked thread ([`Self::c_frame_control`], wave 61)?
    #[inline]
    pub fn c_frame_control(&self) -> bool {
        self.c_frame_control
    }

    /// Does the C JVMTI table serve `PopFrame` / `ForceEarlyReturn*` of the
    /// calling thread from its `Breakpoint` / `SingleStep` callback
    /// ([`Self::c_callback_frame_control`], wave 62)? Only with frame control
    /// itself on.
    #[inline]
    pub fn c_callback_frame_control(&self) -> bool {
        self.c_frame_control && self.c_callback_frame_control
    }

    /// Is a C agent's `ClassFileLoadHook` armed in the start phase
    /// ([`Self::c_start_phase_hook`], wave 54)?
    #[inline]
    pub fn c_start_phase_hook(&self) -> bool {
        self.c_start_phase_hook
    }

    /// Does the C JVMTI table deliver `ClassFileLoadHook` and `ClassLoad`
    /// ([`Self::c_class_hooks`], wave 53)?
    #[inline]
    pub fn c_class_hooks(&self) -> bool {
        self.c_class_hooks
    }

    /// Does the C JVMTI table deliver `ExceptionCatch`
    /// ([`Self::c_exception_catch`], wave 52)?
    #[inline]
    pub fn c_exception_catch(&self) -> bool {
        self.c_exception_catch
    }

    /// Does C JVMTI `GetThreadState` read the suspension hint before the
    /// debug-state lock ([`Self::state_suspension_hint`], interpreter round
    /// i3 wave 72, lane L6)?
    #[inline]
    pub fn state_suspension_hint(&self) -> bool {
        self.state_suspension_hint
    }

    /// Does the C JVMTI `jthread` lookup leave native through the vm-only
    /// entry ([`Self::jvmti_thread_lookup_vm_only`], interpreter round i6
    /// wave 130, lane L1b)?
    #[inline]
    pub fn jvmti_thread_lookup_vm_only(&self) -> bool {
        self.jvmti_thread_lookup_vm_only
    }

    /// Does the C JVMTI `GetThreadState` read its registry facts in one
    /// `threads` read ([`Self::jvmti_thread_state_one_read`], interpreter
    /// round i6 wave 132, lane L1)?
    #[inline]
    pub(crate) fn jvmti_thread_state_one_read(&self) -> bool {
        self.jvmti_thread_state_one_read
    }

    /// Does the C JVMTI `GetThreadState` read the `SUSPENDED` bit from the
    /// thread's own word ([`Self::jvmti_state_suspend_word`], interpreter
    /// round i7 wave 135, lane L1)?
    #[inline]
    pub(crate) fn jvmti_state_suspend_word(&self) -> bool {
        self.jvmti_state_suspend_word
    }

    /// Does a JDWP suspension wait for its threads before its reply
    /// ([`Self::suspend_handshake`], wave 49)?
    #[inline]
    pub fn suspend_handshake(&self) -> bool {
        self.suspend_handshake
    }

    /// Do the event-thread and the poll's `ClassPrepare` suspensions wait for
    /// their threads before their event set goes out
    /// ([`Self::suspend_event_settle`], interpreter round i4 wave 94)?
    #[inline]
    pub fn suspend_event_settle(&self) -> bool {
        self.suspend_handshake && self.suspend_event_settle
    }

    /// Does an invocation's reply wait for the threads its closing
    /// re-suspension covered ([`Self::invoke_reply_settle`], interpreter
    /// round i5 wave 101)?
    #[inline]
    pub fn invoke_reply_settle(&self) -> bool {
        self.suspend_handshake && self.invoke_reply_settle
    }

    /// Does a thread in a native outside a blocking region acknowledge a
    /// JDWP suspension ([`Self::suspend_native_ack`], wave 53)?
    #[inline]
    pub fn suspend_native_ack(&self) -> bool {
        self.suspend_native_ack
    }

    /// Does a thread that wins a contended monitor while suspended give it
    /// back until its resume ([`Self::monitor_exit_on_suspend`], wave 54)?
    #[inline]
    pub fn monitor_exit_on_suspend(&self) -> bool {
        self.monitor_exit_on_suspend
    }

    /// Does a thread in a native list its frames through a window while the
    /// native runs ([`Self::native_frames_window`], wave 56)?
    #[inline]
    pub fn native_frames_window(&self) -> bool {
        self.native_frames_window
    }

    /// Does a native frames window under compiled code carry a copy of the
    /// thread's JIT entry chain for the reader to walk
    /// ([`Self::native_window_chain`], wave 58)?
    #[inline]
    pub fn native_window_chain(&self) -> bool {
        self.native_window_chain
    }

    /// Does the suspension's settle wait take the long bound
    /// ([`Self::suspend_settle_long`], wave 56)?
    #[inline]
    pub fn suspend_settle_long(&self) -> bool {
        self.suspend_settle_long
    }

    /// Do the suspension machinery's waits wait on [`Self::state_changed`]
    /// ([`Self::park_condvar`], wave 54)?
    #[inline]
    pub fn park_condvar(&self) -> bool {
        self.park_condvar
    }

    /// Do the suspension's waits wait on the thread's own suspend word
    /// ([`Self::thread_suspend_word`], wave 63)?
    #[inline]
    pub fn thread_suspend_word(&self) -> bool {
        self.thread_suspend_word
    }

    /// Does a stop-the-world request unpark the threads parked on their
    /// suspend word ([`Self::stw_unparks_parked`], wave 64)? Only with
    /// [`Self::thread_suspend_word`]: without it no park waits on its word.
    #[inline]
    pub fn stw_unparks_parked(&self) -> bool {
        self.thread_suspend_word && self.stw_unparks_parked
    }

    /// Does a park at a suspend point wait GC-blocked
    /// ([`Self::park_gc_blocked`], wave 65)? Only with
    /// [`Self::thread_suspend_word`]: the region's wait is the word's wait,
    /// which a pause does not need to end.
    #[inline]
    pub fn park_gc_blocked(&self) -> bool {
        self.thread_suspend_word && self.park_gc_blocked
    }

    /// Does heap work prefer a thread outside a GC-blocked park region
    /// ([`Self::heap_work_outside_region`], wave 66)? Only with
    /// [`Self::park_gc_blocked`]: without it no park waits in a region.
    #[inline]
    pub fn heap_work_outside_region(&self) -> bool {
        self.park_gc_blocked() && self.heap_work_outside_region
    }

    /// Does a park re-entering its region reuse its standing deposit
    /// ([`Self::park_region_reuses_deposit`], wave 66)? Only with
    /// [`Self::park_gc_blocked`].
    #[inline]
    pub fn park_region_reuses_deposit(&self) -> bool {
        self.park_gc_blocked() && self.park_region_reuses_deposit
    }

    /// Does a park open its GC-blocked region only at the first pause of its
    /// hold ([`Self::park_region_at_first_pause`], wave 68)? Only with
    /// [`Self::park_gc_blocked`]: without it no park waits in a region.
    #[inline]
    pub fn park_region_at_first_pause(&self) -> bool {
        self.park_gc_blocked() && self.park_region_at_first_pause
    }

    /// Does a park region left for heap work keep its entry's snapshot when
    /// nothing folded into it ([`Self::park_leave_keeps_unfolded_snapshot`],
    /// wave 68)? Only with [`Self::park_region_reuses_deposit`]: the kept
    /// snapshot is the one a re-entry then reuses.
    #[inline]
    pub fn park_leave_keeps_unfolded_snapshot(&self) -> bool {
        self.park_region_reuses_deposit() && self.park_leave_keeps_unfolded_snapshot
    }

    /// How long a park at a suspend point waits on its word between two
    /// looks at `stw_requested` (wave 64, lane L6): [`DEBUG_STATE_POLL`],
    /// unless `unparked_by_stw` -- the park enrolled with the GC barrier
    /// ([`Self::stw_unparks_parked`]), whose request wakes it -- when the
    /// word's own [`SUSPEND_WORD_BACKSTOP`] is the only bound.
    #[inline]
    pub fn parked_word_wait_bound(unparked_by_stw: bool) -> std::time::Duration {
        if unparked_by_stw {
            SUSPEND_WORD_BACKSTOP
        } else {
            DEBUG_STATE_POLL
        }
    }

    /// Wake every wait on the debug state ([`Self::state_changed`], wave
    /// 54): a suspend count, a queued task, an arrival changed. Nothing with
    /// [`Self::park_condvar`] off. Callable with or without the debug-state
    /// lock held.
    #[inline]
    pub(crate) fn notify_state_changed(&self) {
        // Wave 64 (lane L6): before the notification, so a waiter woken by
        // it reads the moved count.
        if self.settle_arrival_count {
            self.arrivals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if self.park_condvar {
            self.state_changed.notify_all();
        }
    }

    /// Does the C table list the calling thread from the shared builder
    /// ([`Self::c_listing_shared`], interpreter round i6 wave 119)?
    #[inline]
    pub(crate) fn c_listing_shared(&self) -> bool {
        self.c_listing_shared
    }

    /// Does a reviewed-`Intrinsic` shadow post frame-only JDWP method events
    /// ([`Self::intrinsic_shadow_method_events`], interpreter round i6 wave
    /// 120)?
    #[inline]
    pub(crate) fn intrinsic_shadow_method_events(&self) -> bool {
        self.intrinsic_shadow_method_events
    }

    /// Do a stand-in's `Object.wait` levels answer their locals
    /// ([`Self::standin_level_locals`], interpreter round i6 wave 121)?
    #[inline]
    pub(crate) fn standin_level_locals(&self) -> bool {
        self.standin_level_locals
    }

    /// Does a blocked thread's liveness-dead reference local read through
    /// its slot origin ([`Self::blocked_dead_locals`], interpreter round i6
    /// wave 126)?
    #[inline]
    pub(crate) fn blocked_dead_locals(&self) -> bool {
        self.blocked_dead_locals
    }

    /// Does a blocked thread's liveness-dead reference local read through
    /// its slot origin when every local is a root
    /// ([`Self::blocked_rooted_dead_locals`], interpreter round i6 wave 130)?
    #[inline]
    pub(crate) fn blocked_rooted_dead_locals(&self) -> bool {
        self.blocked_rooted_dead_locals
    }

    /// Does a startup C agent's `can_access_local_variables` keep every local
    /// alive ([`Self::c_agent_keeps_locals_alive`], interpreter round i6
    /// wave 132)?
    #[inline]
    pub(crate) fn c_agent_keeps_locals_alive(&self) -> bool {
        self.c_agent_keeps_locals_alive
    }

    /// Did a startup C agent of this VM acquire `can_access_local_variables`
    /// with [`Self::c_agent_keeps_locals_alive`] on
    /// ([`Self::c_agent_observes_locals`], interpreter round i6 wave 132)?
    /// One `Acquire` load; read by every root scan through
    /// `jvmti::debugger_observes_locals`.
    #[inline]
    pub(crate) fn c_agent_observes_locals(&self) -> bool {
        self.c_agent_observes_locals.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env::bind_startup_envs`: a startup env holds
    /// `can_access_local_variables` (interpreter round i6 wave 132, lane L1).
    /// A no-op with the switch off.
    pub(crate) fn note_c_agent_observes_locals(&self) {
        if self.c_agent_keeps_locals_alive {
            self.c_agent_observes_locals.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Did a startup C agent of this VM acquire a capability that observes
    /// owned monitors or locals ([`Self::c_agent_observes_monitors`], t1
    /// wave 2, lane M)? One `Acquire` load, asked by every compile door.
    #[inline]
    pub(crate) fn c_agent_observes_monitors(&self) -> bool {
        self.c_agent_observes_monitors.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env::bind_startup_envs`: a startup env holds a monitor/locals
    /// observing capability (t1 wave 2, lane M).
    pub(crate) fn note_c_agent_observes_monitors(&self) {
        self.c_agent_observes_monitors.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Did a C agent env of this VM acquire `can_tag_objects`
    /// ([`Self::c_agent_walks_heap`], t1 wave 3, lane M)? One `Acquire` load.
    #[inline]
    pub(crate) fn c_agent_walks_heap(&self) -> bool {
        self.c_agent_walks_heap.load(std::sync::atomic::Ordering::Acquire)
    }

    /// A C agent env acquired `can_tag_objects` (t1 wave 3, lane M). Returns
    /// whether this call raised the gate (t1 wave 5: the caller then refuses
    /// the compiles already in flight, `JitCache::refuse_in_flight_for_object_observer`).
    pub(crate) fn note_c_agent_walks_heap(&self) -> bool {
        !self.c_agent_walks_heap.swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// Does JDWP `VisibleClasses` answer the loader's initiated classes
    /// ([`Self::visible_classes_initiated`], interpreter round i6 wave 118)?
    #[inline]
    pub(crate) fn visible_classes_initiated(&self) -> bool {
        self.visible_classes_initiated
    }

    /// May a breakpoint in a method of the class `class_name` be missed by a
    /// caller compiled after it, which the JIT can expand the method into
    /// without a record (interpreter round i1 wave 40; wave 117's
    /// [`Self::jdk_breakpoint_expansion_set`] narrows the JDK classes to
    /// [`jit_expansion_set::may_expand`])? Such a breakpoint keeps every
    /// method interpreted and takes the whole-cache withdrawal.
    pub(crate) fn breakpoint_may_be_expanded(&self, class_name: &str) -> bool {
        crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(class_name)
            && (!self.jdk_breakpoint_expansion_set || jit_expansion_set::may_expand(class_name))
    }

    /// Does the settle wait walk once and then drain only the arrivals
    /// ([`Self::settle_pending_counter`], interpreter round i6 wave 117)?
    #[inline]
    pub fn settle_pending_counter(&self) -> bool {
        self.settle_pending_counter
    }

    /// Does the settle wait walk only on an arrival
    /// ([`Self::settle_arrival_count`], wave 64)?
    #[inline]
    pub fn settle_arrival_count(&self) -> bool {
        self.settle_arrival_count
    }

    /// [`Self::arrivals`] now (wave 64, lane L6). Read BEFORE the walk it
    /// stamps: a change published after the read moves the count again.
    #[inline]
    pub(crate) fn arrivals(&self) -> u64 {
        self.arrivals.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait for the next change of the debug state `ds` guards, for at most
    /// `bound`, the lock released meanwhile and held again on return
    /// (interpreter round i2 wave 54, lane L6). With [`Self::park_condvar`]
    /// off, the 1 ms poll every such wait made before: the lock released,
    /// then [`DEBUG_STATE_POLL`] of sleep, whatever `bound` says. Answers
    /// whether a notification (not the bound, nor the poll) ended the wait.
    pub(crate) fn await_state_change(
        &self,
        ds: &mut parking_lot::MutexGuard<'_, DebugState>,
        bound: std::time::Duration,
    ) -> bool {
        if self.park_condvar {
            !self.state_changed.wait_for(ds, bound).timed_out()
        } else {
            parking_lot::MutexGuard::unlocked(ds, || std::thread::sleep(DEBUG_STATE_POLL));
            false
        }
    }

    /// Does a suspended unmounted virtual thread list HotSpot's default
    /// continuation rows ([`Self::virtual_continuation_frames`], wave 49)?
    #[inline]
    pub fn virtual_continuation_frames(&self) -> bool {
        self.virtual_continuation_frames
    }

    /// Does this VM's debugger listing splice the JNI natives and the
    /// reflective calls' JDK frames in ([`Self::one_frame_listing`], wave 47)?
    #[inline]
    pub fn one_frame_listing(&self) -> bool {
        self.one_frame_listing
    }

    /// Does a `--compatible` blocking stand-in run its JDK bytecode while a
    /// debugger is attached ([`Self::compatible_blocking_bytecode`], wave 48)?
    #[inline]
    pub fn compatible_blocking_bytecode(&self) -> bool {
        self.compatible_blocking_bytecode
    }

    pub fn new() -> Self {
        Self {
            one_frame_listing: true,
            compatible_blocking_bytecode: true,
            virtual_continuation_frames: true,
            suspend_handshake: true,
            suspend_event_settle: true,
            invoke_reply_settle: true,
            state_suspension_hint: false,
            jvmti_thread_lookup_vm_only: true,
            jvmti_thread_state_one_read: true,
            jvmti_state_suspend_word: false,
            suspend_native_ack: true,
            monitor_exit_on_suspend: true,
            native_frames_window: true,
            native_window_chain: true,
            suspend_settle_long: true,
            park_condvar: true,
            state_changed: parking_lot::Condvar::new(),
            thread_suspend_word: true,
            stw_unparks_parked: false,
            park_gc_blocked: false,
            heap_work_outside_region: true,
            park_region_reuses_deposit: false,
            park_region_at_first_pause: false,
            park_leave_keeps_unfolded_snapshot: true,
            settle_arrival_count: true,
            settle_pending_counter: true,
            jdk_breakpoint_expansion_set: true,
            visible_classes_initiated: true,
            c_listing_shared: true,
            intrinsic_shadow_method_events: false,
            standin_level_locals: false,
            blocked_dead_locals: false,
            blocked_rooted_dead_locals: true,
            c_agent_keeps_locals_alive: true,
            c_agent_observes_locals: std::sync::atomic::AtomicBool::new(false),
            c_agent_observes_monitors: std::sync::atomic::AtomicBool::new(false),
            c_agent_walks_heap: std::sync::atomic::AtomicBool::new(false),
            arrivals: std::sync::atomic::AtomicU64::new(0),
            c_exception_catch: true,
            c_class_hooks: c_class_hooks_switch(),
            c_retransform: c_retransform_switch(),
            c_redefine: c_redefine_switch(),
            c_frame_control: c_frame_control_switch(),
            c_callback_frame_control: true,
            c_in_native: true,
            suspend_at_native_transition: std::sync::atomic::AtomicBool::new(
                true,
            ),
            c_running_native: true,
            running_native_record: std::sync::atomic::AtomicU8::new(0),
            c_funnel_record_from_excuse: true,
            c_start_phase_hook: true,
            bridge_listener_gate: true,
            legacy_jvmti_events: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            generation: std::sync::atomic::AtomicU64::new(0),
            all_methods: std::sync::atomic::AtomicBool::new(false),
            breakpoint_classes: std::sync::atomic::AtomicU64::new(0),
            breakpoint_methods: parking_lot::RwLock::new(HashSet::new()),
            field_watch: std::sync::atomic::AtomicBool::new(false),
            exceptions: std::sync::atomic::AtomicBool::new(false),
            interpret_all: std::sync::atomic::AtomicBool::new(false),
            class_prepare: std::sync::atomic::AtomicBool::new(false),
            session: std::sync::atomic::AtomicBool::new(false),
            jdwp_listening: std::sync::atomic::AtomicBool::new(false),
            proxy_rows_incomplete: std::sync::atomic::AtomicBool::new(false),
            suspension: std::sync::atomic::AtomicBool::new(false),
            suspension_counted: std::sync::atomic::AtomicBool::new(false),
            native_class_prepare: std::sync::atomic::AtomicBool::new(false),
            native_thread_events: std::sync::atomic::AtomicBool::new(false),
            destroy_java_vm_split: std::sync::atomic::AtomicBool::new(false),
            native_events: std::sync::atomic::AtomicU32::new(0),
            jdwp_native_events: std::sync::atomic::AtomicU32::new(0),
            native_class_load: std::sync::atomic::AtomicBool::new(false),
            native_class_file_load_hook: std::sync::atomic::AtomicBool::new(false),
            native_env_present: std::sync::atomic::AtomicBool::new(false),
            blocking_standins: std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)),
            jvmti_phase: std::sync::atomic::AtomicU8::new(JVMTI_PHASE_LIVE),
            c_jvmti_phases: c_jvmti_phases_switch(),
            c_lambda_classes: true,
            c_lambda_methods: true,
            c_primitive_classes: true,
            c_lambda_code: true,
            c_lambda_frames: true,
            c_lambda_events: true,
            jni_lambda_method_ids: true,
            method_entry: RequestVmsEdge::new(METHOD_ENTRY_COUNT),
            method_events: std::sync::atomic::AtomicU8::new(0),
            stops: std::sync::atomic::AtomicBool::new(false),
            jdwp_deferred_start: std::sync::atomic::AtomicBool::new(false),
            frame_writes: std::sync::atomic::AtomicBool::new(false),
            compiled_home_writes: std::sync::atomic::AtomicBool::new(false),
            withdrawal: std::sync::atomic::AtomicU8::new(0),
            withdrawal_linger_until: std::sync::atomic::AtomicU64::new(0),
            withdrawal_generation: std::sync::atomic::AtomicU64::new(0),
            withdrawals_held: std::sync::atomic::AtomicU64::new(0),
            linger_clock_origin: std::time::Instant::now(),
            breakpoint_generation: std::sync::atomic::AtomicU64::new(0),
            caller_verdicts: parking_lot::Mutex::new(HashMap::new()),
            scoped_method_events: true,
            exceptions_from_compiled: true,
            compiled_exception_requests: RequestVmsEdge::new(COMPILED_EXCEPTION_COUNT),
            field_watch_scope: field_watch_scope::FieldWatchScope::new(),
            method_event_class_bits: std::sync::atomic::AtomicU64::new(0),
            method_event_classes: parking_lot::RwLock::new(HashSet::new()),
            method_event_specs: parking_lot::RwLock::new(Vec::new()),
            method_event_slots_seen: std::sync::atomic::AtomicUsize::new(0),
            method_event_degraded: std::sync::atomic::AtomicBool::new(false),
            method_event_degraded_published: std::sync::atomic::AtomicBool::new(false),
            method_event_scope_armed: std::sync::atomic::AtomicBool::new(false),
            method_event_scope_generation: std::sync::atomic::AtomicU64::new(0),
            method_event_scope_published: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Is this VM's compiled code held withdrawn for a debugger or an agent
    /// that needs every method interpreted (wave 37)? One relaxed load,
    /// asked at the interpreter's tier-up strides (never per call); `false`
    /// without a debugger or an agent.
    #[inline(always)]
    pub fn every_body_withdrawn(&self) -> bool {
        self.withdrawal.load(std::sync::atomic::Ordering::Relaxed) != 0
    }

    /// Raise (`armed`) or lower one writer's bit of
    /// [`Self::every_body_withdrawn`]; answers the bits as they were before.
    pub(crate) fn set_withdrawal_source(&self, bit: u8, armed: bool) -> u8 {
        use std::sync::atomic::Ordering;
        if armed {
            self.withdrawal.fetch_or(bit, Ordering::AcqRel)
        } else {
            self.withdrawal.fetch_and(!bit, Ordering::AcqRel)
        }
    }

    /// Do the interpreter's tier-up strides offer nothing to compile (wave
    /// 39, lane L1)? While a source holds the code withdrawn
    /// ([`Self::every_body_withdrawn`]), and for [`WITHDRAWAL_LINGER_MS`]
    /// after the last one went, so a debugger's next step request finds
    /// nothing compiled and takes no second flush. One relaxed load while a
    /// source is armed or nothing lingers; a second one, and a clock read,
    /// only during a linger. Asked at the tier-up strides only, never per
    /// call.
    #[inline(always)]
    pub fn tier_up_held(&self) -> bool {
        if self.every_body_withdrawn() {
            return true;
        }
        let until = self
            .withdrawal_linger_until
            .load(std::sync::atomic::Ordering::Relaxed);
        until != 0 && self.linger_still_running(until)
    }

    /// The linger's clock read: `true` before `until`; after it, the linger
    /// is forgotten, so the next stride pays one load again.
    #[cold]
    #[inline(never)]
    pub(in crate::debug) fn linger_still_running(&self, until: u64) -> bool {
        if self.linger_clock_ms() < until {
            return true;
        }
        let _ = self.withdrawal_linger_until.compare_exchange(
            until,
            0,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        false
    }

    /// Milliseconds since this VM's gates were created (monotonic).
    pub(in crate::debug) fn linger_clock_ms(&self) -> u64 {
        u64::try_from(self.linger_clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The last source of [`Self::every_body_withdrawn`] just went: keep the
    /// tier-up strides closed for [`WITHDRAWAL_LINGER_MS`] more (wave 39).
    pub(crate) fn begin_withdrawal_linger(&self) {
        if WITHDRAWAL_LINGER_MS == 0 {
            return;
        }
        let until = self
            .linger_clock_ms()
            .saturating_add(WITHDRAWAL_LINGER_MS)
            .max(1);
        self.withdrawal_linger_until
            .store(until, std::sync::atomic::Ordering::Relaxed);
    }

    /// A source armed again: the linger, if one ran, is over (the armed bit
    /// holds the strides now). Answers whether one was still running.
    pub(crate) fn end_withdrawal_linger(&self) -> bool {
        let until = self
            .withdrawal_linger_until
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        until != 0 && self.linger_clock_ms() < until
    }

    /// The code cache's generation the last whole-cache withdrawal left
    /// (wave 39); `0` before the first.
    pub(crate) fn withdrawal_generation(&self) -> u64 {
        self.withdrawal_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record the code cache's generation a whole-cache withdrawal just left.
    pub(crate) fn note_withdrawal_generation(&self, generation: u64) {
        self.withdrawal_generation
            .store(generation, std::sync::atomic::Ordering::Release);
    }

    /// Count an arm that withdrew nothing (wave 39).
    pub(crate) fn note_withdrawal_held(&self) {
        self.withdrawals_held
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Arms that withdrew nothing so far (wave 39).
    pub(crate) fn withdrawals_held(&self) -> u64 {
        self.withdrawals_held
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Is a `ThreadReference.Stop` pending for a thread of this VM (wave
    /// 25)? One load, asked by the unwinder for every exception it routes in
    /// a build with a JDWP server.
    #[inline(always)]
    pub fn stop_pending(&self) -> bool {
        self.stops.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is this VM's `-agentlib:jdwp` start deferred to a matching throw
    /// (`onthrow=` / `onuncaught=y`; interpreter round i7 wave 144, lane L1,
    /// [`start_deferred_jdwp_agent`])? One load, asked by the unwinder for
    /// every exception it routes.
    #[inline(always)]
    pub fn jdwp_deferred_start_armed(&self) -> bool {
        self.jdwp_deferred_start
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Defer the agent's start to a matching throw (wave 144).
    pub(crate) fn arm_jdwp_deferred_start(&self) {
        self.jdwp_deferred_start
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Claim the deferred start, once: answers whether this caller does it
    /// (two threads throwing at once start it once, as the back end's
    /// `initMonitor` serializes `initialize`).
    pub(crate) fn take_jdwp_deferred_start(&self) -> bool {
        self.jdwp_deferred_start
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    /// Is a deferred `StackFrame.SetValues` waiting for some thread of this
    /// VM (wave 41, [`DebugState::deferred_frame_writes`])? One load, asked
    /// at every blocking region's exit in a build with a JDWP server.
    #[inline(always)]
    pub(crate) fn frame_writes_pending(&self) -> bool {
        self.frame_writes.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise or lower [`Self::frame_writes_pending`]; under the debug-state
    /// lock, from whether [`DebugState::deferred_frame_writes`] is empty.
    pub(crate) fn set_frame_writes_pending(&self, pending: bool) {
        self.frame_writes
            .store(pending, std::sync::atomic::Ordering::Release);
    }

    /// Is a `SetValues` to a compiled activation waiting for its post-call
    /// exit in this VM (interpreter round i6 wave 123, lane L2;
    /// [`DebugState::compiled_home_writes`])? One load, asked by the deopt
    /// resume's frame build.
    #[inline(always)]
    pub(crate) fn compiled_home_writes_pending(&self) -> bool {
        self.compiled_home_writes
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Raise or lower [`Self::compiled_home_writes_pending`]; under the
    /// debug-state lock, from whether [`DebugState::compiled_home_writes`] is
    /// empty.
    pub(crate) fn set_compiled_home_writes_pending(&self, pending: bool) {
        self.compiled_home_writes
            .store(pending, std::sync::atomic::Ordering::Release);
    }

    /// Is this VM before JVMTI's live phase for its C JVMTI envs: in the
    /// primordial or the start phase (wave 14; `jvmti_phase`)? One
    /// load, asked by `SharedVm::set_init_level` and the C table's phase
    /// checks; the functions JVMTI allows in the OnLoad and live phases only
    /// answer `JVMTI_ERROR_WRONG_PHASE` in both.
    #[inline(always)]
    pub fn jvmti_start_phase(&self) -> bool {
        matches!(
            self.jvmti_phase(),
            JVMTI_PHASE_PRIMORDIAL | JVMTI_PHASE_START
        )
    }

    /// Is this VM outside JVMTI's live phase for its C JVMTI envs: primordial,
    /// start or (wave 55) dead? The live-only and OnLoad-or-live functions of
    /// the C table answer `JVMTI_ERROR_WRONG_PHASE` then.
    #[inline(always)]
    pub fn jvmti_outside_live(&self) -> bool {
        self.jvmti_phase() != JVMTI_PHASE_LIVE
    }

    /// This VM's `jvmtiPhase` for its C JVMTI envs (the `jvmti_phase` field,
    /// wave 55).
    #[inline(always)]
    pub fn jvmti_phase(&self) -> u8 {
        self.jvmti_phase.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Move the `jvmti_phase` field from `from` to `to`; answers whether this
    /// call made the move, so each transition happens once.
    pub(crate) fn advance_jvmti_phase(&self, from: u8, to: u8) -> bool {
        use std::sync::atomic::Ordering;
        self.jvmti_phase
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Does the C JVMTI table know the lambda proxy classes
    /// ([`Self::c_lambda_classes`], wave 55)?
    #[inline]
    pub fn c_lambda_classes(&self) -> bool {
        self.c_lambda_classes
    }

    /// Does the C JVMTI table know a lambda proxy class's methods
    /// ([`Self::c_lambda_methods`], wave 56)? Only with the classes
    /// themselves ([`Self::c_lambda_classes`]).
    #[inline]
    pub fn c_lambda_methods(&self) -> bool {
        self.c_lambda_methods && self.c_lambda_classes
    }

    /// Does the C JVMTI table answer a lambda proxy method's spun code
    /// ([`Self::c_lambda_code`], wave 57)? Only with its methods
    /// ([`Self::c_lambda_methods`]).
    #[inline]
    pub fn c_lambda_code(&self) -> bool {
        self.c_lambda_code && self.c_lambda_methods()
    }

    /// Does a lambda dispatch record the proxy method it enters for the C
    /// table's stack functions ([`Self::c_lambda_frames`], wave 60) and, since
    /// the i54-L1 retirement, for a JDWP session's `ThreadReference.Frames`
    /// (`interpreter::debugger_listing_splices`) -- from the moment the JDWP
    /// server listens, so that a debugger attaching later finds the calls in
    /// progress listed? One plain test, then up to three loads (a C env is
    /// bound, a debugger is attached, the server listens), on a lambda call.
    #[inline(always)]
    pub fn c_lambda_frames_armed(&self) -> bool {
        self.c_lambda_frames
            && self.c_lambda_methods()
            && (self.native_env_present()
                || self.session_attached()
                || self
                    .jdwp_listening
                    .load(std::sync::atomic::Ordering::Acquire))
    }

    /// Does a lambda dispatch that records its proxy frame also post the
    /// proxy method's `MethodEntry` / `MethodExit` ([`Self::c_lambda_events`],
    /// wave 60)? Asked only once [`Self::c_lambda_frames_armed`] holds.
    #[inline]
    pub fn c_lambda_events(&self) -> bool {
        self.c_lambda_events
    }

    /// Does JNI's `GetMethodID` answer a lambda proxy's own method with the
    /// proxy's `jmethodID` ([`Self::jni_lambda_method_ids`], wave 60)?
    #[inline]
    pub fn jni_lambda_method_ids_armed(&self) -> bool {
        self.jni_lambda_method_ids && self.c_lambda_methods() && self.native_env_present()
    }

    /// Does the C JVMTI table answer the primitive classes
    /// ([`Self::c_primitive_classes`], wave 56)?
    #[inline]
    pub fn c_primitive_classes(&self) -> bool {
        self.c_primitive_classes
    }

    /// Does the C JVMTI table model the primordial and the dead phase and
    /// deliver `VMStart` ([`Self::c_jvmti_phases`], wave 55)?
    #[inline]
    pub fn c_jvmti_phases(&self) -> bool {
        self.c_jvmti_phases
    }

    /// Is a debugger attached to this VM? One load, asked by every blocking
    /// native transition (`interpreter::publish_blocked_frames`).
    #[inline(always)]
    pub fn session_attached(&self) -> bool {
        self.session.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a JDWP method entry or exit request in force in this VM (wave 26),
    /// or does a JVMTI env of it listen for `MethodEntry` / `MethodExit`
    /// (wave 27) or `SingleStep` (wave 29, [`Self::jvmti_frames_armed`]), or
    /// is a breakpoint or a step request in force (wave 28,
    /// [`Self::java_frames_armed`])? One relaxed load, asked by the native-call funnel for every
    /// native call in a build with a JDWP server; false without a debugger
    /// or an agent.
    #[inline(always)]
    pub fn method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Relaxed)
            & !METHOD_EVENTS_NATIVE_BIND
            != 0
    }

    /// [`Self::method_events_armed`], or the funnel posts `NativeMethodBind`
    /// for the registered natives it runs ([`Self::native_bind_hook_armed`];
    /// interpreter round i3 wave 83, lane L1). The one relaxed load the
    /// native-call funnel and its leaf twin read before their debugger hook,
    /// as they read `method_events_armed` before; false without a debugger
    /// or an agent.
    #[inline(always)]
    pub fn native_hooks_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Relaxed)
            != 0
    }

    /// The bit of [`Self::native_hooks_armed`] for a C env's
    /// `NativeMethodBind` (wave 83, [`METHOD_EVENTS_NATIVE_BIND`]).
    #[inline]
    pub fn native_bind_hook_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_NATIVE_BIND
            != 0
    }

    /// The C JVMTI table's publication of [`Self::native_bind_hook_armed`]
    /// (wave 83; `jvmti::native_env::events_more::refresh_event_gates`).
    pub(crate) fn set_native_bind_hook(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_NATIVE_BIND, armed);
    }

    /// The JDWP half of [`Self::method_events_armed`] (wave 27): a JDWP
    /// method entry or exit request is in force.
    #[inline]
    pub fn jdwp_method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JDWP
            != 0
    }

    /// The JVMTI half of [`Self::method_events_armed`] (wave 27): an env of
    /// this VM listens for `MethodEntry` or `MethodExit`.
    #[inline]
    pub fn jvmti_method_events_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JVMTI
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a breakpoint or a step
    /// request (wave 28): the native-call funnel asks whether the Java
    /// method a native stands in for is concerned
    /// ([`Self::concerns_method`]) and, if so, runs its bytecode instead
    /// (`interpreter::run_stood_in_java_method`).
    #[inline]
    pub fn java_frames_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_FRAMES
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a JVMTI `SingleStep`
    /// listener (wave 29): every stood-in Java method the funnel's hook
    /// accepts runs its bytecode, in a frame, so the agent's steps enter it
    /// (`interpreter::run_stood_in_java_method`). JVMTI breakpoints ride
    /// [`Self::java_frames_armed`] through [`publish_debugger_gates`].
    #[inline]
    pub fn jvmti_frames_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_JVMTI_FRAMES
            != 0
    }

    /// The bit of [`Self::method_events_armed`] for a JDWP session (wave 46,
    /// [`METHOD_EVENTS_BLOCKING`]).
    #[inline]
    pub fn blocking_standins_armed(&self) -> bool {
        self.method_events
            .load(std::sync::atomic::Ordering::Acquire)
            & METHOD_EVENTS_BLOCKING
            != 0
    }

    /// Which of [`BLOCKING_STANDINS`] `callback` is the registered native of
    /// (wave 46): bit `i` for entry `i`, 0 for none. A few loads; asked by the
    /// native-call funnel's debugger hook only while
    /// [`Self::blocking_standins_armed`].
    #[inline]
    pub fn blocking_standin_matches(&self, callback: usize) -> u8 {
        if callback == 0 {
            return 0;
        }
        let mut matches = 0u8;
        for (i, slot) in self.blocking_standins.iter().enumerate() {
            if slot.load(std::sync::atomic::Ordering::Acquire) == callback {
                matches |= 1 << i;
            }
        }
        matches
    }

    /// Raise or lower one writer's bit of [`Self::method_events_armed`].
    pub(in crate::debug) fn set_method_events_bit(&self, bit: u8, armed: bool) {
        use std::sync::atomic::Ordering;
        if armed {
            self.method_events.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.method_events.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// The C JVMTI table's publication of its half of
    /// [`Self::method_events_armed`] (wave 27): whether this VM's JVMTI
    /// manager has a `MethodEntry` or `MethodExit` listener
    /// (`jvmti::native_env::refresh_method_events_gate`).
    pub(crate) fn set_jvmti_method_events(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_JVMTI, armed);
    }

    /// The C JVMTI table's publication of [`Self::jvmti_frames_armed`]
    /// (wave 29): whether this VM's JVMTI manager has a `SingleStep`
    /// listener (`jvmti::native_env::refresh_method_events_gate`).
    pub(crate) fn set_jvmti_single_step(&self, armed: bool) {
        self.set_method_events_bit(METHOD_EVENTS_JVMTI_FRAMES, armed);
    }

    /// Is a JDWP suspension (of any thread, or VM-wide) in force? One load;
    /// published with the other gates by [`publish_debugger_gates`].
    #[inline(always)]
    pub fn suspension_in_force(&self) -> bool {
        self.suspension.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is any suspension counted, an ended (ZOMBIE-listed) thread's own
    /// included ([`Self::suspension_counted`])? One load; published with
    /// [`Self::suspension_in_force`], which it implies.
    #[inline(always)]
    pub fn suspension_counted(&self) -> bool {
        self.suspension_counted
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Did a C env bind after lambda proxies existed ([`Self::proxy_rows_incomplete`])?
    #[inline]
    pub fn proxy_rows_incomplete(&self) -> bool {
        self.proxy_rows_incomplete
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s publication of [`Self::proxy_rows_incomplete`].
    pub(crate) fn set_proxy_rows_incomplete(&self) {
        self.proxy_rows_incomplete
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// `vm_init`'s publication of [`Self::jdwp_listening`], before it spawns
    /// the JDWP server.
    pub(crate) fn set_jdwp_listening(&self) {
        self.jdwp_listening
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// A debugger attached (`true`) or detached (`false`); the JDWP server
    /// sets it.
    pub(crate) fn set_session_attached(&self, attached: bool) {
        use std::sync::atomic::Ordering;
        self.session.store(attached, Ordering::Release);
        if attached {
            self.running_native_record
                .fetch_or(RUNNING_NATIVE_SESSION, Ordering::AcqRel);
        } else {
            self.running_native_record
                .fetch_and(!RUNNING_NATIVE_SESSION, Ordering::AcqRel);
        }
    }

    /// Does the native-call funnel record the native a thread runs
    /// (`interpreter::enter_running_native`): a debugger is attached, or a C
    /// JVMTI env is bound with [`Self::c_running_native`] on (wave 61, lane
    /// L6)? One load, the funnel's only test of it.
    #[inline(always)]
    pub fn running_native_armed(&self) -> bool {
        self.running_native_record
            .load(std::sync::atomic::Ordering::Acquire)
            & (RUNNING_NATIVE_SESSION | RUNNING_NATIVE_C_ENV)
            != 0
    }

    /// Does a leaf native take the full native-call funnel
    /// (`vm_exec::safe_native_call_leaf_impl`'s debugger test): whenever
    /// [`Self::running_native_armed`], and also while a C JVMTI env counts
    /// running natives from the excuse word alone
    /// ([`RUNNING_NATIVE_C_LEAF_ONLY`], wave 133, lane L1), since only the
    /// full funnel raises that word. One load, as before.
    #[inline(always)]
    pub fn leaf_takes_full_funnel(&self) -> bool {
        self.running_native_record
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// Does the C JVMTI table's `GetThreadState` count a thread running a
    /// funnel native that stands in for a JDK `native` method as in native
    /// ([`Self::c_running_native`] with a C JVMTI env bound; wave 61)?
    #[inline]
    pub fn c_running_native_armed(&self) -> bool {
        self.c_running_native && self.native_env_present()
    }

    /// Is a JDWP `ClassPrepare` request in force in this VM? One load, asked
    /// by class initialization when it prepares a class.
    #[inline(always)]
    pub fn class_prepare_armed(&self) -> bool {
        self.class_prepare
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env of this VM take `ClassPrepare` (wave 13)? One
    /// load, asked by class initialization when it prepares a class.
    #[inline(always)]
    pub fn native_class_prepare_armed(&self) -> bool {
        self.native_class_prepare
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a C JVMTI env bound to this VM (wave 45)? One load, asked by a
    /// thread opening its blocking region's inspection window.
    #[inline(always)]
    pub fn native_env_present(&self) -> bool {
        self.native_env_present
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a thread entering a JNI native publish that it runs in native
    /// ([`Self::c_in_native`] with a C JVMTI env bound; wave 58)? One plain
    /// test, then one load.
    #[inline(always)]
    pub fn c_in_native_armed(&self) -> bool {
        self.c_in_native && self.native_env_present()
    }

    /// Does a suspended thread in native stop at its next native -> VM
    /// transition ([`Self::suspend_at_native_transition`], gci i16/sn)?
    #[inline(always)]
    pub fn suspend_at_native_transition(&self) -> bool {
        self.suspend_at_native_transition
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Arm or disarm [`Self::suspend_at_native_transition`] on a built VM
    /// (gci i16/sn): unit tests only.
    #[cfg(test)]
    pub(crate) fn set_suspend_at_native_transition(&self, on: bool) {
        self.suspend_at_native_transition
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// `native_env`'s publication of [`Self::native_env_present`] (wave 45).
    ///
    /// `excuse_carries_callback`: the VM's native-excuse word records the
    /// callback of every funnel native it is raised for
    /// (`GcBarrier::native_excuse_enabled` and `excused_native_callback`).
    /// With [`Self::c_funnel_record_from_excuse`] on as well, the funnel
    /// keeps no record of its own for the C table (wave 133, lane L1).
    pub(crate) fn set_native_env_present(&self, excuse_carries_callback: bool) {
        self.native_env_present
            .store(true, std::sync::atomic::Ordering::Release);
        if self.c_running_native {
            let bit = if self.c_funnel_record_from_excuse && excuse_carries_callback {
                RUNNING_NATIVE_C_LEAF_ONLY
            } else {
                RUNNING_NATIVE_C_ENV
            };
            self.running_native_record
                .fetch_or(bit, std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// `native_env`'s publication of [`Self::native_class_prepare_armed`].
    pub(crate) fn set_native_class_prepare(&self, armed: bool) {
        self.native_class_prepare
            .store(armed, std::sync::atomic::Ordering::Release);
    }

    /// Does a C JVMTI env listen for `ThreadStart` / `ThreadEnd`
    /// ([`Self::native_thread_events`], wave 52)?
    #[inline]
    pub fn native_thread_events_armed(&self) -> bool {
        self.native_thread_events
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Has `main` ended, so that VM thread 0 is the `DestroyJavaVM` thread
    /// on the wire ([`Self::destroy_java_vm_split`], wave 104)? One load.
    #[inline]
    pub fn destroy_java_vm_split(&self) -> bool {
        self.destroy_java_vm_split
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env of this VM listen for wave-63 event `event`
    /// ([`Self::native_events`])? One relaxed load.
    #[inline(always)]
    pub fn native_event_armed(&self, event: NativeEvent) -> bool {
        (self.native_events.load(std::sync::atomic::Ordering::Relaxed)
            | self
                .jdwp_native_events
                .load(std::sync::atomic::Ordering::Relaxed))
            & event as u32
            != 0
    }

    /// Does a JDWP monitor event request (43-46) listen (interpreter round
    /// i5 wave 103, lane L1; [`Self::jdwp_native_events`])? One load.
    #[inline]
    pub fn jdwp_monitor_events_armed(&self) -> bool {
        self.jdwp_native_events
            .load(std::sync::atomic::Ordering::Acquire)
            & NativeEvent::Monitor as u32
            != 0
    }

    /// The address of [`Self::native_events`], stable for the VM's life (j27
    /// wave 4, lane CLONEIR): the inline array `clone()` of compiled code
    /// tests `cratonvm_jit::direct_helpers::JVMTI_ALLOC_EVENT_BITS` in it
    /// (`DirectHelperTable::jvmti_alloc_events_word`). Only the C table's
    /// word: the JDWP word ([`Self::jdwp_native_events`]) never carries an
    /// allocation event.
    pub(crate) fn native_events_addr(&self) -> usize {
        &self.native_events as *const std::sync::atomic::AtomicU32 as usize // Cast: a data address
    }

    /// `native_env`'s publication of [`Self::native_event_armed`].
    pub(crate) fn set_native_events(&self, mask: u32) {
        self.native_events
            .store(mask, std::sync::atomic::Ordering::Release);
    }

    /// `native_env`'s publication of [`Self::native_thread_events_armed`].
    pub(crate) fn set_native_thread_events(&self, armed: bool) {
        self.native_thread_events
            .store(armed, std::sync::atomic::Ordering::Release);
    }

    /// Does a C JVMTI env listen for `ClassLoad`
    /// ([`Self::native_class_load`], wave 53)? One load per class defined.
    #[inline]
    pub fn native_class_load_armed(&self) -> bool {
        self.native_class_load
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Does a C JVMTI env take `ClassFileLoadHook`
    /// ([`Self::native_class_file_load_hook`], wave 53)? One load per class
    /// the load path offers to the transformer chain.
    #[inline]
    pub fn native_class_file_load_hook_armed(&self) -> bool {
        self.native_class_file_load_hook
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// `native_env`'s publication of [`Self::native_class_load_armed`] and
    /// [`Self::native_class_file_load_hook_armed`].
    pub(crate) fn set_native_class_hooks(&self, load: bool, file_load_hook: bool) {
        self.native_class_load
            .store(load, std::sync::atomic::Ordering::Release);
        self.native_class_file_load_hook
            .store(file_load_hook, std::sync::atomic::Ordering::Release);
    }

    /// Is a JDWP field watch in force in this VM? One load; the field
    /// bytecodes ask it only after the process-wide pre-filter.
    #[inline(always)]
    pub fn field_watch_armed(&self) -> bool {
        self.field_watch.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Is a JDWP `Exception` request in force in this VM? One load, asked by
    /// the interpreter's unwinder for every exception it routes.
    #[inline(always)]
    pub fn exception_events_armed(&self) -> bool {
        self.exceptions.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Must the method `name` + `descriptor` of class `class_id` run
    /// interpreted for the debugger? The JIT doors' question (wave 10): the
    /// method concerns the requests ([`Self::concerns_method`]), or a field
    /// watch or exception request makes every method interpreted. Meaningful
    /// only while `breakpoints_active` is set.
    #[inline]
    pub fn requires_interpreter(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        self.interpret_all
            .load(std::sync::atomic::Ordering::Acquire)
            || self.concerns_method(class_id, name, descriptor)
    }

    /// Must EVERY method run interpreted for the debugger — a step request, a
    /// suspension, a field watch or an exception request in force, or (wave
    /// 40) a breakpoint in a JDK class's method? The answer
    /// of [`Self::requires_interpreter`] with no method in hand (interpreter
    /// round i1 wave 15: a method-entry body's loop-exit verdict,
    /// `interpreter::every_compiled_frame_may_leave`). Meaningful only while
    /// `breakpoints_active` is set.
    #[inline]
    pub fn requires_interpreter_everywhere(&self) -> bool {
        self.interpret_all
            .load(std::sync::atomic::Ordering::Acquire)
            || self.all_methods.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The publication count; changes whenever an answer may have.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Do the requests in force concern the method `name` + `descriptor` of
    /// class `class_id`? Meaningful only while `breakpoints_active` is set.
    #[inline]
    pub fn concerns_method(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        if self.all_methods.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        // Interpreter round i4 wave 93 (lane L1): or a class-scoped method
        // event request covers the method's class (one load otherwise).
        // Wave 95 (lane L1): or a class-scoped field watch covers it.
        self.holds_breakpoint(class_id, name, descriptor)
            || self.in_method_event_scope(class_id)
            || self.field_watch_scope.contains(class_id)
    }

    /// Must class `class_id`'s methods run interpreted for a JDWP field watch
    /// served by class (interpreter round i4 wave 95, lane L1;
    /// `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`, [`field_watch_scope`])? One load
    /// for a class outside the scope's bits; the set is empty whenever no
    /// such watch is in force. Asked by the compile doors
    /// (`interpreter::breakpoint_bars_compiling`) beside
    /// [`Self::in_method_event_scope`].
    #[inline]
    pub fn in_field_watch_scope(&self, class_id: u32) -> bool {
        self.field_watch_scope.contains(class_id)
    }

    /// Does a class-scoped JDWP method event request in force concern every
    /// method of class `class_id` (interpreter round i4 wave 93, lane L1;
    /// [`Self::scoped_method_events`])? One load for a class outside the
    /// scope's bits, one more bit test and a read-locked lookup for a class
    /// sharing one. Asked whether or not `breakpoints_active` is set (the
    /// compile doors, `interpreter::breakpoint_bars_compiling`); the set is
    /// empty whenever no such request is in force.
    #[inline]
    pub fn in_method_event_scope(&self, class_id: u32) -> bool {
        let bit = 1u64 << (class_id % 64);
        if self
            .method_event_class_bits
            .load(std::sync::atomic::Ordering::Acquire)
            & bit
            == 0
        {
            return false;
        }
        self.method_event_classes.read().contains(&class_id)
    }

    /// Does this VM serve JDWP `Exception` requests from compiled code when
    /// they allow it ([`Self::exceptions_from_compiled`], wave 93)?
    #[inline]
    pub fn jdwp_exceptions_from_compiled(&self) -> bool {
        self.exceptions_from_compiled
    }

    /// Is a class-scoped method event scope resolved, so a class prepared now
    /// must be matched against it ([`join_method_event_scope`], wave 93)?
    #[inline(always)]
    pub fn method_event_scope_armed(&self) -> bool {
        self.method_event_scope_armed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Add `class_ids` to [`Self::method_event_classes`] (`replace`: they are
    /// the whole set, plus any id of a class defined at or after `carry_from`
    /// already there, which a [`join_method_event_scope`] may have added
    /// while the caller resolved), republishing the bits and bumping the
    /// generations. Answers the ids that were not in the set before (wave 93).
    pub(in crate::debug) fn store_method_event_classes(
        &self,
        class_ids: &[u32],
        replace: bool,
        carry_from: usize,
    ) -> Vec<u32> {
        use std::sync::atomic::Ordering;
        let mut set = self.method_event_classes.write();
        let gained: Vec<u32> = class_ids
            .iter()
            .copied()
            .filter(|id| !set.contains(id))
            .collect();
        if replace {
            let carried: Vec<u32> = set
                .iter()
                .copied()
                // Widening: a class id to a store slot index.
                .filter(|&id| id as usize >= carry_from)
                .collect();
            set.clear();
            set.extend(carried);
        }
        set.extend(class_ids.iter().copied());
        let bits = set
            .iter()
            .fold(0u64, |bits, &id| bits | (1u64 << (id % 64)));
        self.method_event_class_bits.store(bits, Ordering::Release);
        drop(set);
        self.method_event_scope_generation
            .fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::Release);
        gained
    }

    /// Forget every class-scoped method event scope (wave 93): no such
    /// request is in force.
    pub(in crate::debug) fn clear_method_event_scope(&self) {
        use std::sync::atomic::Ordering;
        self.method_event_scope_armed.store(false, Ordering::Release);
        self.method_event_specs.write().clear();
        self.method_event_classes.write().clear();
        self.method_event_class_bits.store(0, Ordering::Release);
        self.method_event_degraded.store(false, Ordering::Release);
        self.method_event_slots_seen.store(0, Ordering::Release);
        self.method_event_scope_generation
            .fetch_add(1, Ordering::AcqRel);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Does a JDWP or JVMTI breakpoint sit in the method `name` +
    /// `descriptor` of class `class_id` (its declaring class)? The per-method
    /// set alone, whatever else is in force (interpreter round i1 wave 39,
    /// lane L1): the compile doors ask it so that no compile binds or splices
    /// such a method (`interpreter::breakpoint_bars_compiling`). One load
    /// while no breakpoint is set, and one more bit test before the exact
    /// lookup under a read lock, which only a class holding a breakpoint (or
    /// sharing its bit) pays.
    #[inline]
    pub fn holds_breakpoint(&self, class_id: u32, name: &str, descriptor: &str) -> bool {
        let bit = 1u64 << (class_id % 64);
        if self
            .breakpoint_classes
            .load(std::sync::atomic::Ordering::Acquire)
            & bit
            == 0
        {
            return false;
        }
        self.breakpoint_methods
            .read()
            .contains(&(u64::from(class_id), jdwp_method_id(name, descriptor)))
    }

    /// Does any JDWP or JVMTI breakpoint stand in this VM (interpreter round
    /// i1 wave 44, lane L1)? One load.
    #[inline]
    pub fn any_breakpoint(&self) -> bool {
        self.breakpoint_classes
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    /// The generation of the breakpoint set (wave 44): changes whenever a
    /// method gains or loses its last breakpoint.
    #[inline]
    pub fn breakpoint_generation(&self) -> u64 {
        self.breakpoint_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The `jdwp_method_id` of every method holding a breakpoint, whatever
    /// its class (wave 44): what a caller's invoke instructions are matched
    /// against by name and descriptor
    /// (`interpreter::breakpoint_bars_compiling`).
    pub fn breakpoint_method_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .breakpoint_methods
            .read()
            .iter()
            .map(|&(_, method_id)| method_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The verdict recorded for the method `(class_id, method_id)` at
    /// breakpoint generation `generation`, if any (wave 44).
    pub fn caller_verdict(&self, class_id: u32, method_id: u64, generation: u64) -> Option<bool> {
        self.caller_verdicts
            .lock()
            .get(&(class_id, method_id))
            .filter(|&&(at, _)| at == generation)
            .map(|&(_, answer)| answer)
    }

    /// Record a verdict of [`Self::caller_verdict`]; answers whether it is
    /// new at this generation (the positive control prints each new `true`
    /// once).
    pub fn record_caller_verdict(
        &self,
        class_id: u32,
        method_id: u64,
        generation: u64,
        answer: bool,
    ) -> bool {
        let previous = self
            .caller_verdicts
            .lock()
            .insert((class_id, method_id), (generation, answer));
        previous != Some((generation, answer))
    }

    pub(in crate::debug) fn store(&self, all_methods: bool, methods: HashSet<(u64, u64)>) {
        use std::sync::atomic::Ordering;
        let bits = methods.iter().fold(0u64, |bits, &(class_id, _)| {
            bits | (1u64 << (class_id % 64))
        });
        // The exact set first: a reader that sees a class bit finds its methods.
        {
            let mut set = self.breakpoint_methods.write();
            if *set != methods {
                let none_left = methods.is_empty();
                *set = methods;
                // Wave 44: every caller verdict judged against the old set is
                // stale.
                self.breakpoint_generation.fetch_add(1, Ordering::AcqRel);
                if none_left {
                    self.caller_verdicts.lock().clear();
                }
            }
        }
        self.breakpoint_classes.store(bits, Ordering::Release);
        self.all_methods.store(all_methods, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Store the field-watch and exception flags (and `interpret_all`, their
    /// union with `jdk_breakpoint`, wave 40), answering the field-watch
    /// flag's previous value and whether `interpret_all` just rose.
    /// Interpreter round i4 wave 93 (lane L1): `interpret_all` takes the
    /// exception requests that need the interpreter
    /// (`exceptions_need_interpreter`), which is `exceptions` unless the VM
    /// serves them from compiled code.
    ///
    /// Wave 95 (lane L1): likewise the field watches that need the
    /// interpreter (`field_watch_needs_interpreter`), which is `field_watch`
    /// unless a class-scoped JDWP watch serves them
    /// ([`field_watch_scope`]).
    pub(in crate::debug) fn store_subject_flags(
        &self,
        field_watch: bool,
        field_watch_needs_interpreter: bool,
        exceptions: bool,
        exceptions_need_interpreter: bool,
        jdk_breakpoint: bool,
    ) -> (bool, bool) {
        use std::sync::atomic::Ordering;
        let interpret_all =
            field_watch_needs_interpreter || exceptions_need_interpreter || jdk_breakpoint;
        let was_interpret_all = self.interpret_all.swap(interpret_all, Ordering::AcqRel);
        self.exceptions.store(exceptions, Ordering::Release);
        let was_watching = self.field_watch.swap(field_watch, Ordering::AcqRel);
        (was_watching, interpret_all && !was_interpret_all)
    }
}

impl Default for DebuggerGates {
    fn default() -> Self {
        Self::new()
    }
}

/// Did a matcher spend a request since the last look (a `Count` modifier
/// reached zero; interpreter round i1 wave 46, lane L1,
/// [`events::SPENT_COUNT_RELEASES_GATES`])? The caller, holding the
/// debug-state lock, then republishes the gates ([`publish_debugger_gates`])
/// so the spent request's stop arming them. `CRATONVM_FRAME_TRACE=1` prints
/// `[JDWP_COUNT_SPENT] request=<id>` per spent request: the positive
/// control. One empty-`Vec` take in the common case.
pub(crate) fn take_spent_requests(ds: &mut DebugState) -> bool {
    let spent = ds.events.take_spent();
    if spent.is_empty() {
        return false;
    }
    if crate::runtime::env_cache::frame_trace() {
        for id in &spent {
            eprintln!("[JDWP_COUNT_SPENT] request={id} gates republished");
        }
    }
    true
}

/// [`take_spent_requests`], and republish the gates when a request was
/// spent (wave 46).
pub(crate) fn republish_gates_if_spent(shared: &crate::vm::SharedVm, ds: &mut DebugState) {
    if take_spent_requests(ds) {
        publish_debugger_gates(shared, ds);
    }
}

/// Raise (`armed`) or lower [`METHOD_EVENTS_BLOCKING`] for `shared`
/// (interpreter round i1 wave 46, lane L1): raised when a debugger attaches,
/// lowered when it detaches (kept while a C JVMTI env is bound, which never
/// unbinds; the C table does not raise it itself). The stand-ins' callbacks
/// are looked up in the VM's registry when it is raised.
pub(crate) fn arm_blocking_standins(shared: &crate::vm::SharedVm, armed: bool) {
    use std::sync::atomic::Ordering;
    let gates = &shared.debug.debugger_gates;
    let armed = armed || gates.native_env_present();
    if armed {
        for (slot, (class, name, descriptor)) in
            gates.blocking_standins.iter().zip(BLOCKING_STANDINS)
        {
            let callback = shared
                .natives
                .native_methods
                .find(class, name, descriptor)
                .map_or(0, |callback| callback as usize); // Cast: fn address
            slot.store(callback, Ordering::Release);
        }
    }
    gates.set_method_events_bit(METHOD_EVENTS_BLOCKING, armed);
}

/// Publish what the requests and suspensions in `ds` ask of the interpreter
/// and the JIT: the per-method summary ([`DebuggerGates`]) and then the one
/// VM-wide gate, `breakpoints_active` (armed while any breakpoint, step
/// request or suspension is in force). Called with the debug-state lock held
/// by whoever changed them: the JDWP server after every command and at
/// detach, the interpreter's suspend point after an event suspended threads.
pub(crate) fn publish_debugger_gates(shared: &crate::vm::SharedVm, ds: &DebugState) {
    use events::EventKind;
    let gates = &shared.debug.debugger_gates;
    let suspension = ds.any_suspension_of_a_live_thread();
    // Wave 106 (lane L1): before `suspension`, so a reader of that bit never
    // finds this one down.
    gates.suspension_counted.store(
        suspension || ds.any_suspension(),
        std::sync::atomic::Ordering::Release,
    );
    gates
        .suspension
        .store(suspension, std::sync::atomic::Ordering::Release);
    // Interpreter round i3 wave 72, lane L6 (`CRATONVM_PARK_LAZY_SUSPEND_AWARE`
    // with `CRATONVM_PARK_LAZY_STW_WAKE`): a park's running phase re-reads
    // the gate now and, suspended, takes its blocked region. One load when off.
    if suspension && shared.threads.thread_registry.park_lazy_suspend_aware() {
        shared.threads.thread_registry.wake_lazy_park_phases();
    }
    // Wave 54 (lane L6): the waits on the debug state re-check it now (a
    // resume, a suspension, a detach); they take the lock our caller holds
    // once it lets it go.
    gates.notify_state_changed();
    // Wave 24: a method entry or exit request, like a step, concerns every
    // method — only the interpreter's suspend point reports them — and so
    // does a `ThreadReference.Stop` not yet delivered, which the thread
    // throws at its next interpreted bytecode.
    let (method_entries, method_exits) = ds.events.method_events_requested();
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`):
    // a method event request confined to classes the scope resolved
    // (`refresh_method_event_scope`) concerns only those classes' methods
    // (`DebuggerGates::in_method_event_scope`), not every method; the others
    // still do. Off: every method event request concerns every method.
    let (wide_entries, wide_exits, scoped_methods) = method_events_by_scope(gates, ds);
    let all_methods = ds.events.has_single_steps()
        || suspension
        || wide_entries
        || wide_exits
        || !ds.pending_stops.is_empty();
    gates.stops.store(
        !ds.pending_stops.is_empty(),
        std::sync::atomic::Ordering::Release,
    );
    // Wave 26: the native-call funnel's pre-filter (wave 27: its JDWP bit;
    // the C JVMTI table owns the other).
    gates.set_method_events_bit(METHOD_EVENTS_JDWP, method_entries || method_exits);
    gates.method_entry.set(method_entries);
    let mut methods = ds.events.breakpoint_locations();
    // Wave 10: the VM's JVMTI breakpoints ride the same per-method gate
    // (`jvmti::set_breakpoint_for_vm`); the suspend point delivers them.
    methods.extend(crate::runtime::jvmti::breakpoint_methods_for_vm(
        shared.vm_identity,
    ));
    // Wave 28: a breakpoint or a step routes the native-call funnel through
    // its debugger hook, which runs a stood-in Java method's bytecode when
    // the breakpoint sits in it or the step may enter it.
    gates.set_method_events_bit(
        METHOD_EVENTS_FRAMES,
        !methods.is_empty() || ds.events.has_single_steps(),
    );
    // Wave 10: field watches and exception requests. Only the interpreter
    // posts their events, so while one is in force every method runs
    // interpreted (`DebuggerGates::requires_interpreter`), but the dispatch
    // loop's per-bytecode suspend point stays off: their hooks sit in the
    // field bytecodes and the unwinder.
    // Interpreter round i2 wave 63 (lane L1): or a C JVMTI env's field
    // watch, whose events the same field bytecodes post
    // (`jvmti::native_env::post_field_watch`).
    // Interpreter round i5 wave 103 (lane L1): a JDWP monitor event request
    // arms the monitor paths' event hook (`NativeEvent::Monitor`).
    let jdwp_monitor = ds.events.has_requests(EventKind::MonitorContendedEnter)
        || ds.events.has_requests(EventKind::MonitorContendedEntered)
        || ds.events.has_requests(EventKind::MonitorWait)
        || ds.events.has_requests(EventKind::MonitorWaited);
    gates.jdwp_native_events.store(
        if jdwp_monitor {
            NativeEvent::Monitor as u32
        } else {
            0
        },
        std::sync::atomic::Ordering::Release,
    );
    let jdwp_field_watch = ds.events.has_requests(EventKind::FieldAccess)
        || ds.events.has_requests(EventKind::FieldModification);
    let native_field_watch = gates.native_event_armed(NativeEvent::FieldWatch);
    let field_watch = jdwp_field_watch || native_field_watch;
    // Interpreter round i4 wave 95 (lane L1, `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`):
    // JDWP watches whose fields the scope resolved, with no class of it
    // degrading it, keep only the classes that name a watched field
    // interpreted (`DebuggerGates::concerns_method`); a C JVMTI env's watch,
    // or any other JDWP watch, still keeps every method interpreted. The
    // field bytecodes' hook stays armed (`field_watch`) either way.
    let scoped_field_watch = jdwp_field_watch
        && gates.field_watch_scope.enabled()
        && gates
            .field_watch_scope
            .serves(&ds.events.watched_field_ids());
    let field_watch_needs_interpreter =
        native_field_watch || (jdwp_field_watch && !scoped_field_watch);
    let field_watch_scope_grew = gates.field_watch_scope.take_grew();
    let exceptions = ds.events.has_requests(EventKind::Exception);
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`):
    // a VM that serves `Exception` from compiled code keeps every method
    // interpreted only for a request compiled code cannot serve yet (one that
    // suspends, or filters by instance); the unwinder's hook stays armed
    // (`exceptions`) either way.
    let exceptions_need_interpreter = exceptions
        && (!gates.jdwp_exceptions_from_compiled()
            || ds.events.exception_requests_need_the_interpreter());
    gates
        .compiled_exception_requests
        .set(exceptions && !exceptions_need_interpreter);
    // Interpreter round i2 wave 63 (lane L1): a C JVMTI env's
    // `SampledObjectAlloc` samples in the interpreter's allocation funnels,
    // which compiled code's inline allocations bypass: every method runs
    // interpreted while it is enabled, as for a field watch.
    let sampling = gates.native_event_armed(NativeEvent::SampledAlloc);
    // Wave 40 (lane L1): a breakpoint in a method of a JDK class. The JIT's
    // call-site intrinsics (`try_resolve_*_intrinsic` in `jit/src/lib.rs`:
    // `String.length`, `Math.max`, the boxing methods, ...) expand such a
    // method in a caller from a fixed table, asking no VM resolver, so the
    // compile doors' per-method refusal (`breakpoint_bars_compiling`) cannot
    // keep a caller compiled AFTER the breakpoint from running the method
    // inline and missing it. While one is in force every method runs
    // interpreted (`interpret_all`) and nothing is compiled (the withdrawal
    // bit below), as for a field watch.
    let jdk_breakpoint = crate::runtime::interpreter::JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED
        && methods.iter().any(|&(class_id, _)| {
            breakpoint_class_is_expanded_without_a_record(shared, ds, class_id)
        });
    let active = all_methods
        || scoped_methods
        || !methods.is_empty()
        || field_watch
        || exceptions
        || sampling;
    let was_all_methods = gates.all_methods.load(std::sync::atomic::Ordering::Acquire);
    // Wave 38 (lane L1): the classes one of whose methods gains a breakpoint
    // at this publication, for the scoped withdrawal below. Nothing is
    // allocated unless the set grew.
    let (methods_changed, mut gained_classes) = {
        let before = gates.breakpoint_methods.read();
        let gained: Vec<u64> = methods
            .iter()
            .filter(|method| !before.contains(*method))
            .map(|&(class_id, _)| class_id)
            .collect();
        (*before != methods, gained)
    };
    gained_classes.sort_unstable();
    gained_classes.dedup();
    gates.store(all_methods, methods);
    let (was_watching, interpret_all_rose) = gates.store_subject_flags(
        field_watch,
        field_watch_needs_interpreter,
        exceptions,
        exceptions_need_interpreter,
        jdk_breakpoint || sampling,
    );
    gates.class_prepare.store(
        ds.events.has_requests(EventKind::ClassPrepare),
        std::sync::atomic::Ordering::Release,
    );
    if field_watch != was_watching {
        // The process-wide pre-filter the field bytecodes (and every
        // quickened field arm) test before anything else.
        crate::runtime::jvmti::note_debugger_field_watch(field_watch);
    }
    let was_active = shared
        .debug
        .breakpoints_active
        .swap(active, std::sync::atomic::Ordering::AcqRel);
    // Interpreter round i2 wave 62 (lane L7): a dispatch loop that hoists the
    // observer gates (`VmConfig::interp_hoisted_observers`) re-reads this one,
    // and the per-method summary stored above, at its next top: send every
    // loop of this VM there, after the stores.
    // Wave 81 (lane L1): `CRATONVM_JVMTI_STEP_POLL` re-reads the same gates
    // there (a JDWP step request armed from a callback on this thread).
    if shared.config.interp_hoisted_observers || true {
        shared.mem.gc_barrier.note_code_moved_on_every_loop();
    }
    // The compiled-code helpers stand down on this VM-wide flag, but a
    // compiled frame already running reaches them only on an inline-cache
    // miss: flush the VM's MIC/PIC slots on the false→true edge, after the
    // store (interpreter round i1 wave 9, lane L5; the JVMTI twin is
    // `jvmti::publish_union_listener_flags`). Nothing is retired.
    //
    // Wave 10: also when the set of methods that must run interpreted grows
    // to every method (a step, a suspension, a field watch or an exception
    // request) while the flag was already up for a breakpoint: a compiled
    // caller would otherwise keep calling its compiled callees through the
    // caches it filled before.
    let every_method_rose = all_methods && !was_all_methods;
    if active && (!was_active || every_method_rose || interpret_all_rose) {
        shared.jit.jit_cache.clear_inline_caches();
    }
    // Wave 37 (lane L1): a step or a method event request needs every method
    // interpreted, including a compiled caller's baked call into a compiled
    // callee and a frame already running compiled, which neither the doors
    // nor the loop-exit pause below reach. On the edge where the first such
    // request comes into force every compiled body is withdrawn and made not
    // entrant, and the running ones leave at their next exit-capable point;
    // compiling resumes when the last one goes. Not for a suspension, a
    // breakpoint or an exception request: a suspension is taken and released
    // on every stop, and a flush per stop would recompile everything each
    // time; a breakpoint withdraws its own class's dependents (below); the
    // compiled catch doors post an exception event themselves. Before the
    // loop-exit pause, so a frame polling in it reads the withdrawal.
    //
    // Wave 38 (lane L1): and a field watch. Compiled code posts no field
    // event, so a frame running compiled, or a compiled callee a baked call
    // reaches, read and wrote the watched field unseen until it returned to
    // the interpreter (`L1W38JdiFieldWatchInCompiledLoop`; HotSpot reports
    // the compiled loop's access). The doors already refuse every compiled
    // entry while a watch is in force (`interpret_all`), so withdrawing costs
    // only the bodies already running.
    crate::runtime::interpreter::note_every_method_needs_the_interpreter(
        shared,
        WITHDRAWAL_BY_JDWP,
        // Wave 95 (lane L1): a class-scoped field watch withdraws only its
        // classes' dependents (`field_watch_scope::refresh`).
        ds.events.has_single_steps()
            || wide_entries
            || wide_exits
            || field_watch_needs_interpreter
            || sampling,
    );
    // Wave 40 (lane L1): a breakpoint in a JDK class's method withdraws every
    // body (the whole-cache path the scoped withdrawal below already took for
    // a JDK class) and, unlike that one-shot withdrawal, keeps the tier-up
    // strides closed while it stands, so no caller is compiled that expands
    // the method as a call-site intrinsic. Its own bit, so its positive
    // control names it (`source=jdk-breakpoint`). Before the scoped
    // withdrawal, which then finds every body withdrawn and does nothing.
    crate::runtime::interpreter::note_every_method_needs_the_interpreter(
        shared,
        WITHDRAWAL_BY_JDK_BREAKPOINT,
        jdk_breakpoint,
    );
    // Wave 38 (lane L1): a breakpoint concerns one method, so it does not take
    // the whole-cache withdrawal above, but a compiled caller's baked call
    // into that method's compiled body, a caller that inlined it, and a frame
    // running one of those on a thread no pause reaches never let it be hit.
    // On the edge where a method gains a breakpoint, its class's dependents
    // are withdrawn (`interpreter::note_breakpoint_classes_gained`). The class
    // name comes from the session's own class table, or from the name a JVMTI
    // breakpoint recorded: the class manager is never taken under the
    // debug-state lock (the class-unload poll takes the two in the other
    // order).
    if !gained_classes.is_empty() {
        let classes: Vec<(crate::classloading::ClassId, Option<String>)> = gained_classes
            .iter()
            .filter_map(|&id| {
                let raw = u32::try_from(id).ok()?;
                let name = ds
                    .class_signatures
                    .get(&id)
                    .and_then(|sig| sig.strip_prefix('L')?.strip_suffix(';'))
                    .map(str::to_string)
                    .or_else(|| {
                        crate::runtime::jvmti::breakpoint_class_name_for_vm(shared.vm_identity, id)
                            .map(|name| name.to_string())
                    });
                Some((crate::classloading::ClassId::new(raw), name))
            })
            .collect();
        crate::runtime::interpreter::note_breakpoint_classes_gained(shared, &classes);
    }
    // Wave 12 (lane L4): a loop already running in an OSR body of a method the
    // requests now concern leaves it at its next back-edge poll. Asked when
    // what the gates concern grew — an edge of the flags above, or a changed
    // per-method set (a breakpoint in one more method is not a flag edge);
    // nothing is paused unless an OSR body of this VM is running, and the
    // pause runs on its own thread (this caller holds the debug-state lock).
    // Lane L1, wave 12: it was asked on every publication while active, and
    // the server publishes after every command, so with an unconcerned OSR
    // loop running each command of a session cost a thread spawn and a
    // stop-the-world pause.
    // Wave 93 (lane L1): or the classes a scoped method event request
    // concerns grew (`refresh_method_event_scope` withdrew their dependents).
    let scope_generation = gates
        .method_event_scope_generation
        .load(std::sync::atomic::Ordering::Acquire);
    let scope_grew = gates
        .method_event_scope_published
        .swap(scope_generation, std::sync::atomic::Ordering::AcqRel)
        != scope_generation;
    let concerned_grew = !was_active
        || every_method_rose
        || interpret_all_rose
        || methods_changed
        || (scoped_methods && scope_grew)
        // Wave 95 (lane L1): or the classes a scoped field watch concerns.
        || (scoped_field_watch && field_watch_scope_grew);
    if active && concerned_grew {
        crate::runtime::interpreter::request_compiled_loop_exits(shared);
    }
}

/// `(entries, exits, scoped)` of the method event requests in force for
/// [`publish_debugger_gates`] (interpreter round i4 wave 93, lane L1): with
/// [`DebuggerGates::scoped_method_events`], a request whose class scope the
/// last [`refresh_method_event_scope`] resolved, while no class of it
/// degraded the scope, is `scoped`; every other one concerns every method.
/// Without the switch: [`events::EventManager::method_events_requested`] and
/// nothing scoped. Takes the resolved scopes' read lock for a compare (a
/// leaf: its writer holds no other lock while it writes).
pub(in crate::debug) fn method_events_by_scope(gates: &DebuggerGates, ds: &DebugState) -> (bool, bool, bool) {
    if !gates.scoped_method_events {
        let (entries, exits) = ds.events.method_events_requested();
        return (entries, exits, false);
    }
    // Interpreter round i4 wave 94 (lane L1): what this publication saw, for
    // the server's poll (`DebuggerGates::method_event_degraded_published`).
    let degraded = gates
        .method_event_degraded
        .load(std::sync::atomic::Ordering::Acquire);
    gates
        .method_event_degraded_published
        .store(degraded, std::sync::atomic::Ordering::Release);
    if degraded {
        let (entries, exits) = ds.events.method_events_requested();
        return (entries, exits, false);
    }
    let resolved = gates.method_event_specs.read();
    ds.events
        .method_events_split(&|scope| resolved.contains(scope))
}

/// Does `class` fall in one of `scopes` (interpreter round i4 wave 93, lane
/// L1)? `Below`: the class or a subclass of it; `Named`: its dotted name.
pub(in crate::debug) fn method_event_scope_matches(
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    scopes: &[events::MethodEventScope],
) -> bool {
    scopes.iter().any(|scope| match scope {
        events::MethodEventScope::Below(id) => u32::try_from(*id)
            .is_ok_and(|raw| cm.is_subclass_of(class.id, crate::classloading::ClassId::new(raw))),
        events::MethodEventScope::Named(pattern) => {
            events::class_pattern_matches(pattern, &class.name)
        }
    })
}

/// Must a class in a method event scope take every method of the VM with it
/// (interpreter round i4 wave 93, lane L1; `DebuggerGates::method_event_degraded`)?
/// A JDK class, whose methods the JIT may expand at a call site without a
/// record the scoped withdrawal reads
/// (`interpreter::jit_may_expand_a_method_of_without_a_record`), or a class
/// with a native method, whose events the native-call funnel reports only
/// for an interpreted caller.
pub(in crate::debug) fn method_event_class_degrades(class: &crate::classloading::Class) -> bool {
    crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(&class.name)
        || class.methods.iter().any(|m| m.is_native())
}

/// Resolve the class scopes of the JDWP method event requests in force to the
/// classes they concern (interpreter round i4 wave 93, lane L1;
/// `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`, page
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// Called by the JDWP server, holding no lock, at each poll and before it
/// publishes the gates after a command; nothing with the switch off.
///
/// When the scopes changed, every class of the store is matched and the
/// result replaces the set; otherwise only the classes defined since the
/// last call are. The new scopes are published BEFORE the class store's slot
/// count is read, so a class defined after that read is prepared after it
/// and joins at its preparation ([`join_method_event_scope`]); one a join
/// added while this call resolved is carried over. On the edge where a class
/// joins, the compiled code that would run its methods without the
/// interpreter -- its own bodies and every body that inlined one of its
/// methods -- is withdrawn (`interpreter::note_breakpoint_classes_gained`,
/// the breakpoint's scoped withdrawal); the other classes stay compiled.
///
/// Lock order: the class manager is read with no debug-state lock held; the
/// set's and the scopes' locks are leaves.
///
/// Answers whether the scope changed (its scopes, its classes or its
/// degradation), so a caller no publication follows publishes the gates
/// ([`refresh_method_event_scope_and_publish`]).
pub(crate) fn refresh_method_event_scope(shared: &crate::vm::SharedVm) -> bool {
    use std::sync::atomic::Ordering;
    let gates = &shared.debug.debugger_gates;
    if !gates.scoped_method_events {
        return false;
    }
    let wanted = shared.debug.debug_state.lock().events.method_event_scopes();
    let changed = *gates.method_event_specs.read() != wanted;
    if wanted.is_empty() {
        if changed || gates.method_event_scope_armed() {
            gates.clear_method_event_scope();
            return true;
        }
        return false;
    }
    if changed {
        // Interpreter round i4 wave 94 (lane L1): the degradation restarts
        // with the new scopes, under the scopes' write lock, so a join that
        // matched the new scopes and degraded them (it swaps the flag while
        // holding the read lock, `join_method_event_scope`) is never undone
        // by this refresh's own verdict, which is OR-ed in below. Wave 93
        // stored the verdict over it.
        let mut specs = gates.method_event_specs.write();
        *specs = wanted.clone();
        gates.method_event_degraded.store(false, Ordering::Release);
    }
    gates.method_event_scope_armed.store(true, Ordering::SeqCst);
    let from = if changed {
        0
    } else {
        gates.method_event_slots_seen.load(Ordering::Acquire)
    };
    let (matched, degraded, slots) = {
        let cm = shared.classes.class_manager.read();
        let slots = cm.class_store.slot_count();
        if !changed && slots <= from {
            // Wave 94: a join that degraded the scope where it could not
            // publish left the publication behind.
            return gates.method_event_degraded.load(Ordering::Acquire)
                != gates.method_event_degraded_published.load(Ordering::Acquire);
        }
        // A `ClassOnly` naming an interface (its implementors), a JDK class or
        // a class the store does not hold (a lambda proxy's id) is not
        // scoped in stage 1.
        let mut degraded = changed
            && wanted.iter().any(|scope| match scope {
                events::MethodEventScope::Below(id) => !u32::try_from(*id).is_ok_and(|raw| {
                    cm.class_store
                        .get(crate::classloading::ClassId::new(raw))
                        .is_some_and(|c| {
                            !c.is_interface()
                                && !crate::runtime::interpreter::jit_may_expand_a_method_of_without_a_record(&c.name)
                        })
                }),
                events::MethodEventScope::Named(_) => false,
            });
        let mut matched: Vec<(u32, String)> = Vec::new();
        for slot in from..slots {
            let Ok(raw) = u32::try_from(slot) else {
                break;
            };
            let Some(class) = cm.class_store.get(crate::classloading::ClassId::new(raw)) else {
                continue;
            };
            if method_event_scope_matches(&cm, class, &wanted) {
                degraded |= method_event_class_degrades(class);
                matched.push((raw, class.name.to_string()));
            }
        }
        (matched, degraded, slots)
    };
    let ids: Vec<u32> = matched.iter().map(|&(id, _)| id).collect();
    let gained = gates.store_method_event_classes(&ids, changed, slots);
    gates.method_event_slots_seen.store(slots, Ordering::Release);
    if degraded {
        gates.method_event_degraded.store(true, Ordering::Release);
    }
    let degraded = gates.method_event_degraded.load(Ordering::Acquire);
    if crate::runtime::env_cache::dbg_jitc() && (changed || !gained.is_empty()) {
        eprintln!(
            "[cratonvm-jitc] method event scope: scopes={} classes={} gained={} degraded={degraded}",
            wanted.len(),
            gates.method_event_classes.read().len(),
            gained.len(),
        );
    }
    // Wave 94: measured against what the last publication saw, not against
    // the flag at entry, which a join may already have moved.
    let moved = changed
        || !gained.is_empty()
        || degraded != gates.method_event_degraded_published.load(Ordering::Acquire);
    if degraded || gained.is_empty() {
        // Degraded: the publication that follows withdraws every body.
        return moved;
    }
    let classes: Vec<(crate::classloading::ClassId, Option<String>)> = matched
        .into_iter()
        .filter(|(id, _)| gained.contains(id))
        .map(|(id, name)| (crate::classloading::ClassId::new(id), Some(name)))
        .collect();
    crate::runtime::interpreter::note_breakpoint_classes_gained(shared, &classes);
    moved
}

/// [`refresh_method_event_scope`], and publish the gates when the scope
/// changed (interpreter round i4 wave 93, lane L1): for the JDWP server's
/// poll and its detach, after which no other publication follows (a spent
/// `Count`, or the classes defined since the last poll, may change it).
///
/// Wave 95 (lane L1): the field watches' class scope too
/// ([`field_watch_scope::refresh`], `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`).
pub(crate) fn refresh_method_event_scope_and_publish(shared: &crate::vm::SharedVm) {
    let methods_moved = refresh_method_event_scope(shared);
    let watches_moved = field_watch_scope::refresh(shared);
    if methods_moved || watches_moved {
        let ds = shared.debug.debug_state.lock();
        publish_debugger_gates(shared, &ds);
    }
}

/// Class `class_id` of this VM was just prepared while a class-scoped method
/// event scope is resolved (interpreter round i4 wave 93, lane L1;
/// [`DebuggerGates::method_event_scope_armed`]): if a scope names it, it
/// joins the scope before any of its code runs, so nothing of it is compiled
/// while the request stands. A class that degrades the scope
/// ([`method_event_class_degrades`]) republishes the gates, which then keep
/// every method interpreted. Called from [`class_prepared_on_thread`] with no
/// lock held.
pub(in crate::debug) fn join_method_event_scope(shared: &crate::vm::SharedVm, class_id: crate::classloading::ClassId) {
    let gates = &shared.debug.debugger_gates;
    let scopes = gates.method_event_specs.read().clone();
    if scopes.is_empty() {
        return;
    }
    let degrades = {
        let cm = shared.classes.class_manager.read();
        let Some(class) = cm.class_store.get(class_id) else {
            return;
        };
        if !method_event_scope_matches(&cm, class, &scopes) {
            return;
        }
        method_event_class_degrades(class)
    };
    if gates.in_method_event_scope(class_id.as_u32()) {
        return;
    }
    // Interpreter round i4 wave 94 (lane L1): store only while the scopes
    // matched above are still the resolved ones, holding their read lock
    // across the store (the set's lock is taken inside it; nothing takes
    // the two the other way round). Wave 93 stored unconditionally: a join
    // racing a `clear_method_event_scope` (the last scoped request cleared,
    // or a detach) re-added its class after the clear, and that class then
    // stayed out of every compile (`breakpoint_bars_compiling`) with no
    // request in force until another scoped request came. A mismatch means a
    // refresh published other scopes after this class was defined, and that
    // refresh's walk of the store covers it.
    let fresh_degradation = {
        let resolved = gates.method_event_specs.read();
        if *resolved != scopes {
            return;
        }
        // `carry_from` past every slot: nothing is replaced.
        let _ = gates.store_method_event_classes(&[class_id.as_u32()], false, usize::MAX);
        degrades
            && !gates
                .method_event_degraded
                .swap(true, std::sync::atomic::Ordering::AcqRel)
    };
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] method event scope: class id={} joined at its preparation degrades={degrades}",
            class_id.as_u32(),
        );
    }
    // Not on the server or a thread running its heap work, which may hold
    // the debug-state lock: the server's next poll publishes
    // (`DebuggerGates::method_event_degraded_published`, wave 94).
    if fresh_degradation && !debugger_hooks_suppressed() {
        let ds = shared.debug.debug_state.lock();
        publish_debugger_gates(shared, &ds);
    }
}

/// Is the class `class_id`, which holds a breakpoint, one whose methods the
/// JIT may expand at a call site without a record the compile doors read
/// (interpreter round i1 wave 40, lane L1;
/// `interpreter::jit_may_expand_a_method_of_without_a_record`: a JDK class)?
/// The name comes from the session's class table or from the name a JVMTI
/// breakpoint recorded beside it, as for the scoped withdrawal: the class
/// manager is not taken under the debug-state lock. A class neither names is
/// answered `false`: every class a debugger can set a breakpoint in reached
/// it through a reply that recorded its signature
/// ([`populate_class_metadata`]), and `jvmti::set_breakpoint_for_vm` records
/// the name before it publishes.
pub(in crate::debug) fn breakpoint_class_is_expanded_without_a_record(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    class_id: u64,
) -> bool {
    // Wave 117 (`CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET`): narrowed to the
    // JDK classes the JIT can expand ([`jit_expansion_set`]) with the switch.
    let gates = &shared.debug.debugger_gates;
    let expanded = |name: &str| gates.breakpoint_may_be_expanded(name);
    if let Some(name) = ds
        .class_signatures
        .get(&class_id)
        .and_then(|sig| sig.strip_prefix('L')?.strip_suffix(';'))
    {
        return expanded(name);
    }
    crate::runtime::jvmti::breakpoint_class_name_for_vm(shared.vm_identity, class_id)
        .is_some_and(|name| expanded(&name))
}

/// The ids in `filter_classes` that name `class` or one of its supertypes
/// (superclasses and superinterfaces): the answers a `ClassOnly` modifier
/// needs about `class` (interpreter round i1 wave 24), as HotSpot's back end
/// evaluates it (`IsAssignableFrom`).
pub(crate) fn supertypes_among(
    cm: &crate::classloading::ClassManager,
    class: crate::classloading::ClassId,
    filter_classes: &[u64],
) -> Vec<u64> {
    filter_classes
        .iter()
        .copied()
        .filter(|&id| {
            u32::try_from(id)
                .is_ok_and(|raw| cm.is_subclass_of(class, crate::classloading::ClassId::new(raw)))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Method entries (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// How many VMs of this process have a JDWP `MethodEntry` request in force:
/// the one-load pre-filter every interpreter frame push reads
/// (`interpreter::fire_method_entry_after_push`) before it records the new
/// frame for the suspend point ([`note_frame_entered`]). A union over VMs,
/// like JVMTI's listener mirrors: a push in a VM without a request records a
/// depth nobody consumes, which the next push at that depth discards.
///
/// Interpreter round i4 wave 93 (lane L1): two counts in one word, so the
/// per-VM-state ratchet's static count does not grow. The low 32 bits
/// ([`METHOD_ENTRY_COUNT`]) count the VMs above; the high 32 bits
/// ([`COMPILED_EXCEPTION_COUNT`]) count the VMs with a JDWP `Exception`
/// request they serve from compiled code
/// (`DebuggerGates::compiled_exception_requests`), the pre-filter of the
/// compiled `athrow` helper ([`compiled_exception_requests_anywhere`]).
pub(in crate::debug) static METHOD_ENTRY_REQUEST_VMS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// [`METHOD_ENTRY_REQUEST_VMS`]'s method-entry count: one unit, and its mask.
pub(in crate::debug) const METHOD_ENTRY_COUNT: (u64, u64) = (1, 0xFFFF_FFFF_u64);
/// [`METHOD_ENTRY_REQUEST_VMS`]'s compiled-exception count (wave 93).
pub(in crate::debug) const COMPILED_EXCEPTION_COUNT: (u64, u64) = (1_u64 << 32, 0xFFFF_FFFF_u64 << 32);

/// Is a JDWP `MethodEntry` request in force in any VM of this process?
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
#[inline(always)]
pub(crate) fn method_entry_requests_anywhere() -> bool {
    METHOD_ENTRY_REQUEST_VMS.load(std::sync::atomic::Ordering::Relaxed) & METHOD_ENTRY_COUNT.1 != 0
}

/// Does any VM of this process serve a JDWP `Exception` request from
/// compiled code (interpreter round i4 wave 93, lane L1)? The pre-filter the
/// compiled `athrow` helper reads before it records its throw site
/// (`interpreter::compiled_throw_sites_wanted`); one relaxed load.
#[inline(always)]
pub(crate) fn compiled_exception_requests_anywhere() -> bool {
    METHOD_ENTRY_REQUEST_VMS.load(std::sync::atomic::Ordering::Relaxed)
        & COMPILED_EXCEPTION_COUNT.1
        != 0
}

/// One of [`METHOD_ENTRY_REQUEST_VMS`]'s counts (`(unit, mask)`) moved by
/// one VM: up when `armed`, down (never below zero) otherwise.
pub(in crate::debug) fn note_request_vms(count: (u64, u64), armed: bool) {
    use std::sync::atomic::Ordering;
    let (unit, mask) = count;
    let _ = METHOD_ENTRY_REQUEST_VMS.try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        let own = n & mask;
        let rest = n & !mask;
        let own = if armed {
            own.wrapping_add(unit) & mask
        } else if own == 0 {
            0
        } else {
            own - unit
        };
        Some(rest | own)
    });
}

/// One VM's share of one of [`METHOD_ENTRY_REQUEST_VMS`]'s counts
/// (interpreter round i4 wave 95, lane L1): whether this VM holds a unit of
/// it. [`Self::set`] moves the count only on this VM's edges, so it stays
/// balanced per VM; dropping the VM's gates with the unit held (an embedder
/// dropping a VM while a debugger's request stands) gives it back, so the
/// process-wide pre-filter does not stay up for a VM that is gone.
///
/// The `MethodEntry` count (`DebuggerGates::method_entry`, wave 24) and the
/// compiled-exception count (`DebuggerGates::compiled_exception_requests`,
/// wave 93) each hold one. A field type with `Drop`, not a `Drop` for
/// `DebuggerGates`, so `DebuggerGates { .., ..DebuggerGates::new() }` stays
/// legal (a temporary's unused edge is unarmed and gives nothing back).
pub(crate) struct RequestVmsEdge {
    pub(in crate::debug) armed: std::sync::atomic::AtomicBool,
    pub(in crate::debug) count: (u64, u64),
}

impl RequestVmsEdge {
    /// An unarmed share of `count` (`(unit, mask)`).
    pub(in crate::debug) const fn new(count: (u64, u64)) -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            count,
        }
    }

    /// This VM's requests of the kind came into force (`armed`) or all went;
    /// only [`publish_debugger_gates`] calls this (under the debug-state
    /// lock). Moves the count on an edge only.
    pub(in crate::debug) fn set(&self, armed: bool) {
        let was = self
            .armed
            .swap(armed, std::sync::atomic::Ordering::AcqRel);
        if was != armed {
            note_request_vms(self.count, armed);
        }
    }
}

impl Drop for RequestVmsEdge {
    fn drop(&mut self) {
        if *self.armed.get_mut() {
            note_request_vms(self.count, false);
        }
    }
}

thread_local! {
    /// The depths of the interpreter frames this thread pushed while a
    /// `MethodEntry` request was in force whose first bytecode the suspend
    /// point has not reached yet, innermost last (wave 24). The suspend
    /// point reports a method entry at bytecode 0 of a frame only when its
    /// depth is here ([`take_frame_entry`]); a backward branch to bytecode 0
    /// (`while (true)` compiles to `0: goto 0`) is not an entry.
    pub(in crate::debug) static ENTERED_FRAMES: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The calling thread just pushed an interpreter frame at `depth` (its frame
/// count after the push) while a `MethodEntry` request is in force somewhere
/// ([`method_entry_requests_anywhere`]). A depth at or above `depth` still
/// recorded belongs to a frame that is gone.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn note_frame_entered(depth: usize) {
    ENTERED_FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            while frames.last().is_some_and(|&d| d >= depth) {
                frames.pop();
            }
            frames.push(depth);
        }
    });
}

/// The calling thread pushed a frame at `depth` that was NOT entered there (a
/// frame rebuilt from a compiled activation, `interpreter::push_resumed_frame`;
/// interpreter round i4 wave 93, lane L1): every record at or above `depth`
/// belongs to a frame that is gone. While a class-scoped method event request
/// keeps only some methods interpreted, an unconcerned frame never reaches
/// the suspend point that would consume its record, and the record outlived
/// it until the next push at its depth: a resumed frame of a concerned method
/// branching back to bytecode 0 then reported a `MethodEntry`.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn note_frame_resumed(depth: usize) {
    ENTERED_FRAMES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut() {
            while frames.last().is_some_and(|&d| d >= depth) {
                frames.pop();
            }
        }
    });
}

/// Is bytecode 0 of the frame at `depth` on the calling thread the method's
/// entry — pushed, and not yet reached by the suspend point? Consumes the
/// record (and any deeper, stale one).
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn take_frame_entry(depth: usize) -> bool {
    ENTERED_FRAMES.with(|frames| {
        let Ok(mut frames) = frames.try_borrow_mut() else {
            return false;
        };
        while frames.last().is_some_and(|&d| d > depth) {
            frames.pop();
        }
        if frames.last() == Some(&depth) {
            frames.pop();
            true
        } else {
            false
        }
    })
}

// ---------------------------------------------------------------------------
// `ThreadReference.Stop` (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// Record `ThreadReference.Stop(thread, throwable)`: `tid` throws the object
/// id `throwable_id` names at its next interpreted bytecode
/// ([`take_pending_stop`]). The id's collection is disabled until then, so
/// the throwable survives a debugger that lets go of it; a stop already
/// pending for the thread is replaced (HotSpot installs the latest one).
/// `false` for an id naming no live object (`INVALID_OBJECT`). Called where
/// no collection can run (a mutator between polls, or the server quiesced),
/// with the debug-state lock held; the caller republishes the gates, which
/// arm every method while a stop is pending.
pub(crate) fn set_pending_stop(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    tid: u64,
    throwable_id: u64,
) -> bool {
    if throwable_id == 0 || !set_collection_enabled(shared, ds, throwable_id, false) {
        return false;
    }
    // Pinned as well: a debugger that disposes of the id (JDI does once its
    // mirror is unreachable) must not free the handle before delivery.
    ds.objects.note_export(throwable_id, ObjectExport::Pinned);
    if let Some(previous) = ds.pending_stops.insert(tid, throwable_id) {
        release_pending_stop(shared, ds, previous);
    }
    true
}

/// Undo what [`set_pending_stop`] did to the id of a stop that is delivered
/// or replaced: collection enabled again, the pin dropped.
pub(in crate::debug) fn release_pending_stop(shared: &crate::vm::SharedVm, ds: &mut DebugState, throwable_id: u64) {
    let _ = set_collection_enabled(shared, ds, throwable_id, true);
    ds.objects.unpin(throwable_id);
}

/// The throwable a `ThreadReference.Stop` left for thread `tid`, if any: the
/// interpreter's suspend point throws it at the bytecode it is about to run,
/// as HotSpot throws an asynchronous exception at the thread's current
/// bytecode. Its id's collection is enabled again (the object stays where it
/// is until the caller's next safepoint, and the caller throws it before
/// one). `None` — one lock and one map probe — in the common case.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(crate) fn take_pending_stop(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> Option<crate::types::ObjectRef> {
    let mut ds = shared.debug.debug_state.lock();
    let id = ds.pending_stops.remove(&tid)?;
    let throwable = object_for_id(shared, &ds, id);
    release_pending_stop(shared, &mut ds, id);
    publish_debugger_gates(shared, &ds);
    release_disposed_objects(shared, &mut ds);
    throwable
}

/// The exception the interpreter's unwinder is about to route for thread
/// `tid`, or in its place the throwable a `ThreadReference.Stop` left for the
/// thread (interpreter round i1 wave 25). HotSpot installs a stop as an
/// asynchronous exception that REPLACES a pending one
/// (`JavaThread::handle_async_exception` → `set_pending_exception`); the
/// case that matters is the `InterruptedException` of a blocking call the
/// stop woke ([`inspect`]'s `stop_thread`), which must not reach the
/// program's handlers: the stop must, and it must be matched against the
/// handlers the in-flight exception would have been, not thrown later at
/// the first bytecode of one of them (which lies outside its own protected
/// range). Not on a thread whose debugger hooks are suppressed (heap work the
/// debugger runs on a parked thread). The unwinder asks only while
/// `DebuggerGates::stop_pending` is up.
pub(crate) fn exception_or_pending_stop(
    shared: &crate::vm::SharedVm,
    tid: u64,
    exc: crate::types::ObjectRef,
) -> crate::types::ObjectRef {
    take_stop_for_blocking_call(shared, tid).unwrap_or(exc)
}

/// The pending `ThreadReference.Stop` of thread `tid`, taken, for the code
/// that is about to throw in its place (interpreter round i1 wave 25): the
/// unwinder ([`exception_or_pending_stop`]), and a blocking native woken by
/// the stop's interrupt, which throws it instead of its
/// `InterruptedException` (`NativeContext::take_debugger_stop`: the sleep
/// natives, and `exceptions::throw_runtime_error` for an interrupted
/// `Object.wait` / `Thread.join`), so the stop is what the program's
/// handlers see whether the frame that catches runs interpreted or compiled.
/// `None` on a thread whose debugger hooks are suppressed (heap work the
/// debugger runs on a parked thread) or with no stop pending.
pub(crate) fn take_stop_for_blocking_call(
    shared: &crate::vm::SharedVm,
    tid: u64,
) -> Option<crate::types::ObjectRef> {
    if debugger_hooks_suppressed() {
        return None;
    }
    take_pending_stop(shared, tid)
}

// ---------------------------------------------------------------------------
// `-agentlib:jdwp` (interpreter round i4 wave 85, lane L1)
// ---------------------------------------------------------------------------

/// The socket transport's text for an I/O error, as `dbgsysGetLastIOError`
/// words it: the POSIX `strerror` text for the common socket errors on
/// every OS (MEASURED, Temurin 25.0.3 on Windows: a refused connect is
/// `connect failed: Connection refused`, not Winsock's message), else the
/// OS's message without Rust's ` (os error N)` suffix.
pub(in crate::debug) fn transport_error_text(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let posix = match error.kind() {
        ErrorKind::ConnectionRefused => Some("Connection refused"),
        ErrorKind::AddrInUse => Some("Address already in use"),
        ErrorKind::AddrNotAvailable => Some("Cannot assign requested address"),
        ErrorKind::TimedOut => Some("Connection timed out"),
        ErrorKind::ConnectionReset => Some("Connection reset by peer"),
        ErrorKind::PermissionDenied => Some("Permission denied"),
        ErrorKind::HostUnreachable => Some("No route to host"),
        ErrorKind::NetworkUnreachable => Some("Network is unreachable"),
        _ => None,
    };
    if let Some(text) = posix {
        return text.to_string();
    }
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The peers a `server=y` transport with `allow=` accepts (interpreter round
/// i7 wave 140, lane L1; [`parse_jdwp_allow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JdwpAllow {
    /// `allow=*`.
    All,
    /// IPv6 addresses (an IPv4 one as its IPv4-mapped form) and prefix
    /// lengths over the 128 bits.
    Peers(Vec<(u128, u8)>),
}

/// The socket transport's parse of `allow=<spec>` (`parseAllowedPeers`),
/// MEASURED on HotSpot 25.0.3 (`tools/probes/interp/L1/L1W140JdwpAllow.java`):
/// `*` alone accepts every peer, and a `*` with anything else is
/// `allow option '*' cannot be expanded` (the whole value shown); otherwise
/// `+`-separated entries (`;` is no separator), each a numeric IPv4
/// (dotted quad only: `127.1` is refused) or IPv6 address with an optional
/// `/prefix` of decimal digits, at most 32 for IPv4 and 128 for IPv6. The
/// error is `(shown, message)`: the text the `Error in allow option: '..'`
/// line quotes (the entry's address, or its prefix) and the transport
/// error's message. Pure.
pub(crate) fn parse_jdwp_allow(spec: &str) -> Result<JdwpAllow, (String, &'static str)> {
    if spec == "*" {
        return Ok(JdwpAllow::All);
    }
    if spec.contains('*') {
        return Err((spec.to_string(), "allow option '*' cannot be expanded"));
    }
    let mut peers = Vec::new();
    for entry in spec.split('+') {
        let (address, prefix) = match entry.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (entry, None),
        };
        let invalid_address = || (address.to_string(), "invalid IP address in allow option");
        let (bits, v4) = if let Ok(v4) = address.parse::<std::net::Ipv4Addr>() {
            (u128::from(v4.to_ipv6_mapped()), true)
        } else if let Ok(v6) = address.parse::<std::net::Ipv6Addr>() {
            (u128::from(v6), false)
        } else {
            return Err(invalid_address());
        };
        let max: u32 = if v4 { 32 } else { 128 };
        let length = match prefix {
            None => max,
            Some(text) => {
                let invalid_mask = || (text.to_string(), "invalid netmask in allow option");
                if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid_mask());
                }
                match text.parse::<u32>() {
                    Ok(n) if n <= max => n,
                    _ => return Err(invalid_mask()),
                }
            }
        };
        // An IPv4 prefix counts from the mapped form's 96 leading bits.
        let length = if v4 { length + 96 } else { length };
        // Truncation: at most 128.
        peers.push((bits, length as u8));
    }
    Ok(JdwpAllow::Peers(peers))
}

/// Does `allow` accept a debugger connecting from `peer`? An IPv4 peer is
/// compared in its IPv4-mapped form, as the transport's dual-stack socket
/// sees it. Pure.
pub(crate) fn jdwp_allow_admits(allow: &JdwpAllow, peer: std::net::IpAddr) -> bool {
    let bits = match peer {
        std::net::IpAddr::V4(v4) => u128::from(v4.to_ipv6_mapped()),
        std::net::IpAddr::V6(v6) => u128::from(v6),
    };
    match allow {
        JdwpAllow::All => true,
        JdwpAllow::Peers(peers) => peers.iter().any(|&(address, length)| {
            let mask = if length == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(length.min(128)))
            };
            bits & mask == address & mask
        }),
    }
}

/// A refused peer as the transport's `Peer not allowed to connect` line
/// names it: an IPv4 peer of a socket bound to every interface (`address=*`,
/// HotSpot's dual-stack socket) in its IPv4-mapped form, as MEASURED
/// (`::ffff:127.0.0.1`), else as it connected (`127.0.0.1` on a socket
/// bound to the loopback address). Pure.
pub(crate) fn jdwp_peer_text(peer: std::net::IpAddr, every_interface: bool) -> String {
    match peer {
        std::net::IpAddr::V4(v4) if every_interface => format!("::ffff:{v4}"),
        other => other.to_string(),
    }
}

/// Where the JDK back end's `transport.c` names its `could not connect`
/// exit (wave 140, lane L1): MEASURED on Windows (Temurin 25.0.3); elsewhere
/// the source-relative path JDK builds use there (not measured, as for
/// `config::JDWP_NO_TRANSPORTS_AT`).
#[cfg(windows)]
pub(in crate::debug) const JDWP_ACCEPT_TIMEOUT_AT: &str = "s\\src\\jdk.jdwp.agent\\share\\native\\libjdwp\\transport.c:368";
/// See the Windows definition above.
#[cfg(not(windows))]
pub(in crate::debug) const JDWP_ACCEPT_TIMEOUT_AT: &str = "src/jdk.jdwp.agent/share/native/libjdwp/transport.c:368";

/// One accept of the agent's listener (wave 140, lane L1): blocking without
/// a `deadline`; with one, polled until it passes (`None` then). The stream
/// handed on blocks, whatever the listener's mode was.
pub(in crate::debug) fn accept_until(
    listener: &std::net::TcpListener,
    deadline: Option<std::time::Instant>,
) -> Option<std::io::Result<std::net::TcpStream>> {
    let Some(deadline) = deadline else {
        return Some(listener.accept().map(|(stream, _)| stream));
    };
    if let Err(e) = listener.set_nonblocking(true) {
        return Some(Err(e));
    }
    let answer = loop {
        match listener.accept() {
            Ok((stream, _)) => break Some(stream.set_nonblocking(false).map(|()| stream)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => break Some(Err(e)),
        }
    };
    let _ = listener.set_nonblocking(false);
    answer
}

/// No debugger connected within `timeout=` (wave 140, lane L1): the back
/// end's two lines on stderr and the process ends with status 0, as
/// HotSpot 25.0.3 ends it (MEASURED: `ERROR: transport error 203: timed out
/// waiting for connection`, then `JDWP exit error JVMTI_ERROR_NONE(0): could
/// not connect, timeout or fatal error [...transport.c:368]`, exit status 0).
pub(in crate::debug) fn exit_on_accept_timeout(shared: &crate::vm::SharedVm) -> ! {
    use std::io::Write as _;
    // As `exit_for_debugger`: the VM's buffered streams first.
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    let _ = std::io::stdout().flush();
    {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "ERROR: transport error 203: timed out waiting for connection");
        let _ = writeln!(
            err,
            "JDWP exit error JVMTI_ERROR_NONE(0): could not connect, timeout or fatal error [{JDWP_ACCEPT_TIMEOUT_AT}]"
        );
        let _ = err.flush();
    }
    cratonvm_native_api::process_exit::exit_process(0)
}

/// Start the JDWP server `-agentlib:jdwp` asked for (`VmConfig::jdwp_agent`)
/// where the JDK back end's `initialize` runs, at `VMInit` (interpreter
/// round i4 wave 85, lane L1). The launcher calls it on the main thread
/// (`thread`) after the VM's boot and before the startup Java agents and
/// the main class:
///
/// * `server=y`: bind `address` (the loopback interface without a host, as
///   JDK 9+ does; every interface for `*`; an ephemeral port without one),
///   print `Listening for transport dt_socket at address: <port>` on stdout
///   (`TTY_MESSAGE`; not with `quiet=y`), and serve each connection in turn,
///   accepting again after a detach;
/// * `server=n`: connect to `address` (within `timeout=` when given) and
///   serve that one connection;
/// * `suspend=y`: wait for the debugger's session, send `VM_START` (request
///   0, this thread, `SUSPEND_ALL`) with the VM suspended, and hold this
///   thread until the debugger resumes it; `suspend=n` with a connection made
///   here (`server=n`) sends `VM_START` with `SUSPEND_NONE`, and a listener
///   (`server=y`) sends none, the debugger attaching later.
///
/// A transport that does not start is the back end's `ERROR:` lines and exit
/// status 2 ([`crate::config::JdwpAgentFailure::transport_init`]), for the
/// launcher to print. `Ok` with nothing to do without the option.
pub fn start_jdwp_agent(
    shared: &Arc<crate::vm::SharedVm>,
    thread: &mut crate::threading::jvm_thread::JvmThread,
) -> Result<(), crate::config::JdwpAgentFailure> {
    let Some(agent) = shared.config.jdwp_agent.clone() else {
        return Ok(());
    };
    // Interpreter round i6 wave 130 (lane L1b,
    // `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS`, default ON, `=0` drops
    // the option as before): `includevirtualthreads=y` lists the virtual
    // threads in `AllThreads` (`commands::listed_thread_ids`), as the back
    // end's `gdata->includeVThreads`. Set before the server starts; `n`
    // leaves `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS` in charge. Page:
    // docs/feature-designs/jvmti-delivery-threading.md.
    if agent.include_virtual_threads
    {
        shared.debug.debug_state.lock().include_virtual_threads = true;
    }
    // Interpreter round i7 wave 144, lane L1 (`CRATONVM_JDWP_AGENT_LAUNCH`,
    // parity, default ON; `=0` makes the launcher refuse the options):
    // `onthrow=` / `onuncaught=y` defer the back end's initialization, the
    // transport included, to the first throw that matches
    // ([`start_deferred_jdwp_agent`]), as `debugInit.c` defers it
    // (`initOnStartup` false). Nothing listens and nothing is printed until
    // then. Page:
    // docs/feature-designs/jvmti-delivery-threading.md.
    if agent.on_throw.is_some() || agent.on_uncaught {
        shared.debug.debugger_gates.arm_jdwp_deferred_start();
        return Ok(());
    }
    start_jdwp_transport(shared, thread, agent)
}

/// The deferred start's trigger (interpreter round i7 wave 144, lane L1):
/// exception `exc_class` is thrown on `thread`, `uncaught` when no frame on
/// the thread catches it. When this VM's `-agentlib:jdwp` deferred its start
/// (`onthrow=` / `onuncaught=y`) and the throw matches -- an exception of
/// exactly the `onthrow` class (the back end compares the class's signature,
/// so a subclass does not match), or any uncaught one with `onuncaught=y` --
/// the transport starts now, on the throwing thread, once: it listens (or
/// connects), runs `launch=`, and with `suspend=y` holds this thread until
/// the debugger resumes it, as the back end's `initialize(.., EI_EXCEPTION)`
/// does from its early `Exception` callback. A transport that fails ends the
/// process with the back end's lines, as at start. Answers whether the start
/// ran (the caller re-reads what a collection in the hold may have moved).
pub(crate) fn start_deferred_jdwp_agent(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    exc_class: crate::classloading::ClassId,
    uncaught: bool,
) -> bool {
    let Some(agent) = shared.config.jdwp_agent.clone() else {
        return false;
    };
    let matches = (agent.on_uncaught && uncaught)
        || agent.on_throw.as_deref().is_some_and(|wanted| {
            let cm = shared.classes.class_manager.read();
            cm.get_class(exc_class)
                .is_some_and(|class| class.name.replace('/', ".") == wanted)
        });
    if !matches || !shared.debug.debugger_gates.take_jdwp_deferred_start() {
        return false;
    }
    let Some(owned) = shared.self_arc.read().as_ref().and_then(|w| w.upgrade()) else {
        return false;
    };
    // What the program printed before the throw goes out before the
    // transport's `Listening` line (HotSpot's streams are unbuffered).
    let _ = shared.natives.fd_table.flush(1);
    let _ = shared.natives.fd_table.flush(2);
    if let Err(failure) = start_jdwp_transport(&owned, thread, agent) {
        use std::io::Write as _;
        let _ = shared.natives.fd_table.flush(1);
        let _ = std::io::stdout().flush();
        let mut err = std::io::stderr().lock();
        for line in &failure.stderr_lines {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
        drop(err);
        cratonvm_native_api::process_exit::exit_process(failure.exit_code);
    }
    true
}

/// `launch=<command>` (interpreter round i7 wave 144, lane L1): run
/// `<command> <transport> <address>` once the transport is up, as the back
/// end's `launch()` builds it and `dbgsysExec` runs it: on Windows the whole
/// line goes to `CreateProcess` (the first word names the program, the rest
/// is its command line as written, no shell), elsewhere it is split at
/// whitespace and `execvp`'d. The child inherits the VM's standard streams
/// and is not waited for. A command that cannot be started ends the start
/// with exit status 1 (PREDICTED text: `ERROR: JDWP unable to launch: <line>`).
pub(in crate::debug) fn launch_jdwp_debugger(command: &str, address: &str) -> Result<(), crate::config::JdwpAgentFailure> {
    let line = format!("{command} dt_socket {address}");
    let refused = || crate::config::JdwpAgentFailure {
        stderr_lines: vec![format!("ERROR: JDWP unable to launch: {line}")],
        exit_code: 1,
    };
    #[cfg(windows)]
    let spawned = {
        use std::os::windows::process::CommandExt as _;
        let trimmed = line.trim_start();
        let (program, rest) = if let Some(quoted) = trimmed.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((program, rest)) => (program, rest),
                None => (quoted, ""),
            }
        } else {
            trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""))
        };
        std::process::Command::new(program).raw_arg(rest.trim_start()).spawn()
    };
    #[cfg(not(windows))]
    let spawned = {
        let mut words = line.split_whitespace();
        match words.next() {
            Some(program) => std::process::Command::new(program).args(words).spawn(),
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    };
    match spawned {
        Ok(_child) => Ok(()),
        Err(_) => Err(refused()),
    }
}

/// [`start_jdwp_agent`]'s transport: listen or connect, run `launch=`, and
/// with `suspend=y` hold `thread` until the debugger resumes it. Called at
/// `VMInit`, or from the deferred start's trigger
/// ([`start_deferred_jdwp_agent`], wave 144).
pub(in crate::debug) fn start_jdwp_transport(
    shared: &Arc<crate::vm::SharedVm>,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    agent: crate::config::JdwpAgentConfig,
) -> Result<(), crate::config::JdwpAgentFailure> {
    use crate::config::JdwpAgentFailure;
    // Wave 144: the address `launch=` is given, as the transport answers it
    // (the port a listener bound, the connect address as written).
    let launch_address;
    let server_ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let owned = Arc::clone(shared);
    let ended = Arc::clone(&server_ended);
    if agent.server {
        // Interpreter round i7 wave 140, lane L1 (`CRATONVM_JDWP_AGENT_ALLOW`,
        // parity, default ON, `=0` drops the option as before): the
        // transport parses `allow=` before it listens, and refuses a bad list
        // with its `Error in allow option` line and the transport error
        // (MEASURED, `L1W140JdwpAllow`); each accepted peer outside the list
        // is closed with `Debugger failed to attach: ERROR: Peer not allowed
        // to connect: <peer>` on stderr, and the transport keeps listening.
        // Page: docs/feature-designs/jvmti-delivery-threading.md.
        let allow = match agent.allow.as_deref() {
            Some(spec)
                if true =>
            {
                match parse_jdwp_allow(spec) {
                    Ok(allow) => Some(allow),
                    Err((shown, message)) => {
                        let mut failure = JdwpAgentFailure::transport_init_with(103, message);
                        failure
                            .stderr_lines
                            .insert(0, format!("Error in allow option: '{shown}'"));
                        return Err(failure);
                    }
                }
            }
            _ => None,
        };
        // Wave 140 (L1, `CRATONVM_JDWP_AGENT_RELISTEN_LINE`, parity, default
        // ON, `=0` prints the line once as before): the transport prints its
        // `Listening for transport` line again each time it listens anew
        // after a session ended (MEASURED, `L1W140JdwpAllow` cases B-F: the
        // line twice), not after a refused peer.
        // Wave 144 (L1): with `launch=` the back end prints no `Listening`
        // line at all, neither at the listen nor after a session (MEASURED,
        // HotSpot 25.0.3, `L1W144JdwpLaunchOnThrow` A-C and F: the TTY
        // message is printed only when no debugger is launched), the
        // deferred `onthrow` / `onuncaught` start included.
        let listening_lines = !agent.quiet && agent.launch.is_none();
        let relisten_line = listening_lines;
        let every_interface = agent.host.as_deref() == Some("*");
        let host = match agent.host.as_deref() {
            None | Some("localhost") => "127.0.0.1",
            Some("*") => "0.0.0.0",
            Some(host) => host,
        };
        let listener = std::net::TcpListener::bind((host, agent.port)).map_err(|e| {
            JdwpAgentFailure::transport_init(&format!("bind failed: {}", transport_error_text(&e)))
        })?;
        let port = listener
            .local_addr()
            .map_or(agent.port, |address| address.port());
        launch_address = port.to_string();
        let print_listening = move || {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "Listening for transport dt_socket at address: {port}");
            let _ = out.flush();
        };
        if listening_lines {
            print_listening();
        }
        // Wave 140 (L1, `CRATONVM_JDWP_AGENT_ACCEPT_TIMEOUT`, parity, default
        // ON, `=0` waits for ever as before): `timeout=<ms>` (not 0) bounds
        // each listen of a `server=y` transport, the first and every one
        // after a session; when no debugger connects in time the back end
        // prints its two lines on stderr and ends the process with status 0
        // (MEASURED, HotSpot 25.0.3, `suspend=n` and `suspend=y` alike).
        let accept_timeout = agent
            .timeout_ms
            .filter(|&ms| ms > 0)
            .filter(|_| {
                true
            })
            .map(std::time::Duration::from_millis);
        std::thread::Builder::new()
            .name("JDWP-Server".into())
            .spawn(move || {
                let mut first = true;
                let streams = std::iter::from_fn(|| {
                    if !std::mem::replace(&mut first, false) && relisten_line {
                        print_listening();
                    }
                    let deadline = accept_timeout.map(|wait| std::time::Instant::now() + wait);
                    loop {
                        let accepted = match accept_until(&listener, deadline) {
                            Some(accepted) => accepted,
                            None => exit_on_accept_timeout(&owned),
                        };
                        let refused_peer = match (allow.as_ref(), &accepted) {
                            (Some(allow), Ok(stream)) => stream
                                .peer_addr()
                                .ok()
                                .filter(|peer| !jdwp_allow_admits(allow, peer.ip())),
                            _ => None,
                        };
                        let Some(peer) = refused_peer else {
                            return Some(accepted);
                        };
                        {
                            use std::io::Write as _;
                            let mut err = std::io::stderr().lock();
                            let _ = writeln!(
                                err,
                                "Debugger failed to attach: ERROR: Peer not allowed to connect: {}",
                                jdwp_peer_text(peer.ip(), every_interface)
                            );
                            let _ = err.flush();
                        }
                        // Closed before the handshake; the transport listens on.
                        drop(accepted);
                    }
                });
                run_jdwp_sessions(&owned, streams);
                ended.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|e| JdwpAgentFailure::transport_init(&transport_error_text(&e)))?;
    } else {
        let host = agent.host.as_deref().unwrap_or("localhost");
        let connect_failed = |e: &std::io::Error| {
            JdwpAgentFailure::transport_init(&format!("connect failed: {}", transport_error_text(e)))
        };
        let addresses: Vec<std::net::SocketAddr> =
            std::net::ToSocketAddrs::to_socket_addrs(&(host, agent.port))
                .map_err(|e| connect_failed(&e))?
                .collect();
        let mut last_error = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
        let mut connected = None;
        for address in addresses {
            let attempt = match agent.timeout_ms.filter(|&ms| ms > 0) {
                Some(ms) => std::net::TcpStream::connect_timeout(
                    &address,
                    std::time::Duration::from_millis(ms),
                ),
                None => std::net::TcpStream::connect(address),
            };
            match attempt {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(e) => last_error = e,
            }
        }
        let stream = connected.ok_or_else(|| connect_failed(&last_error))?;
        std::thread::Builder::new()
            .name("JDWP-Server".into())
            .spawn(move || {
                run_jdwp_sessions(&owned, std::iter::once(Ok(stream)));
                ended.store(true, std::sync::atomic::Ordering::Release);
            })
            .map_err(|e| JdwpAgentFailure::transport_init(&transport_error_text(&e)))?;
        launch_address = match agent.host.as_deref() {
            Some(host) => format!("{host}:{}", agent.port),
            None => agent.port.to_string(),
        };
    }
    // Wave 144 (L1): `launch=`, once the transport is up.
    if let Some(command) = agent.launch.as_deref() {
        launch_jdwp_debugger(command, &launch_address)?;
    }
    shared.debug.debugger_gates.set_jdwp_listening();
    if !agent.suspend && agent.server {
        return Ok(());
    }
    let tid = thread.thread_id.0;
    let mut ctx = crate::vm::NativeContextImpl {
        shared: shared.as_ref(),
        thread,
    };
    cratonvm_native_api::NativeThreadAccess::begin_blocking_region(&mut ctx);
    let answer = hold_for_vm_start(shared, tid, agent.suspend, &server_ended);
    cratonvm_native_api::NativeThreadAccess::end_blocking_region(&mut ctx);
    answer
}

/// [`start_jdwp_agent`]'s wait at `VMInit` (wave 85), in a blocking region:
/// until the debugger's session is attached (a `server=n` connection whose
/// handshake fails ends the server: the back end's
/// `handshake failed - connection prematurally closed`), then `VM_START`,
/// and with `suspend` the hold until thread `tid` is resumed (or the
/// debugger detaches).
pub(in crate::debug) fn hold_for_vm_start(
    shared: &crate::vm::SharedVm,
    tid: u64,
    suspend: bool,
    server_ended: &std::sync::atomic::AtomicBool,
) -> Result<(), crate::config::JdwpAgentFailure> {
    let pause = || std::thread::sleep(std::time::Duration::from_millis(2));
    while !shared.debug.debugger_gates.session_attached() {
        if server_ended.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::config::JdwpAgentFailure::transport_init(
                "handshake failed - connection prematurally closed",
            ));
        }
        pause();
    }
    let policy = if suspend {
        events::SuspendPolicy::All
    } else {
        events::SuspendPolicy::None
    };
    {
        let mut ds = shared.debug.debug_state.lock();
        if suspend {
            ds.suspend_all();
            publish_debugger_gates(shared, &ds);
        }
    }
    send_event_set(
        shared,
        vec![DebugEvent {
            kind: events::EventKind::VMStart,
            request_id: 0,
            suspend_policy: policy,
            thread_id: tid,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        }],
    );
    while suspend
        && shared.debug.debugger_gates.session_attached()
        && shared.debug.debug_state.lock().is_thread_suspended(tid)
    {
        pause();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `VM_DEATH` (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------

/// How long [`report_vm_death`] waits for the server to write the event.
pub(in crate::debug) const VM_DEATH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The VM is ending (interpreter round i1 wave 24): report `VM_DEATH` to an
/// attached debugger and wait, at most [`VM_DEATH_WAIT`], until the JDWP
/// server has written it. HotSpot's back end sends the event — one per
/// `VMDeath` request, then request id 0, always, whatever the requests say
/// (wave 26: that order, measured; request 0 went first), in one set with
/// the strongest of their policies — before the transport closes,
/// so JDI delivers a `VMDeathEvent` before the `VMDisconnectEvent`
/// (jdb: "The application exited"). It was never sent: the debugger saw the
/// connection drop.
///
/// A `VMDeath` request's `SUSPEND_ALL` (interpreter round i1 wave 26, lane
/// L1) suspends the VM before the set is sent and holds the dying thread
/// until the debugger resumes it (or detaches), so the debugger can inspect
/// the VM before it dies, as on HotSpot (`VMDeathRequest`'s one use; JDI
/// creates one only since `canRequestVMDeathEvent` answers true). The hold
/// runs in a blocking region: a debugger command served meanwhile may
/// collect, and a GC-blocked thread is one the collector does not wait for.
/// `SUSPEND_EVENT_THREAD` suspends and holds the dying thread alone
/// ([`vm_death_event_set`] has HotSpot's measured behaviour).
///
/// Called by the launcher on both of its exit arms (`Java main` returned,
/// and `System.exit` through its pre-exit hook), after the shutdown hooks,
/// where HotSpot posts JVMTI `VMDeath` (`before_exit`). A no-op without an
/// attached debugger, and after the first call.
pub fn report_vm_death(shared: &crate::vm::SharedVm) {
    if !shared.debug.debugger_gates.session_attached() {
        return;
    }
    // The dying thread, when this VM knows the calling OS thread: it is held
    // by its own suspend count, else by the VM-wide one (an id no thread has).
    let dying = crate::native::jni::current_jvm_thread_of(shared);
    // SAFETY: `current_jvm_thread_of` answers the live `JvmThread` running
    // this call on this OS thread; `thread_id` is an immutable `Copy` field.
    let dying_tid = dying.map_or(u64::MAX, |thread| unsafe { (*thread).thread_id.0 });
    let (set, held) = {
        let mut ds = shared.debug.debug_state.lock();
        if ds.vm_death_sent {
            return;
        }
        let (set, held) = vm_death_event_set(&mut ds, dying.map(|_| dying_tid));
        if held {
            publish_debugger_gates(shared, &ds);
        }
        (set, held)
    };
    send_event_set(shared, set);
    if held {
        hold_for_vm_death(shared, dying, dying_tid);
        return;
    }
    let until = std::time::Instant::now() + VM_DEATH_WAIT;
    while std::time::Instant::now() < until && shared.debug.debugger_gates.session_attached() {
        if shared.debug.debug_state.lock().vm_death_sent {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// The `VM_DEATH` event set [`report_vm_death`] sends, with its suspension
/// applied to `ds` (wave 26): one event per `VMDeath` request, then the
/// unsolicited one (request 0), all with the strongest of the requests'
/// policies — the order HotSpot's back end sends them in (measured with JDI:
/// the `EventSet` iterates the `VMDeathRequest`'s event before the one with
/// no request). `SUSPEND_ALL` suspends the VM; `SUSPEND_EVENT_THREAD`
/// suspends `dying`, the thread reporting the death, when it is known — the
/// event names no thread, but HotSpot's back end suspends the thread that
/// posts it (measured: JDI's `EventSet.resume()` then throws "Inconsistent
/// suspend policy" and the VM stays held until a `VirtualMachine.Resume` or
/// a detach). Answers the set and whether a suspension was applied (the
/// caller then holds the dying thread).
pub(in crate::debug) fn vm_death_event_set(ds: &mut DebugState, dying: Option<u64>) -> (Vec<DebugEvent>, bool) {
    let hits = ds.events.match_thread_events(events::EventKind::VMDeath, 0);
    let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
    let held = match (policy, dying) {
        (events::SuspendPolicy::All, _) => {
            ds.suspend_all();
            true
        }
        (events::SuspendPolicy::EventThread, Some(tid)) => {
            ds.suspend_thread(tid);
            true
        }
        _ => false,
    };
    let set = hits
        .into_iter()
        .map(|(request_id, _)| request_id)
        .chain(std::iter::once(0u32))
        .map(|request_id| DebugEvent {
            kind: events::EventKind::VMDeath,
            request_id,
            suspend_policy: policy,
            thread_id: 0,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    (set, held)
}

/// [`report_vm_death`]'s hold for a suspending `VMDeath` request (wave
/// 26): wait, GC-blocked when `dying` is this VM's thread, until thread
/// `dying_tid` is no longer suspended (the debugger resumed the set, or
/// detached, which resumes everything) or the session ends.
pub(in crate::debug) fn hold_for_vm_death(
    shared: &crate::vm::SharedVm,
    dying: Option<*mut crate::threading::jvm_thread::JvmThread>,
    dying_tid: u64,
) {
    let wait = || {
        while shared.debug.debugger_gates.session_attached()
            && shared
                .debug
                .debug_state
                .lock()
                .is_thread_suspended(dying_tid)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    match dying {
        // gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 2): already GC-safe -- the exit native opened its region on
        // its own context (`CRATONVM_EXIT_HOOK_GC_SAFE`) -- so just wait: no
        // `&mut` of a thread whose `&mut` that native's context holds.
        Some(thread) if dying_thread_already_gc_safe(thread) => wait(),
        Some(thread) => {
            // SAFETY: `current_jvm_thread_of` answered this OS thread's own
            // live `JvmThread`, which only this thread uses while it runs
            // this call (the launcher's exit arm, or the `System.exit`
            // native through its pre-exit hook, whose own context it
            // stands in for until the region ends).
            let mut ctx = crate::vm::NativeContextImpl {
                shared,
                thread: unsafe { &mut *thread },
            };
            cratonvm_native_api::NativeThreadAccess::begin_blocking_region(&mut ctx);
            wait();
            cratonvm_native_api::NativeThreadAccess::end_blocking_region(&mut ctx);
        }
        // Not a thread of this VM: nothing waits for it.
        None => wait(),
    }
}

/// gci i11/ja: whether the dying thread `thread` (this OS thread's own, from
/// `current_jvm_thread_of`) is already in its blocked region. One atomic read
/// through a shared reference; nothing is written.
pub(in crate::debug) fn dying_thread_already_gc_safe(thread: *mut crate::threading::jvm_thread::JvmThread) -> bool {
    // SAFETY: this OS thread's own live `JvmThread` (see `hold_for_vm_death`);
    // only an atomic is read, through a shared reference.
    let view = unsafe { &*(thread as *const crate::threading::jvm_thread::JvmThread) };
    view.gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// Object ids (wave 7): the VM half of `ids::ObjectTable`
// ---------------------------------------------------------------------------

/// The VM's collection count, which [`ids::ObjectTable`]'s address index is
/// keyed by: `GcBarrier::gc_generation`, bumped once at the end of every
/// pause, after the pause's remap of the JNI global references.
pub(in crate::debug) fn gc_epoch(shared: &crate::vm::SharedVm) -> u64 {
    shared
        .mem
        .gc_barrier
        .gc_generation
        .load(std::sync::atomic::Ordering::Acquire)
}

/// The JDWP object id of `obj`, minting one on first export. The same object
/// always gets the same id while the debugger holds it, however far a
/// collection moved it; ids never collide with thread ids (see
/// [`ids::HEAP_OBJECT_ID_BASE`]).
///
/// The id is backed by a JNI WEAK global reference (wave 8; a strong one
/// while `DisableCollection` is in force, [`set_collection_enabled`]): every
/// collection remaps it or clears it (`jni::sweep_weak_global_refs` and the
/// concurrent-cycle sweeps), so no new side table exists, and an id no
/// longer keeps its object alive, as in HotSpot. Lock order: debug state,
/// then the JNI global-reference table.
///
/// Call only where no collection can run concurrently: on a mutator between
/// two safepoint polls (a parked thread, the suspend point), or on the
/// server inside `GcBarrier::run_if_no_stw_requested`.
pub(crate) fn export_object(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    obj: crate::types::ObjectRef,
    how: ObjectExport,
) -> u64 {
    let epoch = gc_epoch(shared);
    if ds.objects.needs_rekey(epoch) {
        let globals = shared.natives.jni_global_refs.lock();
        ds.objects.rekey(epoch, |handle| {
            // Cast: a referent's address is the index key.
            globals.resolve(handle).map(|r| r.as_ptr() as usize)
        });
    }
    let addr = obj.as_ptr() as usize; // Cast: the index key
    if let Some((id, handle)) = ds.objects.candidate_for_addr(addr) {
        // A weak referent can die — and its address go to a new object —
        // between two rekeys: a concurrent cycle clears weak handles outside
        // a pause. The handle, not the index, says whose address it is.
        let current = shared.natives.jni_global_refs.lock().resolve(handle);
        if current.is_some_and(|r| r.as_ptr() as usize == addr) {
            ds.objects.note_export(id, how);
            return id;
        }
        ds.objects.forget_addr(id);
    }
    let handle = shared.natives.jni_global_refs.lock().add_weak(obj);
    let id = ds.ids.fresh_object_id().0;
    ds.objects.insert(id, handle, addr, how);
    id
}

/// A frame local as a suspended thread's published snapshot records it
/// (`interpreter::publish_debugger_frames`, `StackFrame.SetValues`): a
/// reference as its object id, pinned while the snapshot stands (the
/// snapshot's withdrawal unpins it). Same calling rule as [`export_object`].
pub(crate) fn snapshot_local(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    v: crate::types::Value,
) -> LocalValue {
    use crate::types::Value;
    match v {
        Value::Int(i) => LocalValue::Int(i),
        Value::Long(l) => LocalValue::Long(l),
        Value::Float(f) => LocalValue::Float(f),
        Value::Double(d) => LocalValue::Double(d),
        Value::Object(Some(r)) => {
            LocalValue::ObjectRef(export_object(shared, ds, r, ObjectExport::Pinned))
        }
        // A slot never written, or a `long`'s / `double`'s second slot, is no
        // reference (interpreter round i1 wave 42): HotSpot types it as an
        // `int`, so asking it for an object is `TYPE_MISMATCH`
        // (`commands::local_read_refusal`). It read as a null reference.
        Value::Uninitialized => LocalValue::Int(0),
        _ => LocalValue::ObjectRef(0),
    }
}

/// Local `slot` of `frame` as the debugger reads it (interpreter round i1
/// wave 15). `Frame::get_local` decodes a slot context-free except for a
/// `double`; a `long` is stored as its verbatim bits, so it came back as a
/// `double` with those bits (or, for a few bit patterns, as another type),
/// and `StackFrame.GetValues` answered 0 for every `long` local (the snapshot
/// held no `Long`), while `SetValues` of a `long` local of a class compiled
/// without `-g` was refused as a type mismatch. The slot's kind mark says
/// which it is.
pub(crate) fn debugger_local(
    frame: &crate::runtime::frame::Frame,
    slot: u16,
) -> crate::types::Value {
    let i = usize::from(slot);
    if i < frame.locals_len() && frame.get_local_tag(i) == crate::types::VTAG_LONG {
        // Cast: the long's verbatim bits.
        return crate::types::Value::Long(frame.get_local_raw(i) as i64);
    }
    frame.get_local(slot)
}

/// [`debugger_local`] for a READ whose object answer becomes a handle (a
/// JDWP snapshot's pinned export, a JVMTI `GetLocalObject` reference). gcf
/// f2/f, `gcf-f1f-debugger-local-reads-export-liveness-dead-stale-locals-FIXED-20261012.md`.
///
/// A reference local the frame's per-bci liveness calls dead (`live`, from
/// `Frame::live_locals_mask_here`) is not a root (`Frame::scan_local_objects`),
/// so a collection may have freed or moved its referent and left the old
/// address in the slot, with its object tag. A handle to that address is a
/// root to garbage. Such a slot is answered as its object only while the
/// address is still an object start in the heap (the validator the
/// conservative stack scans use on stale words, which reads no header outside
/// a committed arena); otherwise as `null`. A live slot is answered as
/// before, and so is every slot when no collection has run since it died --
/// HotSpot keeps every local alive under a debugger and shows the object.
pub(crate) fn debugger_local_for_read(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    slot: u16,
    live: &crate::runtime::local_liveness::LiveMask,
) -> crate::types::Value {
    screen_dead_local(shared, live, slot, debugger_local(frame, slot))
}

/// The screen of [`debugger_local_for_read`] over a value already read (and,
/// for a blocked thread's window, already brought to the referent's current
/// address).
pub(crate) fn screen_dead_local(
    shared: &crate::vm::SharedVm,
    live: &crate::runtime::local_liveness::LiveMask,
    slot: u16,
    v: crate::types::Value,
) -> crate::types::Value {
    match v {
        crate::types::Value::Object(Some(r))
            if !live.is_live(usize::from(slot))
                // Cast: the reference's address, probed, never dereferenced here.
                && !shared.mem.heap.is_object_start(r.as_ptr() as usize) =>
        {
            crate::types::Value::Object(None)
        }
        other => other,
    }
}

/// The object a JDWP object id names now, wherever a collection moved it;
/// `None` for an id this session never handed out, has disposed of, or whose
/// object was collected (JDWP `INVALID_OBJECT`).
///
/// Resolving a weak handle is a keep-alive, as `jni::jobject_to_obj` makes
/// it: a concurrent mark may not have reached the referent (a weak handle is
/// not a root), and whatever the debugger does with it next — store it into a
/// field, pass it to an invocation — must not leave it unmarked. Same calling
/// rule as [`export_object`].
pub(crate) fn object_for_id(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<crate::types::ObjectRef> {
    let Some(handle) = ds.objects.handle_of(id) else {
        return thread_object_for_id(shared, ds, id);
    };
    let (obj, weak) = {
        let globals = shared.natives.jni_global_refs.lock();
        let obj = globals.resolve(handle);
        (obj, obj.is_some() && globals.is_weak(handle))
    };
    if weak {
        if let Some(o) = obj {
            shared
                .mem
                .heap
                .referent_keep_alive(crate::types::Value::Object(Some(o)));
        }
    }
    obj
}

/// The `java.lang.Thread` of the live thread a JDWP THREAD id names, when a
/// command takes it as an object id (interpreter round i1 wave 23).
///
/// In HotSpot a `threadID` IS the thread object's `objectID`: JDI's
/// `ThreadReference` is an `ObjectReference`, so `referenceType()`,
/// `toString()`, `getValue(field)` or an invocation on a thread send the
/// `ObjectReference` commands with the thread id — jdb's `threads` prints
/// every thread through `referenceType()` (`Env.description`). This server's
/// thread ids are the VM's own thread ids, which the object table never
/// hands out (its ids start at [`ids::HEAP_OBJECT_ID_BASE`], so the two
/// cannot collide), and those commands answered `INVALID_OBJECT`, which JDI
/// turns into `ObjectCollectedException`: jdb's `threads` failed. The thread
/// must be one the server lists (`thread_names`); its object comes from the
/// thread registry, current wherever a collection moved it (the callers run
/// with no collection possible, as for any id).
///
/// Lock order: the debug state, then the registry's thread map, read-only
/// and innermost (no registry method calls into the debugger).
///
/// Interpreter round i1 wave 24: a thread the registry knows counts even
/// before the server's thread table has caught up with it (that table is
/// refreshed only before the commands that list or target threads,
/// [`refreshes_thread_names`]): a thread started after the last refresh — a
/// `ThreadStart` event's, or one a thread value names (`t`,
/// `inspect::thread_value`) — answered `INVALID_OBJECT`, which JDI turns
/// into `ObjectCollectedException`, for its `referenceType()`. The heap
/// service, the back end's own thread, stays hidden.
pub(in crate::debug) fn thread_object_for_id(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<crate::types::ObjectRef> {
    // The id is a wire id (wave 25): the main thread's is
    // `ids::MAIN_THREAD_WIRE_ID`, and 0 is null. Wave 104 (lane L1): once
    // `main` has ended, `DESTROY_JAVA_VM_WIRE_ID` names VM thread 0, and
    // `main`'s id the ended `main`, whose `Thread` is no longer the entry's.
    let ended_main = ds.destroy_java_vm_split.is_some() && id == ids::MAIN_THREAD_WIRE_ID;
    let id = ds.vm_thread(id);
    if ended_main {
        return ds.ended_main_thread_obj(shared);
    }
    if id >= ids::HEAP_OBJECT_ID_BASE || ds.heap_service_thread() == Some(id) {
        return None;
    }
    shared
        .threads
        .thread_registry
        .java_thread_obj(crate::threading::ThreadId(id))
}

/// Is the object behind a live id gone? `None` for an id the table does not
/// know (JDWP `INVALID_OBJECT`). `ObjectReference.IsCollected`. A live
/// thread's id (wave 23, [`thread_object_for_id`]) is never collected.
pub(crate) fn object_collected(
    shared: &crate::vm::SharedVm,
    ds: &DebugState,
    id: u64,
) -> Option<bool> {
    let Some(handle) = ds.objects.handle_of(id) else {
        return thread_object_for_id(shared, ds, id).map(|_| false);
    };
    Some(
        shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(handle)
            .is_none(),
    )
}

/// `ObjectReference.DisableCollection` (`enable == false`) /
/// `EnableCollection` (`true`), which JDWP nests: the first disable swaps the
/// id's weak handle for a strong one, the last enable swaps it back. `false`
/// for an unknown id or one whose object is already gone (`INVALID_OBJECT`).
/// The replaced handle is freed by [`release_disposed_objects`].
pub(crate) fn set_collection_enabled(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    id: u64,
    enable: bool,
) -> bool {
    let Some(handle) = ds.objects.handle_of(id) else {
        // A live thread's id (wave 23, [`thread_object_for_id`]): its
        // object is reachable while the thread runs, so there is nothing to
        // pin, and the command succeeds as HotSpot's does.
        return thread_object_for_id(shared, ds, id).is_some();
    };
    let Some(obj) = shared.natives.jni_global_refs.lock().resolve(handle) else {
        // Enabling collection of a collected object is harmless (HotSpot
        // accepts it); disabling it is too late.
        if enable {
            let _ = ds.objects.enable_collection(id);
        }
        return enable;
    };
    let count = if enable {
        ds.objects.enable_collection(id)
    } else {
        ds.objects.disable_collection(id)
    };
    let swap_to_strong = !enable && count == Some(1);
    let is_weak = shared.natives.jni_global_refs.lock().is_weak(handle);
    let swap_to_weak = enable && count == Some(0) && !is_weak;
    if swap_to_strong || swap_to_weak {
        let fresh = {
            let mut globals = shared.natives.jni_global_refs.lock();
            if swap_to_strong {
                globals.add(obj)
            } else {
                globals.add_weak(obj)
            }
        };
        ds.objects.replace_handle(id, fresh);
    }
    true
}

/// Free the JNI global references behind the object ids that are gone
/// (disposed of, unpinned, or the session ended).
pub(crate) fn release_disposed_objects(shared: &crate::vm::SharedVm, ds: &mut DebugState) {
    let handles = ds.objects.take_released();
    if handles.is_empty() {
        return;
    }
    let mut globals = shared.natives.jni_global_refs.lock();
    for handle in handles {
        let _ = globals.remove(handle);
    }
}

// ---------------------------------------------------------------------------
// Event object holds (interpreter round i6 wave 127, lane L1)
// ---------------------------------------------------------------------------
//
// An object id is backed by a WEAK handle (wave 8), as HotSpot's back end
// keeps its `commonRef` nodes. An `Exception` event with `SUSPEND_NONE` lets
// the thread run on: the exception is caught, unreachable, and a collection
// that ends before the debugger's first `ObjectReference.ReferenceType`
// takes it -- JDI then throws `ObjectCollectedException` from
// `ExceptionEvent.exception().referenceType()` (`L1W93JdiExceptionFromCompiled`,
// `--compatible` with the JIT, 4 of 4 runs). HotSpot's `eventHelper` holds
// the event's objects strongly until the packet is written, and its
// collections do not land in the debugger's round trip after it; here one
// did. So the event's object is held with its collection disabled (the
// `DisableCollection` count, which nests with the debugger's own) until the
// first heap command that names it, which then sees it as HotSpot's debugger
// does and leaves it as weak as before. `CRATONVM_JDWP_EVENT_OBJECT_HOLD=0`
// restores the bare weak id. Page:
// docs/feature-designs/jvmti-delivery-threading.md.

/// At most this many event objects are held; past it the oldest hold goes
/// (a raw JDWP client that never looks at the objects must not pin every
/// exception the session reported).
pub(crate) const EVENT_OBJECT_HOLD_CAP: usize = 1024;

/// Hold `obj`, which an event being built exported just now (same
/// debug-state critical section), with its collection disabled until the
/// debugger's first heap command naming its id ([`release_event_hold_named`]).
/// No-op under `CRATONVM_JDWP_EVENT_OBJECT_HOLD=0`, for an object exported as
/// something else (a live thread goes out as its thread id), or an id already
/// held. Same calling rule as [`export_object`] (no collection can run).
pub(crate) fn hold_event_object(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    obj: crate::types::ObjectRef,
) {
    if !ds.event_object_hold {
        return;
    }
    // Cast: the address index key. The export that just ran rekeyed the
    // index and confirmed (or minted) the entry at this address.
    let Some((id, _)) = ds.objects.candidate_for_addr(obj.as_ptr() as usize) else {
        return;
    };
    if ds.event_object_holds.contains(&id) || !set_collection_enabled(shared, ds, id, false) {
        return;
    }
    ds.event_object_holds.push_back(id);
    while ds.event_object_holds.len() > EVENT_OBJECT_HOLD_CAP {
        if let Some(oldest) = ds.event_object_holds.pop_front() {
            let _ = set_collection_enabled(shared, ds, oldest, true);
        }
    }
    // The weak handles the swaps replaced.
    release_disposed_objects(shared, ds);
}

/// After a heap command (`inspect::execute`, no collection able to run):
/// release the event hold of the object id the command names first, if it
/// is held ([`hold_event_object`]). The object-centred command sets
/// (`ObjectReference`, `StringReference`, `ArrayReference`,
/// `ClassLoaderReference`, `ClassObjectReference`) all start with it.
pub(crate) fn release_event_hold_named(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    command_set: u8,
    data: &[u8],
) {
    if ds.event_object_holds.is_empty() {
        return;
    }
    if !matches!(
        command_set,
        commands::CS_OBJECT_REF
            | commands::CS_STRING_REF
            | commands::CS_ARRAY_REF
            | commands::CS_CLASSLOADER_REF
            | commands::CS_CLASS_OBJ_REF
    ) {
        return;
    }
    let Ok(id) = protocol::PayloadReader::new(data).read_u64_be() else {
        return;
    };
    let Some(at) = ds.event_object_holds.iter().position(|&held| held == id) else {
        return;
    };
    ds.event_object_holds.remove(at);
    // `false` for an id disposed of meanwhile: nothing left to release.
    let _ = set_collection_enabled(shared, ds, id, true);
    release_disposed_objects(shared, ds);
}

thread_local! {
    /// Set on the thread running [`run_jdwp_server`]; see
    /// [`debugger_hooks_suppressed`].
    pub(in crate::debug) static JDWP_SERVER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// How many heap-work [`ParkedTask`]s the calling thread is running
    /// (wave 8; wave 9: invocations no longer count); see
    /// [`debugger_hooks_suppressed`].
    pub(in crate::debug) static DEBUGGER_WORK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Must the interpreter's JDWP suspend point leave the calling thread alone?
/// True on the JDWP server thread, and on a parked thread while it runs heap
/// work for the debugger (a command that reads or writes the heap, which the
/// server waits for): stopping there would wait on the server, which is
/// waiting for that very work.
///
/// Not while a parked thread runs a debugger INVOCATION (wave 9): the server
/// answers an invocation when it returns and serves commands meanwhile
/// (`run_jdwp_server`), so a breakpoint or step inside the invoked method is
/// reported and parks the thread again, above the invocation's frames, as in
/// HotSpot. Until wave 9 those events were dropped.
pub(crate) fn debugger_hooks_suppressed() -> bool {
    JDWP_SERVER_THREAD.with(|f| f.get()) || DEBUGGER_WORK_DEPTH.with(|d| d.get()) > 0
}

// ---------------------------------------------------------------------------
// Work the JDWP server hands to a parked thread (wave 8)
// ---------------------------------------------------------------------------

/// Work the JDWP server runs on a thread parked at an interpreter suspend
/// point ([`run_on_parked_thread`], [`run_invocation_on_parked_thread`]). It
/// answers through a channel it captured, when the park loop sends its
/// [`ParkedReply`].
pub(crate) struct ParkedTask {
    pub(in crate::debug) run: ParkedRun,
    /// A debugger invocation (wave 9): runs Java whose events are reported,
    /// and may park the thread again at a stop inside it. Heap work keeps the
    /// suspend point suppressed ([`debugger_hooks_suppressed`]).
    pub(in crate::debug) invocation: bool,
    /// Queued for any parked thread (no thread named: heap work, wave 66):
    /// it neither reads nor writes the frames of the thread that runs it, so
    /// a park's standing deposit outlives it ([`ParkGcRegion`]).
    pub(in crate::debug) any_thread: bool,
}

impl ParkedTask {
    /// Was this work queued for any parked thread (heap work), not for this
    /// one (an invocation, a frame write, a stack read)? Interpreter round
    /// i2 wave 66, lane L6.
    pub(crate) fn runs_on_any_thread(&self) -> bool {
        self.any_thread
    }
}

/// Where [`DebugState::heap_work_target`] sends heap work (wave 66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::debug) enum HeapWorkTarget {
    /// This parked thread, idle.
    Parked(u64),
    /// The heap service ([`start_heap_service`]).
    Service,
    /// No thread can take it.
    Nowhere,
}

/// The thread a [`ParkedTask`] runs on.
pub(in crate::debug) type ParkedThread = crate::threading::jvm_thread::JvmThread;

/// The work of a [`ParkedTask`], run on the parked thread.
pub(in crate::debug) type ParkedRun = Box<dyn FnOnce(&crate::vm::SharedVm, &mut ParkedThread) -> ParkedReply + Send>;

/// The answer of a [`ParkedTask`], held back until the parked thread has
/// settled its park (wave 9): its park level is idle again and, after a stop
/// inside an invocation, its frame snapshot is republished
/// (`interpreter::park_for_debugger`). Sent any earlier, the server could
/// reply to the debugger and hand the thread its next command while the
/// thread still looked busy (`ALREADY_INVOKING` / `THREAD_NOT_SUSPENDED`
/// for a thread about to be idle) or published no frames at all. Dropping it
/// unsent fails the waiting call (`ParkedError::Lost`).
pub(crate) struct ParkedReply(pub(in crate::debug) Box<dyn FnOnce() + Send>);

impl ParkedReply {
    /// Hand the answer to the waiting call.
    pub(crate) fn send(self) {
        (self.0)();
    }
}

/// Why [`run_on_parked_thread`] could not run its work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkedError {
    /// The thread named (or, for any thread, every thread) is not parked at
    /// an interpreter suspend point: running, blocked in native code or
    /// compiled code, or not a thread at all.
    NotParked,
    /// The thread named is parked but busy: it runs an invocation (and has
    /// not stopped inside it), or work is already queued for it (wave 9;
    /// JDWP `ALREADY_INVOKING` for an invocation).
    Busy,
    /// The thread left without answering (its park ended, or the work
    /// panicked).
    Lost,
}

/// Run `work` on thread `tid` — or, for `None`, on the lowest-numbered
/// parked thread that is idle — while it is parked at an interpreter suspend
/// point, and wait for its answer. The JDWP server's way to touch the heap
/// (interpreter round i1 wave 8); invocations go through
/// [`run_invocation_on_parked_thread`] (wave 9).
///
/// Why a parked thread: it is a registered mutator. The collector counts it,
/// waits for it at a safepoint and scans and remaps its frames, and no
/// collection can run while it executes Rust code between two polls. The
/// server thread is none of that: until wave 8 it ran debugger invocations
/// itself, on an unregistered `JvmThread` borrowing the suspended thread's
/// id, so a collection triggered by another thread moved the objects in the
/// ephemeral frames under the running Java
/// (docs/feature-designs/jvmti-delivery-threading.md).
///
/// Called with no debug-state lock held, by the thread running
/// [`run_jdwp_server`], an invocation's waiter thread, or a test standing in
/// for them; it blocks until the work is done. The work runs with the
/// suspend point suppressed and must not run Java.
pub(crate) fn run_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: Option<u64>,
    work: F,
) -> Result<R, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    queue_on_parked_thread(shared, tid, None, work)?
        .recv()
        .map_err(|_| ParkedError::Lost)
}

/// Run a debugger invocation's `work` on parked thread `tid` and wait for
/// its answer (wave 9). Around the work, on that thread and under the
/// debug-state lock, the JDWP invocation protocol applies
/// ([`DebugState::release_for_invocation`] /
/// [`DebugState::resuspend_after_invocation`]): without
/// `INVOKE_SINGLE_THREADED` every suspended thread runs while the method
/// does (until wave 9 none did, so a method that needed another suspended
/// thread's monitor or hand-off hung the session), and afterwards every
/// thread — or, single-threaded, `tid` alone — is suspended again. The
/// re-suspension happens before the answer is sent, so the thread never
/// sees itself resumed between the two and leaves its park.
///
/// The work runs with the suspend point live: a stop inside it reports its
/// event and parks the thread again (see [`debugger_hooks_suppressed`]). So
/// the caller must not be the JDWP server loop itself, which has to keep
/// serving commands until the answer comes (`run_jdwp_server` runs each
/// invocation on a waiter thread).
pub(crate) fn run_invocation_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: u64,
    single_threaded: bool,
    work: F,
) -> Result<R, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    queue_on_parked_thread(shared, Some(tid), Some(single_threaded), work)?
        .recv()
        .map_err(|_| ParkedError::Lost)
}

/// Queue `work` for a parked thread (see [`run_on_parked_thread`]);
/// `invocation` is `Some(single_threaded)` for a debugger invocation
/// ([`run_invocation_on_parked_thread`]). Answers the channel the work's
/// result arrives on.
pub(in crate::debug) fn queue_on_parked_thread<R, F>(
    shared: &crate::vm::SharedVm,
    tid: Option<u64>,
    invocation: Option<bool>,
    work: F,
) -> Result<std::sync::mpsc::Receiver<R>, ParkedError>
where
    R: Send + 'static,
    F: FnOnce(&crate::vm::SharedVm, &mut crate::threading::jvm_thread::JvmThread) -> R
        + Send
        + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel::<R>(1);
    let mut ds = shared.debug.debug_state.lock();
    let target = match tid {
        Some(t) if !ds.parked_threads.contains_key(&t) => return Err(ParkedError::NotParked),
        // Wave 44: an invocation runs only on a thread an event suspended
        // (`DebugState::event_parks`); the refusal is `INVALID_THREAD`, the
        // not-parked answer (`SharedVmBridge::not_parked`).
        Some(t) if invocation.is_some() && !ds.parked_by_event(t) => {
            return Err(ParkedError::NotParked)
        }
        // Running an invocation it has not stopped in, or work already
        // queued (a second invocation on the same thread: JDWP
        // `ALREADY_INVOKING`).
        Some(t) if !ds.can_take_work(t) => return Err(ParkedError::Busy),
        Some(t) => t,
        None => {
            // Wave 66 (lane L6, `CRATONVM_DEBUG_HEAP_WORK_OUTSIDE_REGION`):
            // a park outside its GC-blocked region first, then the heap
            // service, then a park inside one.
            let outside_region = shared.debug.debugger_gates.heap_work_outside_region();
            match ds.heap_work_target(outside_region, invocation.is_none()) {
                HeapWorkTarget::Parked(t) => t,
                // No thread can take it (wave 10): heap work (a `None` target
                // is never an invocation) goes to the heap service, a
                // registered mutator of its own, when the session has one.
                HeapWorkTarget::Service => {
                    let Some((service, _)) = ds.heap_service.as_ref() else {
                        return Err(ParkedError::NotParked);
                    };
                    let job: crate::native::jni::AttachedServiceJob = Box::new(
                        move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                            let _depth = HeapWorkDepth::enter();
                            let _ = tx.send(work(shared, thread));
                        },
                    );
                    return service
                        .send(job)
                        .map(|()| rx)
                        .map_err(|_| ParkedError::NotParked);
                }
                HeapWorkTarget::Nowhere => return Err(ParkedError::NotParked),
            }
        }
    };
    let reply = move |answer: R| {
        ParkedReply(Box::new(move || {
            let _ = tx.send(answer);
        }))
    };
    let run: ParkedRun = match invocation {
        None => Box::new(
            move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                reply(work(shared, thread))
            },
        ),
        Some(single_threaded) => {
            // Captured now, under the lock the task is queued under: a detach
            // after this point ends the session the invocation belongs to.
            let session = ds.session;
            Box::new(
                move |shared: &crate::vm::SharedVm, thread: &mut ParkedThread| {
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        if ds.session == session {
                            ds.release_for_invocation(target, single_threaded);
                            publish_debugger_gates(shared, &ds);
                        }
                    }
                    let answer = work(shared, thread);
                    {
                        let mut ds = shared.debug.debug_state.lock();
                        if ds.session == session {
                            ds.resuspend_after_invocation(target, single_threaded);
                            publish_debugger_gates(shared, &ds);
                        }
                    }
                    reply(answer)
                },
            )
        }
    };
    ds.parked_tasks.insert(
        target,
        ParkedTask {
            run,
            invocation: invocation.is_some(),
            any_thread: tid.is_none(),
        },
    );
    // Wave 63 (lane L6): a thread parked on its own word sees the task bit.
    ds.republish_suspend_word(target);
    // Wave 54 (lane L6): the parked thread takes it now, not at its next
    // poll ([`DebuggerGates::park_condvar`]).
    shared.debug.debugger_gates.notify_state_changed();
    Ok(rx)
}

/// Run a [`ParkedTask`] on the calling (parked) thread; heap work runs with
/// the suspend point suppressed for its duration
/// ([`debugger_hooks_suppressed`]), an invocation does not (wave 9).
/// `interpreter::park_for_debugger`'s poll loop calls this, marks its park
/// level idle ([`DebugState::note_task_done`]) and then sends the reply.
pub(crate) fn run_parked_task(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    task: ParkedTask,
) -> ParkedReply {
    if task.invocation {
        return (task.run)(shared, thread);
    }
    let _depth = HeapWorkDepth::enter();
    (task.run)(shared, thread)
}

/// Heap work in progress on the calling thread: the debugger's suspend point
/// stays suppressed while one lives ([`debugger_hooks_suppressed`]).
pub(in crate::debug) struct HeapWorkDepth;

impl HeapWorkDepth {
    pub(in crate::debug) fn enter() -> Self {
        DEBUGGER_WORK_DEPTH.with(|d| d.set(d.get() + 1));
        HeapWorkDepth
    }
}

impl Drop for HeapWorkDepth {
    fn drop(&mut self) {
        DEBUGGER_WORK_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Start the JDWP heap service for `shared` (interpreter round i1 wave 10):
/// a daemon thread attached to the VM (`jni::run_attached_service`) that runs
/// heap work when no thread is parked at a suspend point — `CreateString`
/// and `ArrayType.NewInstance`, which HotSpot serves with no thread
/// suspended, and the other heap commands, which then run on a registered
/// mutator rather than quiesced on the server thread. It never runs Java,
/// never parks, is not a thread the debugger lists (`populate_thread_metadata`)
/// and takes no invocation. Idempotent; `false` if the thread could not be
/// started. Started at attach by `run_jdwp_server`, stopped at detach
/// ([`stop_heap_service`]).
pub(crate) fn start_heap_service(shared: &Arc<crate::vm::SharedVm>) -> bool {
    if shared.debug.debug_state.lock().heap_service.is_some() {
        return true;
    }
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let owned = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("cratonvm-jdwp-heap".to_string())
        .spawn(move || {
            crate::native::jni::run_attached_service(
                owned,
                "cratonvm-jdwp-heap",
                started_tx,
                jobs_rx,
            )
        });
    if spawned.is_err() {
        return false;
    }
    let Ok(tid) = started_rx.recv() else {
        return false;
    };
    shared.debug.debug_state.lock().heap_service = Some((jobs_tx, tid));
    true
}

/// Stop the heap service: the thread detaches once the work queued before
/// this has run.
pub(crate) fn stop_heap_service(shared: &crate::vm::SharedVm) {
    let service = shared.debug.debug_state.lock().heap_service.take();
    drop(service);
}

/// Build JDWP location extra bytes: tag(1) + classID(8) + methodID(8) + index(8).
/// `class_id` is the VM's; it goes out in its wire form (wave 25,
/// `ids::class_to_wire`) — except in the null location `(0, 0, 0)` an
/// exception event writes for an uncaught exception, which stays all zeros
/// (a method id is never 0 for a real method: `jdwp_method_id`).
pub fn build_location_extra(class_id: u64, method_id: u64, offset: u64) -> Vec<u8> {
    let class_wire = if method_id == 0 {
        class_id
    } else {
        ids::class_to_wire(class_id)
    };
    let mut extra = Vec::with_capacity(25);
    extra.push(1u8); // TypeTag: CLASS
    extra.extend_from_slice(&class_wire.to_be_bytes());
    extra.extend_from_slice(&method_id.to_be_bytes());
    extra.extend_from_slice(&offset.to_be_bytes());
    extra
}

/// Send a thread event (ThreadStart or ThreadDeath) to the debugger if
/// there is an active event channel: one event per matching request
/// ([`events::EventManager::match_thread_events`]).
///
/// Wave 9: each request's suspend policy is applied before the event is
/// queued, as the interpreter's suspend point does for a breakpoint —
/// `SUSPEND_EVENT_THREAD` counts one suspension of the thread, `SUSPEND_ALL`
/// one of the VM — so a started thread parks at its first gated bytecode and
/// the debugger's resume of the event set balances a real suspension. The
/// policy used to be reported and not applied, and only the first matching
/// request reported. Wave 22: the matching requests' events are one event
/// set, suspended once with its strongest policy ([`send_event_set`]).
pub fn send_thread_event(shared: &crate::vm::SharedVm, kind: events::EventKind, thread_id: u64) {
    // Wave 25: a `PlatformThreadsOnly` modifier asks this (before the
    // debug-state lock: the table has a lock of its own).
    let is_virtual = shared.threads.virtual_thread_manager.is_virtual(thread_id);
    send_thread_event_of(shared, kind, thread_id, is_virtual);
}

/// [`send_thread_event`] for a thread whose caller knows whether it is a
/// virtual thread (interpreter round i1 wave 25: the VM's `Thread.start`
/// reads it from the `Thread` object's class, `BaseVirtualThread`, before the
/// virtual-thread table registers the thread — so `PlatformThreadsOnly`, which
/// asked that table, let a virtual thread's `ThreadStart` through,
/// `L1W25JdiVersion`). A virtual thread is remembered from its start for its
/// death and for `ThreadReference.IsVirtual`.
pub fn send_thread_event_of(
    shared: &crate::vm::SharedVm,
    kind: events::EventKind,
    thread_id: u64,
    is_virtual: bool,
) {
    let _ = send_thread_event_suspending(shared, kind, thread_id, is_virtual);
}

/// A platform thread's JDWP `ThreadDeath` ([`send_thread_event`]), answering
/// whether the event set's own suspend policy suspended something
/// (`SUSPEND_EVENT_THREAD`: the thread; `SUSPEND_ALL`: the VM). Interpreter
/// round i5 wave 106, lane L1 (hand-back): only then does the dying thread
/// hold ([`hold_ending_thread_while_suspended`]); a suspension it did not
/// cause -- another thread's, a debugger invocation's closing re-suspension,
/// a `VirtualMachine.Suspend` that a `ThreadReference.Stop`'s thread ran
/// into -- never holds it (`L1W25JdiStopMonitors` hung on that).
pub(crate) fn send_thread_death_event(shared: &crate::vm::SharedVm, thread_id: u64) -> bool {
    let is_virtual = shared.threads.virtual_thread_manager.is_virtual(thread_id);
    send_thread_event_suspending(shared, events::EventKind::ThreadDeath, thread_id, is_virtual)
}

/// [`send_thread_event_of`], answering whether the set's policy suspended.
pub(in crate::debug) fn send_thread_event_suspending(
    shared: &crate::vm::SharedVm,
    kind: events::EventKind,
    thread_id: u64,
    is_virtual: bool,
) -> bool {
    let (hits, suspended) = {
        let mut ds = shared.debug.debug_state.lock();
        let is_virtual = is_virtual || ds.virtual_threads.contains(&thread_id);
        // Wave 45 (lane L1): a thread that ends leaves no C JVMTI suspension
        // behind.
        let forgot =
            matches!(kind, events::EventKind::ThreadDeath) && ds.forget_jvmti_suspension(thread_id);
        // Interpreter round i7 wave 136 (lane L1): nor a C agent's frame
        // control it never took (its object's global reference is released).
        if matches!(kind, events::EventKind::ThreadDeath) {
            early_return::forget_thread(shared, &mut ds, thread_id);
        }
        match kind {
            events::EventKind::ThreadStart if is_virtual => {
                ds.virtual_threads.insert(thread_id);
            }
            events::EventKind::ThreadDeath => {
                ds.virtual_threads.remove(&thread_id);
            }
            _ => {}
        }
        let hits = ds
            .events
            .match_thread_events_of(kind, thread_id, is_virtual);
        // One event set, suspended once with its strongest policy (wave 22).
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, thread_id);
        // Wave 46 (lane L1): or a `Count` this match spent.
        let spent = take_spent_requests(&mut ds);
        if suspended || forgot || spent {
            publish_debugger_gates(shared, &ds);
        }
        (hits, suspended)
    };
    let set = hits
        .into_iter()
        .map(|(request_id, suspend_policy)| DebugEvent {
            kind,
            request_id,
            suspend_policy,
            thread_id,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    send_event_set(shared, set);
    suspended
}

/// `main` has ended its Java side on the launcher's thread, VM thread 0,
/// whose `Thread` is still `mirror` (interpreter round i5 wave 104, lane L1;
/// called by `vm_exec::end_main_thread_java_side` after `Thread.exit()`,
/// before the termination's `ensure_join`, where HotSpot's
/// `JavaThread::exit` posts `ThreadEnd`). With
/// `DebugState::destroy_java_vm_identity` and `destroy_java_vm` (the
/// launcher will list a `DestroyJavaVM` thread,
/// `VmConfig::destroy_java_vm_thread`): a C JVMTI agent's `ThreadEnd` for
/// `main`, the JDWP `ThreadDeath` of `main`'s id, and from then on the
/// `DestroyJavaVM` split ([`DebugState::destroy_java_vm_split`]): `main`'s
/// id names the ended `main` (`ids::ENDED_MAIN_VM_ID`, ZOMBIE) and VM
/// thread 0 goes out as `ids::DESTROY_JAVA_VM_WIRE_ID`.
///
/// The death's suspend policy is counted on the ended `main`, not on VM
/// thread 0: the debugger resumes the event's thread by `main`'s id, which
/// now names the ended `main`, so a suspension of thread 0 would never be
/// resumed. HotSpot holds `main` in its exit until then; here the launcher
/// goes on to list `DestroyJavaVM`.
pub(crate) fn note_main_thread_ended(
    shared: &crate::vm::SharedVm,
    name: &str,
    mirror: crate::types::ObjectRef,
    destroy_java_vm: bool,
) {
    if !destroy_java_vm || !shared.debug.debug_state.lock().destroy_java_vm_identity {
        return;
    }
    // The handle first: `mirror` is current only until this thread's next
    // safepoint, which the agent's callback below may reach.
    let handle = shared.natives.jni_global_refs.lock().add_weak(mirror);
    if shared.debug.debugger_gates.native_thread_events_armed() {
        crate::jvmti::native_env::post_thread_event(shared, 0, false);
    }
    let hits = {
        let mut ds = shared.debug.debug_state.lock();
        if ds.destroy_java_vm_split.is_some() {
            drop(ds);
            let _ = shared.natives.jni_global_refs.lock().remove(handle);
            return;
        }
        let hits = if shared.debug.debugger_gates.session_attached() {
            ds.events
                .match_thread_events_of(events::EventKind::ThreadDeath, 0, false)
        } else {
            Vec::new()
        };
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = match policy {
            events::SuspendPolicy::EventThread => {
                ds.suspend_thread(ids::ENDED_MAIN_VM_ID);
                true
            }
            events::SuspendPolicy::All => {
                ds.suspend_all();
                true
            }
            events::SuspendPolicy::None => false,
        };
        let spent = take_spent_requests(&mut ds);
        // Interpreter round i5 wave 105 (lane L1): a `ThreadOnly` filter or a
        // step naming `main` (VM thread 0, which the launcher goes on to run
        // as `DestroyJavaVM`) now names the ended `main`, so it matches none
        // of `DestroyJavaVM`'s events: HotSpot's filter holds `main`'s
        // thread object. After `main`'s own `ThreadDeath` matched above.
        let renamed = ds.events.rename_thread_filters(0, ids::ENDED_MAIN_VM_ID) > 0;
        ds.destroy_java_vm_split = Some(name.to_string());
        ds.ended_main_handle = handle;
        ds.thread_names
            .insert(ids::ENDED_MAIN_VM_ID, name.to_string());
        ds.thread_statuses
            .insert(ids::ENDED_MAIN_VM_ID, commands::THREAD_STATUS_ZOMBIE);
        shared
            .debug
            .debugger_gates
            .destroy_java_vm_split
            .store(true, std::sync::atomic::Ordering::Release);
        if suspended || spent || renamed {
            publish_debugger_gates(shared, &ds);
        }
        hits
    };
    let set = hits
        .into_iter()
        .map(|(request_id, suspend_policy)| DebugEvent {
            kind: events::EventKind::ThreadDeath,
            request_id,
            suspend_policy,
            // `thread_to_wire` sends it unchanged: `main`'s wire id.
            thread_id: ids::ENDED_MAIN_VM_ID,
            class_id: 0,
            method_id: 0,
            offset: 0,
            extra: Vec::new(),
            set_follows: false,
        })
        .collect();
    send_event_set(shared, set);
}

/// The launcher's thread, VM thread 0, now carries the `DestroyJavaVM`
/// `Thread` (interpreter round i5 wave 104, lane L1; called by
/// `vm_exec::list_destroy_java_vm_thread` once the mirror is registered):
/// HotSpot's `jni_DestroyJavaVM` attaches it, which posts JVMTI and JDWP
/// `ThreadStart`. Posts both when [`note_main_thread_ended`] started the
/// split; the JDWP event goes out under `ids::DESTROY_JAVA_VM_WIRE_ID`
/// ([`send_event_set`]).
pub(crate) fn note_destroy_java_vm_attached(shared: &crate::vm::SharedVm) {
    if !shared.debug.debugger_gates.destroy_java_vm_split() {
        return;
    }
    if shared.debug.debugger_gates.native_thread_events_armed() {
        crate::jvmti::native_env::post_thread_event(shared, 0, true);
    }
    if shared.debug.debugger_gates.session_attached() {
        send_thread_event_of(shared, events::EventKind::ThreadStart, 0, false);
    }
}

/// Report JDWP monitor event `kind` (43-46) about monitor `obj` on thread
/// `tid`, the calling thread (interpreter round i5 wave 103, lane L1;
/// `canRequestMonitorEvents`). Called from
/// `jvmti::native_env::post_monitor_event`, which the monitor paths call
/// where they post the C JVMTI events, while
/// [`DebuggerGates::jdwp_monitor_events_armed`]. `trailer` is the event's
/// data after its location: a `MonitorWait`'s timeout (`long`), a
/// `MonitorWaited`'s `timed_out` (`boolean`), nothing for the contended ones.
///
/// As HotSpot's back end (`eventHandler.c`, `cbMonitor*`): the location is
/// the thread's top frame (`GetFrameLocation(thread, 0)`; here its top
/// interpreter frame, the null location without one), whose class the
/// `ClassOnly` / `ClassMatch` / `ClassExclude` filters test; `InstanceOnly`
/// tests the monitor object. A suspending policy counts the suspension here;
/// the thread parks at its next bytecode (a contended entry gives the
/// monitor back while suspended, `vm_exec::block_enter_suspension_aware`),
/// as `class_prepared_on_thread` does.
pub(crate) fn post_jdwp_monitor_event(
    shared: &crate::vm::SharedVm,
    tid: u64,
    kind: events::EventKind,
    obj: crate::types::ObjectRef,
    trailer: &[u8],
) {
    use events::{EventLocation, EventSubject};
    if debugger_hooks_suppressed() {
        return;
    }
    let (filter_classes, instance_ids) = {
        let ds = shared.debug.debug_state.lock();
        if !ds.events.has_requests(kind) {
            return;
        }
        (
            ds.events.filter_class_ids(kind),
            ds.events.instance_filter_ids(kind),
        )
    };
    // The calling thread's top interpreter frame.
    let top: Option<(crate::classloading::ClassId, String, u64, u64, usize)> =
        crate::native::jni::current_jvm_thread_of(shared).and_then(|current| {
            // SAFETY: the calling thread's own, live `JvmThread`, read only
            // here, while the thread runs this call.
            let thread = unsafe { &*current };
            let depth = thread.frames.len();
            thread.frames.last().map(|frame| {
                (
                    frame.class_id,
                    frame.class_name().to_string(),
                    frame_method_id(frame),
                    frame.last_instr_pc as u64, // Widening: bytecode index
                    depth,
                )
            })
        });
    // What needs the class manager, before the debug-state lock.
    let (location_supers, object_tag) = {
        let cm = shared.classes.class_manager.read();
        (
            top.as_ref().map_or_else(Vec::new, |(class, ..)| {
                supertypes_among(&cm, *class, &filter_classes)
            }),
            inspect::object_tag(shared, &cm, obj),
        )
    };
    let (class_id, class_name, method_id, offset, frame_depth) = match &top {
        Some((class, name, method, offset, depth)) => (
            u64::from(class.as_u32()),
            name.as_str(),
            *method,
            *offset,
            *depth,
        ),
        None => (0, "", 0, 0, 0),
    };
    let location = EventLocation {
        class_id,
        class_name,
        method_id,
        offset,
        thread_id: tid,
        frame_depth,
        line_start: None,
        line: None,
    };
    let set = {
        let mut ds = shared.debug.debug_state.lock();
        let instance_hits: Vec<u64> = instance_ids
            .into_iter()
            .filter(|&id| object_for_id(shared, &ds, id) == Some(obj))
            .collect();
        let subject = EventSubject {
            location_class_is: &|id| location_supers.contains(&id),
            field: None,
            instance_is: &|id| instance_hits.contains(&id),
            exception_is: &|_| false,
            caught: false,
        };
        let hits = ds.events.match_subject_events(kind, &location, &subject);
        let mut set = Vec::with_capacity(hits.len());
        for &(request_id, suspend_policy) in &hits {
            // The tagged monitor object, then the trailer; the drain puts the
            // location between them. Each event exports its own reference.
            let mut extra = protocol::PayloadWriter::new();
            inspect::put_tagged_with_tag(
                shared,
                &mut ds,
                &mut extra,
                b'L',
                crate::types::Value::Object(Some(obj)),
                Some(object_tag),
            );
            extra.put_bytes(trailer);
            set.push(DebugEvent {
                kind,
                request_id,
                suspend_policy,
                thread_id: tid,
                class_id,
                method_id,
                offset,
                extra: extra.into_bytes(),
                set_follows: false,
            });
        }
        // Interpreter round i7 wave 139, lane L1 (`CRATONVM_JDWP_EVENT_OBJECT_HOLD`,
        // the "not covered" part of i127-L1): the monitor lives until the
        // debugger's first look at it, as an `Exception` event's object does.
        if !set.is_empty() {
            hold_event_object(shared, &mut ds, obj);
        }
        let policy = strongest_suspend_policy(hits.iter().map(|&(_, p)| p));
        let suspended = ds.apply_event_set_policy(policy, tid);
        let spent = take_spent_requests(&mut ds);
        if suspended || spent {
            publish_debugger_gates(shared, &ds);
        }
        set
    };
    send_event_set(shared, set);
}

/// The JDWP `methodID` this server hands out for the method `name` +
/// `descriptor`: a 64-bit FNV-1a hash of `name`, a `0` separator byte (which
/// neither a method name nor a descriptor can contain) and `descriptor`. One
/// definition for the id the debugger learns (`populate_class_metadata`), the
/// id the interpreter matches a breakpoint location against, and the ids in a
/// suspended thread's frame snapshot.
///
/// Until wave 4 only the name was hashed, so every overload of a method
/// shared one id: a breakpoint set in `f(I)V` also fired at the same bytecode
/// index of `f(J)V`, and `ClassType.InvokeMethod` resolved the id to whichever
/// overload the method table listed first.
///
/// Since interpreter round i1 wave 42 the hash lives with the JVMTI native
/// rows (`jvmti::native_env::native_row_method_hash`, compiled in every
/// build), which key a running native by the same id.
#[inline]
pub(crate) fn jdwp_method_id(name: &str, descriptor: &str) -> u64 {
    crate::jvmti::native_env::native_row_method_hash(name, descriptor)
}

/// The JDWP `methodID` of a location in an obsolete method: HotSpot's back
/// end writes a null method id for a method JVMTI `IsMethodObsolete` answers
/// true for (`writeCodeLocation`), which JDI turns into an
/// `ObsoleteMethodImpl`, and `Method.IsObsolete` answers true for it.
pub(crate) const OBSOLETE_METHOD_ID: u64 = 0;

/// The JDWP `methodID` of the method `frame` runs: [`OBSOLETE_METHOD_ID`]
/// for a frame moved onto a body its class's redefinition replaced
/// (`Frame::runs_obsolete_method`), else [`jdwp_method_id`] (interpreter
/// round i1 wave 19, lane L3).
pub(crate) fn frame_method_id(frame: &crate::runtime::frame::Frame) -> u64 {
    if frame.runs_obsolete_method() {
        OBSOLETE_METHOD_ID
    } else {
        jdwp_method_id(frame.method_name(), frame.method_descriptor())
    }
}

/// The JDWP `fieldID` of field `index` of class `class_id`'s own `fields`
/// table: the class id in the high 32 bits, the index in the low 32
/// (wave 8). [`field_of_id`] is the inverse.
pub(crate) fn jdwp_field_id(class_id: crate::classloading::ClassId, index: usize) -> u64 {
    // Cast: a class's field count is bounded by the u16 `fields_count`.
    (u64::from(class_id.as_u32()) << 32) | (index as u64 & 0xFFFF_FFFF)
}

/// The `(declaring class, index into its fields table)` a JDWP `fieldID`
/// names ([`jdwp_field_id`]).
pub(crate) fn field_of_id(field_id: u64) -> (crate::classloading::ClassId, usize) {
    // Cast: the high half is a u32 class id; the low half fits usize.
    (
        crate::classloading::ClassId::new((field_id >> 32) as u32),
        (field_id & 0xFFFF_FFFF) as usize,
    )
}

