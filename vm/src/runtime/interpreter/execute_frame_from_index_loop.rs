#[allow(unused_imports)]
use super::*;

/// The dispatch loop behind [`execute_frame_from_index`]; `LEAN_TOP` selects
/// the wave-116 loop top.
pub(in crate::runtime::interpreter) fn execute_frame_from_index_loop<const LEAN_TOP: bool>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    initial_frame_idx: usize,
) -> MethodCallResult {
    let mut frame_idx = thread.frames.len() - 1;
    // AUDIT CRIT-3 fix: hoist the PGO-enabled atomic load ONCE per
    // execute_frame invocation.  Branch sites in the interpreter hot loop
    // (~13 of them) test this local instead of doing an atomic load + 2
    // `Arc<str>` clones (for make_method_key) every iteration.  Profiling
    // state observed at frame entry — the next `execute_frame` invocation
    // re-reads the global gate, so newly-enabled profiling picks up on the
    // next call rather than mid-loop.
    // CORRECTION (2026-09-07): "the next call" understates the window, for
    // every gate hoisted here. `execute_frame_from_index` runs an ENTIRE
    // NESTED CALL TREE in one invocation — an interpreted call pushes a frame
    // and `continue`s this same loop — so these locals are not observed per
    // METHOD, they are observed per OUTERMOST interpreter entry. A gate armed
    // while a deep call tree is running is not seen until that whole tree
    // unwinds.
    //
    // That is defensible for a profiler and for a diagnostic trace, which is
    // what every gate hoisted here is. It would NOT be defensible for a
    // debugger's field watchpoints or for `any_class_redefined`, which is
    // exactly why `field_fast`'s per-access checks are NOT hoisted alongside
    // these — see the retired `heap-touching-bytecodes-are-the-outlier`
    // write-up, which asked whether they could be and answered no.
    let pgo_enabled = crate::jit::profile::is_profiling_enabled();
    // Round nio20 wave 2, lane L3: the invoke arms' per-execution site
    // counters ride the branch profile's gate (`record_call_for_frame`).
    let call_counts_on = crate::runtime::env_cache::fast_call_counts();
    // T17.Δ.3 — hoist the JVMTI single-step listener gate out of the per-bytecode
    // loop (same contract as `pgo_enabled` above). `any_single_step_listener_active()`
    // is a `GLOBAL_MANAGER` OnceLock load + an AtomicBool load; reading it on EVERY
    // bytecode was pure overhead in the universal no-agent case. Per-thread step
    // enablement is still re-checked per-bytecode inside `fire_jvmti_single_step`
    // when a listener is active.
    //
    // Wave 6: re-read with the debugger gate (`refresh_debugger_gate!`, at every
    // frame switch and backward branch). Hoisted only per ENTRY, a listener that
    // subscribed while a call tree ran was not seen until that whole tree
    // unwound (see the CORRECTION above) — for `main`'s tree, never — so an
    // agent enabling SingleStep mid-run got no events from the running loop,
    // and every superinstruction kept fusing past the pcs it wanted to see.
    // Since wave 82 (lane L7) the live value is the `TG_SINGLE_STEP` bit of
    // `top_gates` (below); this is its entry value.
    let single_step_at_entry = crate::runtime::jvmti::any_single_step_listener_active();
    // Debugger breakpoints (wave 3, lane L1). `breakpoints_armed` is the
    // per-VM JDWP gate (`breakpoints_armed_now`), read here and again at every
    // frame switch (the `fast_gate_code_ptr` branch below) and backward branch
    // (`refresh_debugger_gate!`), never per bytecode; `false` as a constant in
    // a build without the JDWP surface.
    // While it is set the loop top tests every pc against the breakpoint
    // requests. `fusion_off` is the one test every superinstruction makes: a
    // fused group runs 2-4 bytecodes from its FIRST pc, so both a
    // single-step listener and an armed breakpoint (which may sit on an
    // interior pc of the group) must see the bytecodes one at a time.
    //
    // Wave 7: per METHOD. `breakpoints_armed` is the VM-wide gate AND "the
    // requests in force concern the executing method" (`debugger_gate_for_frame`:
    // a breakpoint concerns its own method, a step or a suspension every
    // method). It was the VM-wide gate alone, so one breakpoint anywhere sent
    // every bytecode of every method on every thread through the suspend
    // point — two debug-state lock round trips each — and turned every fusion
    // off. `debug_gates_seen` / `debug_frame_concerned` cache the per-method
    // answer against the summary's generation; a frame switch invalidates
    // them. Entry starts from the VM-wide gate; the first iteration is a frame
    // switch and narrows it.
    let mut breakpoints_armed = breakpoints_armed_now(shared);
    let mut debug_gates_seen: u64 = u64::MAX;
    let mut debug_frame_concerned = false;
    let mut fusion_off = single_step_at_entry || breakpoints_armed;
    // The opcode-pair census (`CRATONVM_QUICKEN_STATS=pairs`, Stage 0 of the
    // dispatch-loop proposal; `cratonvm_reader::quickened`), and the one bit
    // the loop top tests for both of its cold hooks (`TG_TOP_HOOKS` of
    // `top_gates`, below; a bool `loop_top_hooks` until wave 82), so arming
    // the census adds no test to the unarmed loop. `census_prev` is the last
    // fast-path dispatch (see `note_opcode_pair`).
    let pair_census = cratonvm_reader::quickened::pair_census_enabled();
    // Interpreter round i2 wave 62, lane L1: a C agent's `SingleStep`
    // callback just asked for a `PopFrame` or a forced return of the frame it
    // stepped (`DebuggerGates::c_callback_frame_control`); the suspend point
    // at this pc takes it. Written and read only on the single-step path.
    let mut step_frame_control = false;
    // Boxed on the census's first dispatch (wave 84, lane L7, frame
    // ratchet): one pointer in the loop's frame, not the 32-byte cursor a
    // diagnostic kept there in every run (`note_opcode_pair_in`).
    let mut census_prev: Option<Box<OpcodePairCursor>> = None;
    // Hoisted for the same reason as `pgo_enabled` above: the `if_acmpne`
    // fast-path arm declines while the `CRATONVM_ACTIVE_PROFILES_IDENTITY_TRACE`
    // diagnostic is armed, so the decoded arm (which owns that instrument)
    // still runs. Reading the cached gate once per `execute_frame` keeps the
    // arm's admission test to a register compare. Arming it mid-method is
    // observed on the next call/return, the accepted pgo-style tradeoff.
    let acmp_identity_trace = crate::runtime::env_cache::active_profiles_identity_trace();
    // Two diagnostics the value-return arm consults, hoisted for the same
    // reason: that arm runs once per interpreted call, and each gate was a
    // memo-slot load per return (the invoke-phase instrument's `on()` was read
    // up to seven times per return). Armed mid-run they are observed on the
    // next `execute_frame` entry, the accepted pgo-style tradeoff.
    let trace_sb_filter = crate::runtime::env_cache::trace_sb_filter();
    let invoke_phases_on = invoke_phases::on();
    // The return arms' plain-return gates (`pop_and_recycle_frame_gated`),
    // hoisted for the same reason (interpreter round i2 wave 61, lane L7):
    // the frame trace and the invoke-phase instrument, and the loop-work
    // credit a returning frame that looped would owe.
    let return_diagnostics_on = crate::runtime::env_cache::frame_trace() || invoke_phases_on;
    let loop_work_on = loop_work_tierup_enabled();
    // The quickened `getfield` arms' field-phase instrument
    // (`CRATONVM_DBG_FIELD_PHASES`; `field_fast::getfield_fast`), hoisted
    // likewise (wave 61, lane L7): a static byte load per `getfield` before.
    let field_phases_on = field_phases::on();
    // The per-method branch-profile arm's re-read (`branch_profile_on`
    // below; interpreter round i2 wave 62, lane L7,
    // `CRATONVM_INTERP_BRANCH_ARM_GATE`). The flag's one writer,
    // `jit_bridge::arm_method_branch_profile`, runs only where a tier was
    // recommended with the C2 branch window on and, under the gate, a JIT
    // (`offer_invocation_to_tiered_manager`); it is never cleared. So with
    // the gate on a loop re-reads it only while an arm is possible
    // (`branch_arm_possible`) and its cached answer is still `false`: under
    // `--nojit` the back edge loses the three dependent loads
    // (`frame_branch_profile_armed`), with the JIT it loses them once the
    // method is armed. With the gate off, `branch_reread_always` keeps the
    // re-read at every backward branch and frame switch.
    let branch_arm_gate = shared.config.interp_branch_arm_gate;
    // Interpreter round i5 wave 113 (`CRATONVM_VTHREAD_SKIP_BRANCH_ARM`,
    // `VmConfig::vthread_skip_branch_arm`, default on, `=0` off): never on a JDK
    // virtual thread, which keeps running a refused method interpreted after
    // its body publishes and would record into the arm's shared counters for
    // good (page `i91-L6-virtual-thread-code-is-never-compiled`, "Wave 113").
    let branch_arm_possible = !(matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && thread.jdk_continuation.jdk())
        && (!branch_arm_gate
            || (shared.config.jit_enabled && crate::jit::profile::c2_branch_window_enabled()));
    let branch_reread_always = !branch_arm_gate;
    // The observer gates, hoisted (interpreter round i2 wave 62, lane L7,
    // `CRATONVM_INTERP_HOISTED_OBSERVERS`, `VmConfig::interp_hoisted_observers`).
    // With it on, the backward branches no longer re-read the debugger gate
    // (`refresh_debugger_gate!`) and the quickened `getfield` / `putfield`
    // arms test `field_watch_hoist` instead of loading the field-watch union
    // (`jvmti::any_field_watchpoint_active`). Both are re-read in the loop
    // top's slow path instead, which every writer of those gates sends every
    // loop of this VM to after it publishes (`jvmti::bump_observer_loops`,
    // `debug::publish_debugger_gates`): an observer armed while a loop runs
    // -- from another thread, or from an agent callback on this one -- is
    // seen before the loop's next bytecode. `None` (the switch off) makes
    // each arm load the union itself, as before.
    let observers_hoisted = shared.config.interp_hoisted_observers;
    let mut field_watch_hoist: Option<bool> =
        observers_hoisted.then(crate::runtime::jvmti::any_field_watchpoint_active);
    // Whether this thread can owe a continuation return conversion at all
    // (`JvmThread::continuation_return_adapters`, wave 25, lane L7): an
    // adapter is recorded only when a `ContinuationYield` crosses a lambda
    // door, and every yield producer (`vt_sleep_for`, `vt_park_for`,
    // `vt_wait_on_key` and their callers, the remount's own yield) requires
    // `thread.kind == Virtual`, which is fixed before the thread runs any
    // Java. So a platform thread's list is always empty, and its value-return
    // arms test this bool -- hot in a register or the loop's own stack line
    // -- instead of loading the list's length from the `JvmThread`, a line
    // the return path touches for nothing else (interpreter round i1 wave
    // 26, lane L7).
    let thread_is_virtual = matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    // ── Back-edge poll word (2026-09-02) ─────────────────────────────────
    //
    // Every backward branch used to call `safepoint_check` UNCONDITIONALLY and
    // then `continue` — straight into the loop-top poll thirty lines below,
    // which asks `stw_requested` first and only calls `safepoint_check` if it
    // is set. So the back-edge call was redundant with the very next thing the
    // loop does, *except* for its tail: the async-exception drain, which is
    // the only work at a back edge that the loop top does not repeat.
    //
    // That tail was not cheap. `take_async_exception` goes through
    // `self_async_slot`: a thread-local `RefCell` borrow, an `Arc::clone`, a
    // `swap(0, AcqRel)` and an `Arc` drop — three locked read-modify-writes
    // per loop iteration, inside a function too large to inline (it carries
    // the memwatch and blocked-access debug hooks). Measured with
    // `probes/BackEdge.java` (two loops with identical total body-bytecode
    // counts and an 8x difference in back-edge count, so the per-back-edge
    // cost falls out of the difference): one interpreted backward branch cost
    // 33-37 ns against HotSpot's template interpreter at 4.9 ns.
    //
    // Hoisting the slot handle turned the back-edge question into two relaxed
    // loads and a predicted branch.
    //
    // WAVE 2 (2026-09-23): and now into ONE. The async-exception term is gone
    // from the back-edge test, because the channel it polled cannot fire:
    // nothing posts to it (`ThreadRegistry::post_async_exception` has no
    // caller — JDK 20+ `Thread.stop` throws `UnsupportedOperationException`,
    // JDK 17's `stop0` keeps its own table, and there is no JVMTI `StopThread`
    // or JDWP `ThreadReference.Stop`), and nothing raises what it would drain
    // (`check_pending_async_exception` has no caller). Every loop iteration was
    // paying a load for a feature that did not exist. WAVE 4: the whole
    // channel (registry slot, `JvmThread` field, root-scan sites, the
    // `NativeContext` post) is deleted; see
    // docs/feature-designs/profile-guided-inlining.md.
    // Whoever implements JVMTI `StopThread` / JDWP `ThreadReference.Stop`
    // designs delivery afresh (the page says what it must satisfy, including
    // that `goto +0` has to count as a back edge).
    //
    // The STW flag byte, hoisted for the same reason as the gates above.
    //
    // `GcBarrier::stw_requested` stopped being an inline field on 2026-09-10:
    // it is now a `&'static AtomicBool` into a cell allocated from the JIT's
    // own page allocator, so that compiled code can poll it with a
    // RIP-relative `TEST BYTE [rip+disp32]` instead of materializing a 64-bit
    // address (`CacheLineFlag`). Spelled inline, the two per-bytecode reads
    // below would each become a dependent PAIR of loads — fetch the pointer
    // out of `GcBarrier`, then the byte — and the pointer word shares a line
    // with `threads_blocked` and the barrier mutex, i.e. with
    // exactly the neighbours whose writes `CacheLineFlag` exists to stay away
    // from. Resolving it once per `execute_frame` leaves each read a single
    // load from a line no one ever writes but the collector.
    let stw_flag = shared.mem.gc_barrier.stw_requested.flag();
    // The condition every back edge now tests before paying for
    // `safepoint_check`. Written as a macro rather than a closure because the
    // call sites sit inside `&mut thread` borrows and a closure capturing
    // `stw_flag` would still have to be called in a position where `thread`
    // is reborrowed.
    //
    // `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE=1` (or `CRATONVM_JIT=-backedge-poll-
    // gate`) makes the macro answer `true` unconditionally, restoring the
    // unconditional call so the two arms can be priced inside one binary.
    //
    // ── The two diagnostics that must keep their sampling rate ──────────
    //
    // Skipping `safepoint_check` is equivalent to calling it only when the
    // call would have been a no-op, and there are exactly two ways it is not:
    // `memwatch::poll` and `blocked_access_debug`, both of which fire from
    // inside it and neither of which the loop-top poll reaches. A memwatch is
    // a *sampling* instrument — its own doc promises it "catches the
    // corrupting write within one safepoint window" — so silently cutting its
    // rate would weaken a diagnostic rather than speed anything up, and would
    // do it invisibly.
    //
    // Both are startup-static gates, so folding them in costs one more `or`
    // in the hoisted `bool` and nothing per back edge. An armed run keeps
    // exactly the old poll frequency; the universal unarmed run kept one flag
    // load per back edge until wave 26, and now none (see the macro below).
    let backedge_poll_gate_off = crate::runtime::env_cache::no_backedge_poll_gate()
        || crate::runtime::memwatch::is_watching()
        || cratonvm_gc::blocked_access_debug::enabled();
    // ── Quickened field / array arms (2026-09-02) ───────────────────────
    // `Some(heap)` iff this frame may take the `getfield` / `putfield` and
    // primitive `*aload` / `*astore` fast arms; see `field_fast` for the
    // contract and for what turns them off. Hoisted per `execute_frame`
    // entry like every other gate above (same pgo-style tradeoff).
    let fast_field_zgc = field_fast::fast_field_zgc(shared);
    // The fused `aload{_N, n}; arraylength` shapes (the pair, and the
    // second operand of `fused_icmp_arm!`) read the array's length straight
    // out of the local (`field_fast::arraylength_of_local`). Their admission
    // is the unfused `arraylength` arm's — `fast_field_zgc`, plus the
    // `CRATONVM_JIT_NO_ARRAYLENGTH_FAST` switch — hoisted here so a fused
    // group tests one bool for it; since wave 60 (lane L7) the unfused arm
    // tests this bool too, instead of `arraylength_fast` re-reading the switch.
    let arraylength_fusion_on =
        fast_field_zgc.is_some() && !crate::runtime::env_cache::no_arraylength_fast();
    // The site-cache census gate (`CRATONVM_DBG_FIELD_SITE`,
    // `VmConfig::site_census`), passed to every quickened field / array arm's
    // hit count so the unarmed per-bytecode test is a register, not a
    // `OnceLock` read (interpreter round i2 wave 60, lane L7). Per-VM and
    // fixed for the VM's life, so hoisting it changes nothing it observes.
    let site_census = shared.config.site_census;
    // The fused compare-and-branch's `ldc` int bound
    // (`CRATONVM_INTERP_FUSED_LDC_BOUND`, `VmConfig::interp_fused_ldc_bound`;
    // interpreter round i2 wave 63, lane L7): admitted only with the
    // per-thread numeric-constant table's own gate (`ldc_const_cache`), since
    // the fusion answers from that table alone. Per-VM and fixed for the VM's
    // life, hoisted like `arraylength_fusion_on`.
    let fused_ldc_bound_on = shared.config.ldc_const_cache;
    // The `ldc` arm's pc store through the hoisted frame
    // (`CRATONVM_INTERP_LDC_HOISTED_PC`, `VmConfig::interp_ldc_hoisted_pc`;
    // wave 63, lane L7).
    let ldc_hoisted_pc = shared.config.interp_ldc_hoisted_pc;
    // The `ldc` / `ldc_w` arm's inline answer for a warm int / float
    // (`CRATONVM_INTERP_LDC_INLINE_HIT`, `VmConfig::interp_ldc_inline_hit`;
    // interpreter round i2 wave 64, lane L7): admitted with the per-thread
    // numeric-constant table's own gate, as `fused_ldc_bound_on` is. Per-VM
    // and fixed for the VM's life.
    let ldc_inline_hit_on = shared.config.ldc_const_cache;
    // The fused compare-and-branch's quickened `getstatic` int bound
    // (`CRATONVM_INTERP_FUSED_GETSTATIC_BOUND`,
    // `VmConfig::interp_fused_getstatic_bound`; wave 64, lane L7). The
    // quickened `getstatic`'s own kill switch (`no_field_fast_path`, a
    // process memo `getstatic_site_hit` re-reads per execution) is folded in
    // here once; the field-watch union is NOT (it can be armed while the
    // loop runs): the fused arm tests `field_watch_hoist` when the observers
    // are hoisted and loads the union itself otherwise, so a `FieldAccess`
    // watch armed mid-loop is seen exactly as the unfused `getstatic` sees it.
    let fused_getstatic_bound_on = !crate::runtime::env_cache::no_field_fast_path();
    // The `ldc2_w` arm's inline answer for a warm long / double
    // (`CRATONVM_INTERP_LDC2W_INLINE_HIT`, `VmConfig::interp_ldc2w_inline_hit`;
    // interpreter round i2 wave 65, lane L7): admitted with the per-thread
    // numeric-constant table's own gate, as `ldc_inline_hit_on` is. Per-VM
    // and fixed for the VM's life.
    let ldc2w_inline_hit_on =
        shared.config.ldc_const_cache;
    // The `getstatic` arm's inline answer for a quickened site
    // (`CRATONVM_INTERP_GETSTATIC_INLINE_HIT`,
    // `VmConfig::interp_getstatic_inline_hit`; wave 65, lane L7). The
    // quickened `getstatic`'s kill switch (`no_field_fast_path`, a process
    // memo) is folded in once, as for `fused_getstatic_bound_on`; the
    // field-watch union is not (it can be armed while the loop runs): the
    // arm tests `field_watch_hoist` when the observers are hoisted and loads
    // the union itself otherwise. The relocation-provenance trace
    // (`CRATONVM_DBG_REMAP_TRACE`, latched once per process) records every
    // reference `getstatic` pushes, so while it is armed the arm declines a
    // reference field and the call records it.
    let getstatic_inline_on = !crate::runtime::env_cache::no_field_fast_path();
    let getstatic_inline_remap_trace = getstatic_inline_on && gc_and_alloc::remap_trace_on();
    // The `putstatic` arm's inline store to a quickened site
    // (`CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`,
    // `VmConfig::interp_putstatic_inline_hit`; wave 66, lane L7). The kill
    // switch is folded in once, as for `getstatic_inline_on`; the field-watch
    // union (which a JVMTI or JDWP `FieldModification` watch arms) is not: the
    // arm tests `field_watch_hoist` when the observers are hoisted and loads
    // the union itself otherwise, so a watch armed mid-loop declines the next
    // store and `op_putstatic` posts it.
    let putstatic_inline_on = !crate::runtime::env_cache::no_field_fast_path();
    // The fused `getstatic; if<cond>` against zero for a quickened int-typed
    // static (`CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`,
    // `VmConfig::interp_fused_getstatic_ifzero`; wave 66, lane L7). Admitted
    // with `fused_getstatic_bound_on`'s rule: the kill switch folded in once,
    // the field-watch union tested per execution (`field_watch_hoist`), and
    // the arm runs only while `fusion_off` is clear (no single-step, no
    // breakpoint concerning this method), as every fusion does.
    let fused_getstatic_ifzero_on = !crate::runtime::env_cache::no_field_fast_path();
    // The fused static int increment, `getstatic #F; <int operand>; iadd |
    // isub; putstatic #F` (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT`,
    // `VmConfig::interp_fused_static_increment`; wave 67, lane L7). Admitted
    // only with BOTH inline hits it replaces (each already carries the kill
    // switch), so the fused group never answers a site the two arms would
    // send to their calls; the field-watch union (a `FieldAccess` or
    // `FieldModification` watch) is tested per execution and the arm runs
    // only while `fusion_off` is clear, as every fusion does.
    let fused_static_increment_on =
        getstatic_inline_on && putstatic_inline_on;
    // The fused increments' `ldc2_w` long operand
    // (`CRATONVM_INTERP_FUSED_INCREMENT_LDC2W`,
    // `VmConfig::interp_fused_increment_ldc2w`; interpreter round i3 wave 83,
    // lane L7). Admitted only with the `ldc2_w` inline hit (which carries the
    // numeric-constant table's own gate), so a fused group never reads a
    // constant the unfused `ldc2_w` arm would not; each side's own switch and
    // wide switch are tested where that side builds its mode.
    let fused_increment_ldc2w_on =
        ldc2w_inline_hit_on;
    // The `aload n; getfield` fusion (`CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD`,
    // `VmConfig::interp_fused_wide_local_getfield`; interpreter round i3 wave
    // 83, lane L7), admitted with the quickened field arms' own admission
    // (`fast_field_zgc`) so the `aload n` arm tests one bit.
    let fused_wide_local_getfield_on =
        fast_field_zgc.is_some();
    // The `lload_N` arm's long array accumulate
    // (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`,
    // `VmConfig::interp_fused_array_accumulate_long`; wave 82, lane L7; one
    // gate bit since the wave-83 frame rework), with the quickened field
    // arms' own ZGC admission the helper needs.
    let fused_array_accumulate_long_on =
        fast_field_zgc.is_some();
    // Interpreter round i3 wave 77 (lane L7, frame ratchet; a pure refactor):
    // the ten gates above travel through the loop as ONE hoisted word, not
    // ten loop-carried bools (each of which the register allocator gave its
    // own spill byte in this function's frame). Every arm still tests one
    // value per gate (a `test` of the word's bit instead of a byte compare),
    // and the `#[inline(always)]` readers that take a gate as an argument
    // (`fused_int_operand`, `getstatic_inline_hit`) fold the bit test into
    // their own branch exactly as they folded the bool. The ten locals are
    // dead past this line.
    let static_ldc_gates: u16 = gate_word!(u16;
        (fused_ldc_bound_on, SLG_FUSED_LDC_BOUND),
        (ldc_hoisted_pc, SLG_LDC_HOISTED_PC),
        (ldc_inline_hit_on, SLG_LDC_INLINE_HIT),
        (fused_getstatic_bound_on, SLG_FUSED_GETSTATIC_BOUND),
        (ldc2w_inline_hit_on, SLG_LDC2W_INLINE_HIT),
        (getstatic_inline_on, SLG_GETSTATIC_INLINE),
        (getstatic_inline_remap_trace, SLG_GETSTATIC_REMAP_TRACE),
        (putstatic_inline_on, SLG_PUTSTATIC_INLINE),
        (fused_getstatic_ifzero_on, SLG_FUSED_GETSTATIC_IFZERO),
        (fused_static_increment_on, SLG_FUSED_STATIC_INCREMENT),
        (fused_increment_ldc2w_on, SLG_FUSED_INCREMENT_LDC2W),
        (fused_wide_local_getfield_on, SLG_FUSED_WIDE_LOCAL_GETFIELD),
        (fused_array_accumulate_long_on, SLG_FUSED_ARRAY_ACCUMULATE_LONG),
        (true, SLG_FUSED_LOAD_PAIR),
        (shared.config.interp_fused_iconst_goto, SLG_FUSED_ICONST_GOTO),
        (true, SLG_FUSED_ICONST_ARITH),
    );
    // The fused instance-field int increment, `aload_N; dup; getfield #F;
    // <int operand>; iadd | isub; putfield #F`
    // (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT`,
    // `VmConfig::interp_fused_field_increment`; wave 68, lane L7). The
    // quickened field arms' admission (`fast_field_zgc`: the kill switch, the
    // ZGC heap, every per-access diagnostic) is tested at the arm, as the
    // `aload_N; getfield` fusion tests it; the field-watch union and the
    // field-phase instrument are the helper's, per execution. Since wave 69
    // (lane L7) one byte packs the switch with the per-entry gates the
    // helper reads (the census, the field-phase instrument) and the two
    // wave-69 switches (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE`,
    // `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`): `0` is the switch
    // off, and the call passes one argument for the three it passed before.
    // Wave 70 (lane L7) adds one more bit to the same byte rather than a
    // hoisted bool: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP`
    // (`field_fast::FIELD_INC_NO_DUP`), the `aload_N; aload_N` prefix. Wave
    // 71 (lane L7, round i3) one more:
    // `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`
    // (`field_fast::FIELD_INC_VALUE`), the `dup_x1` / `dup2_x1` value forms
    // (the `dup` prefix the arm already offers; no change to the arm).
    let fused_field_increment_mode = field_fast::field_increment_mode(
        true,
        site_census,
        field_phases_on,
        true,
        shared.config.interp_fused_increment_early_decline,
        true,
        true,
    );
    // Wave 72 (lane L7, round i3): the last free bit of the same byte,
    // `CRATONVM_INTERP_FUSED_FIELD_UPDATE_TWO_FIELDS`
    // (`field_fast::FIELD_INC_TWO_FIELDS`, `this.a = this.b + k`), set only
    // on top of `FIELD_INC_NO_DUP` (the prefix it shares).
    let fused_field_increment_mode = field_fast::with_two_fields(
        fused_field_increment_mode,
        true,
    );
    // The quickened `aaload` / `aastore` arms' kill switch
    // (`CRATONVM_JIT_NO_REF_ARRAY_FAST`), hoisted like `arraylength_fusion_on`
    // (interpreter round i2 wave 60, lane L7): `array_load_ref` and
    // `array_store_ref` re-read its memo on every execution before.
    let ref_array_fast_on = !crate::runtime::env_cache::no_ref_array_fast();
    // ── Stack-dump hook admission (2026-09-02) ──────────────────────────
    // The dump hook at the top of the loop is a load of an atomic that
    // NOTHING can set unless a watchdog or sampler was armed, and both are
    // armed before Java starts running. A run with neither — every run that
    // is not being debugged — skips the hook entirely on this hoisted bool.
    // See `SharedVm::arm_stack_dump_watch` for why arming happens at spawn
    // time rather than at fire time.
    let stack_dump_possible = shared.stack_dump_watch_armed();
    // ── Invoke fast door admission (2026-09-02) ─────────────────────────
    // Off while anything the general dispatcher would have to observe per
    // call is armed: PGO (it records call sites and receivers), the invoke
    // traces, the frame trace, or a virtual thread. See
    // `execute_invokevirtual_fast_door`.
    // Default-ON again as of 2026-09-03: the wrong answers this door produced
    // were the receiver profile it was not recording, and it records it now.
    // `CRATONVM_JIT_INVOKE_FAST_DOOR=1` remains accepted (it forced the door on
    // while the default was off) and is now a no-op.
    let fast_doors_observable = !crate::runtime::env_cache::no_invoke_fast_door()
        && !pgo_enabled
        && !crate::runtime::env_cache::frame_trace()
        && !crate::runtime::env_cache::dbg_h2trace()
        && !crate::runtime::env_cache::dbg_loader_trace()
        && !crate::runtime::env_cache::dbg_gse()
        && !crate::runtime::env_cache::dbg_pbstart();
    let platform_fast_doors_on = fast_doors_observable
        && !matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    // A mounted JDK virtual thread, whose doors may be admitted one by one
    // under their virtual-thread switches (waves 114-115, lane L6).
    let jdk_vthread_doors_observable = fast_doors_observable
        && matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        && thread.jdk_continuation.jdk();
    // Interpreter round i6 wave 115, lane L6 (`CRATONVM_VTHREAD_VIRTUAL_DOOR`,
    // default off): the virtual / interface door on a JDK virtual thread too;
    // it then judges its callee as the general `VirtualBytecode` arm does
    // there (`invoke_fast::vthread_door_gate`). The lambda door behind it
    // declines on a virtual thread (`invoke_fast::execute_lambda_door`).
    let invoke_fast_door_on = platform_fast_doors_on
        || (jdk_vthread_doors_observable);
    // The same admission for the two non-virtual doors (`invokestatic`,
    // `invokespecial`); they share every reason the virtual one stays off
    // and add their own kill switch. See `interpreter::invoke_fast`.
    // Interpreter round i5 wave 114, lane L6
    // (`CRATONVM_VTHREAD_NONVIRTUAL_DOORS`, default off): on a JDK virtual
    // thread too; the two doors then judge a compiled callee as the general
    // arms do there (`invoke_fast::vthread_door_gate`). The virtual door and
    // the signature-polymorphic site cache stay off on a virtual thread.
    // Wave 115 (lane L6): from `platform_fast_doors_on`, not
    // `invoke_fast_door_on`, so `CRATONVM_VTHREAD_VIRTUAL_DOOR` alone does
    // not admit these two doors on a virtual thread.
    let nonvirtual_fast_door_on = (platform_fast_doors_on
        || (jdk_vthread_doors_observable && shared.config.vthread_nonvirtual_doors))
        && !crate::runtime::env_cache::no_nonvirtual_fast_door();
    // The signature-polymorphic site route cache (`sig_poly_site`; wave 48,
    // lane L7): admitted with the fast door, whose reasons to stay off (PGO,
    // the invoke traces, a virtual thread) are all reasons to leave a call on
    // the path that observes it. Never on a virtual thread (wave 115: from
    // `platform_fast_doors_on`, whatever `CRATONVM_VTHREAD_VIRTUAL_DOOR` says).
    // Interpreter round i6 wave 122, lane L4 (`CRATONVM_VTHREAD_SIG_POLY_SITES`,
    // default off; i114-L6 item 5): on a mounted JDK virtual thread too. A hit
    // replays the native the slow path ran (same `safe_native_call*`, no
    // virtual-thread arm on the slow path to skip), and its frame-push arms
    // push interpreted frames only, never a compiled entry.
    let sig_poly_site_on = platform_fast_doors_on
        || (jdk_vthread_doors_observable && shared.config.vthread_sig_poly_sites);
    // ... probed ahead of the fast door when the thread's site filter says
    // the site may be cached (wave 54, lane L7, `sig_poly_site::try_hit_first`).
    let sig_poly_site_first_on = sig_poly_site_on;
    // ── The diagnostic, door and loop-top gate words (wave 82) ──────────
    //
    // Interpreter round i3 wave 82 (lane L7, frame ratchet; the wave-77
    // `static_ldc_gates` packing carried on,
    // `i77-L7-proposal-pack-the-dispatch-loops-diagnostic-gates-into-one-word`):
    // the gates above travel through the loop as three words, not as fifteen
    // loop-carried bools (each of which the register allocator gave its own
    // spill byte in this function's frame). Each bit is exactly the bool of
    // the same name it replaces, and every arm still tests one value per
    // gate; the locals are dead past this point.
    //
    // * `diag_gates`: the off-by-default diagnostics and the per-entry facts
    //   tested only on cold or per-call paths. `DG_FIELD_PHASES` is bit 0,
    //   so the two `getfield` arms that pass it by value pass `word & 1`.
    // * `door_gates`: the invoke-door admissions, read at the invoke arms.
    // * `top_gates` (mutable): the three gates the loop top tests per
    //   bytecode -- the stack-dump hook, the single-step hook and the cold
    //   hooks (breakpoints, pair census) -- in one byte, so a bytecode reads
    //   one gate byte where it read three. `TG_SINGLE_STEP` and
    //   `TG_TOP_HOOKS` are the former `single_step_active` and
    //   `loop_top_hooks`, rewritten where those were.
    //
    // `CRATONVM_INTERP_STACK_DUMP_AT_POLL` (wave 82, lane L7,
    // `VmConfig::interp_stack_dump_at_poll`, default off) selects one
    // behaviour: the stack-dump hook leaves the per-bytecode path for the
    // loop top's slow path, which `SharedVm::request_stack_dump` /
    // `request_stack_sample` send this VM's loops to, so a run with a
    // watchdog or sampler armed no longer pays the hook's two dependent
    // loads (`stack_dump_pending`, `stack_sample_mode`) per bytecode:
    // `TG_STACK_DUMP` stays clear and `DG_STACK_DUMP_AT_POLL` is set instead.
    let stack_dump_at_poll = stack_dump_possible && shared.config.interp_stack_dump_at_poll;
    let diag_gates: u16 = gate_word!(u16;
        (field_phases_on, DG_FIELD_PHASES),
        (acmp_identity_trace, DG_ACMP_IDENTITY_TRACE),
        (trace_sb_filter, DG_TRACE_SB_FILTER),
        (pair_census, DG_PAIR_CENSUS),
        (invoke_phases_on, DG_INVOKE_PHASES),
        (return_diagnostics_on, DG_RETURN_DIAGNOSTICS),
        (backedge_poll_gate_off, DG_BACKEDGE_POLL_GATE_OFF),
        (loop_work_on, DG_LOOP_WORK),
        (thread_is_virtual, DG_THREAD_IS_VIRTUAL),
        (shared.config.interp_hoisted_observers, DG_OBSERVERS_HOISTED),
        (
            shared.config.interp_hoisted_observers || true,
            DG_OBSERVER_REREAD,
        ),
        (stack_dump_at_poll, DG_STACK_DUMP_AT_POLL),
        (
            true,
            DG_METHOD_ENTRY_FRAME_CONTROL,
        ),
        (
            finalizer_register_at_object_init_enabled(&shared.config)
                && shared.config.finalizer_reregister_at_init_return,
            DG_FINALIZER_INIT_RETURN,
        ),
    );
    // Interpreter round i6 wave 115 (lane L8): a `u16` since this wave, for
    // the two branch fusions' bits (`DOOR_FUSED_ZERO_BRANCH`,
    // `DOOR_FUSED_NULL_BRANCH`); the byte's eight bits were all taken. The
    // same register, the same one-bit tests.
    let door_gates: u16 = gate_word!(u16;
        (invoke_fast_door_on, DOOR_INVOKE_FAST),
        (nonvirtual_fast_door_on, DOOR_NONVIRTUAL_FAST),
        (sig_poly_site_on, DOOR_SIG_POLY_SITE),
        (sig_poly_site_first_on, DOOR_SIG_POLY_SITE_FIRST),
        (true, DOOR_FUSED_LONG_ARITH),
        (true, DOOR_FUSED_DOUBLE_ARITH),
        (
            true,
            DOOR_FUSED_LONG_INT_OPERAND,
        ),
        (true, DOOR_FUSED_I2L_LONG_TAIL),
        (true, DOOR_FUSED_ZERO_BRANCH),
        (true, DOOR_FUSED_NULL_BRANCH),
        (true, DOOR_FUSED_GETFIELD_CHAIN),
        (shared.config.interp_fused_null_branch_wide, DOOR_FUSED_NULL_BRANCH_WIDE),
        (true, DOOR_FAST_SWITCH),
        (shared.config.interp_fused_loop_step_test, DOOR_FUSED_LOOP_STEP_TEST),
        (shared.config.interp_fused_store_chain, DOOR_FUSED_STORE_CHAIN),
        (shared.config.interp_fused_store_chain_wide, DOOR_FUSED_STORE_CHAIN_WIDE),
    );
    let mut top_gates: u8 = gate_word!(u8;
        (single_step_at_entry, TG_SINGLE_STEP),
        (breakpoints_armed || pair_census, TG_TOP_HOOKS),
        (stack_dump_possible && !stack_dump_at_poll, TG_STACK_DUMP),
        // The lean loop top (wave 116, lane L8) is built on the one-test form:
        // every writer of the two slots sets `TG_PENDING`.
        (
            !shared.config.interp_one_top_signal_test && !LEAN_TOP,
            TG_SLOT_SIGNALS,
        ),
    );
    // ── OSR call floor (2026-09-02) ─────────────────────────────────────
    // `try_osr_with_backoff` cannot do anything until `Frame::backward_count`
    // reaches the smallest threshold `Frame::should_try_osr` accepts (the
    // attempt backoff only raises it), so the four back-edge sites compare
    // the count against this floor inline and make the out-of-line call only
    // past it. A virtual thread, `CRATONVM_JIT_OSR=0` or `--nojit`
    // (`CRATONVM_DISABLE_JIT`, since wave 16) makes the call a guaranteed
    // no-op, so the floor is `u32::MAX`; the arrival trace
    // (`CRATONVM_DBG_OSR_FRAME_TRACE`) records every arrival inside the
    // call, so it keeps the floor at 0 and the old call rate.
    // `CRATONVM_JIT_NO_OSR_INLINE_GATE=1` restores the unconditional call.
    // Past the floor, each frame's own `Frame::osr_poll_at` spaces the calls
    // out (see `try_osr_with_backoff`).
    let osr_call_floor: u32 =
        if crate::runtime::env_cache::no_osr_inline_gate() || osr_frame_trace::enabled() {
            0
        } else if (matches!(thread.kind, crate::threading::ThreadKind::Virtual)
            // Interpreter round i5 wave 100, lane L6
            // (`CRATONVM_VTHREAD_COMPILED_LEAF_OSR`, default off): a JDK
            // virtual thread keeps the ordinary floor, and `try_osr_offer`
            // admits only a frame whose method cannot reach a yield
            // (`jdk_continuation::vthread_may_osr_method`).
            // Wave 101 (lane L2): or `CRATONVM_VTHREAD_COMPILED_LEARNED`.
            && !crate::vm::jdk_continuation::vthread_osr_door_on(shared, thread))
            || !crate::runtime::env_cache::osr_backedge_enabled()
            || crate::runtime::env_cache::disable_jit()
        {
            u32::MAX
        } else {
            let threshold = crate::runtime::env_cache::tier_osr_backedge().unwrap_or(OSR_THRESHOLD);
            // MEGAOSR19-3 (`CRATONVM_JIT_OSR_EARLY_REENTRY`, opt-in): the
            // early re-entry window starts below the threshold
            // (`osr_early_reentry_due`).
            if crate::runtime::env_cache::osr_early_reentry() {
                threshold.min(OSR_EARLY_REENTRY_BACKEDGES)
            } else {
                threshold
            }
        };
    // Wave 26 (lane L7): the flag load is gone from the back edge. Every
    // back-edge site `continue`s straight after this test, into the loop top,
    // whose poll word (`loop_poll`) every stop-the-world request raises -- so
    // the loop top calls `safepoint_check` for a pause the back edge would
    // have answered, one iteration step later with nothing in between but the
    // (empty) pending-signal test and the stack-dump hook. The back edge only
    // made the call twice per iteration while a pause was up. With the gate
    // off (the kill switch, or one of the two sampling diagnostics above) the
    // call stays unconditional, as before.
    macro_rules! backedge_poll_needed {
        () => {
            diag_gates & DG_BACKEDGE_POLL_GATE_OFF != 0
        };
    }
    // When a fast-path bytecode needs to throw a RuntimeError (AIOOBE, NPE, etc.),
    // it sets this to Some(...) and breaks out of the fast-path match instead of
    // returning directly. The main loop then converts it to a catchable Java exception.
    // Boxed since wave 84 (lane L7, frame ratchet): see `PendingRuntimeError`.
    let mut pending_runtime_error: PendingRuntimeError = None;
    // When an invoke handler receives an ExceptionThrown error (e.g. from JIT
    // dispatch), it stores the exception here instead of returning directly.
    // The main loop then routes it through the exception table for proper
    // try/catch handling.  The usize is the PC of the invoke instruction.
    let mut pending_java_exception: Option<(ObjectRef, usize)> = None;
    // The two slots' ONLY writers (interpreter round i4 wave 84, lane L7,
    // `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST`): each parks its signal and sets
    // `TG_PENDING`, in both modes, so the loop top's one gate test sees it.
    // A bare `pending_* = Some(..)` anywhere else would be a throw the
    // switched-on loop never routes; `grep -n 'pending_java_exception = '`
    // must find only the macro below.
    macro_rules! park_java_exception {
        ($exc:expr, $pc:expr $(,)?) => {{
            pending_java_exception = Some(($exc, $pc));
            top_gates |= TG_PENDING;
        }};
    }
    macro_rules! park_runtime {
        ($re:expr, $pc:expr $(,)?) => {{
            pending_runtime_error = park_runtime_error($re, $pc);
            top_gates |= TG_PENDING;
        }};
    }
    // T19.H1 — per-invocation guard so an `execute()` frame dumps its
    // stack at most once when the watchdog flag is sticky-true.
    let mut stack_dump_emitted = false;
    // --- Quickening (bytecode pre-decode) state, valid for the frame whose
    // bytecode allocation lives at `quick_code_ptr`.
    //
    // `quick` is an owned `Arc` deliberately: the dispatch below borrows an
    // `&Instruction` out of it and hands that borrow to `execute_instruction`,
    // which takes `&mut thread`. Keeping the stream in a loop-local (rather
    // than reading it back out of `thread.frames[..]` every iteration) is what
    // makes those two borrows disjoint.
    //
    // The pointer key is sound because a live `QuickenedCode` holds a strong
    // `Arc<[u8]>` to the very bytecode it was built from, so this address
    // cannot be freed and recycled by a different method while `quick` is
    // `Some`. When `quick` is `None` a recycled address can only mislabel
    // another method as un-quickened -- a pessimisation, never a miscompare.
    // ARCH-2026-08-04 A4b — hoisted out of the dispatch loop.
    //
    // `VmConfig` is immutable after `SharedVm::new`: `with_skip_verification` /
    // `with_xverify_mode` are `mut self -> Self` builders that run before
    // construction, so this is loop-invariant for the whole `execute()` call
    // (indeed for the process). It was re-read once per bytecode as a
    // `shared -> config -> bool` pointer chase.
    //
    // What it selects, and why it is a whole-mode switch rather than a
    // per-opcode one: the fast-path local-access handlers (`lload`/`dload`,
    // `istore`/`fstore`, `astore`, `lstore`/`dstore`, and the
    // `get_local_compact_unchecked` / `set_local_compact_unchecked` helpers the
    // `iload`/`iadd` fusions use) index `frame.locals` with the raw bytecode
    // operand, relying on the verifier having proven `operand < max_locals`.
    // Under `-noverify` / `-Xverify:none` that proof is gone. Note the indexing
    // is ordinary Rust `Vec` indexing, so the failure is a *panic*, not memory
    // unsafety — but a panic is still not an acceptable answer to a valid
    // command line, hence the fallback to the bounds-checked decoded path.
    //
    // That fallback is what makes `--noverify` select a different
    // implementation for all 122 fast-path arms at once, which is why
    // `difftest`'s `interp-decoded` axis has to run (it did not until A4b; see
    // `difftest/src/main.rs`).
    let use_fast_path = !shared.config.skip_verification;
    // Per-METHOD half of the same admission (wave 2, lane L1): the verifier's
    // `safe_for_fast_path` proof, which `use_fast_path` alone never consulted.
    // Recomputed only when the executing bytecode allocation changes — a call,
    // return, unwind or OSR transfer — never per bytecode. See
    // `frame_fast_path_admitted` for what it admits and why.
    let mut fast_gate_code_ptr: *const u8 = std::ptr::null();
    // The unpadded code length of `fast_gate_code_ptr`'s allocation, cached
    // by the refresh for the lean loop top (`LEAN_TOP`, wave 116, lane L8);
    // read only while the pointer still matches, never before the first
    // refresh (the pointer starts null).
    let mut lean_code_len: usize = 0;
    // This OS thread's dispatch-loop poll word (`gc_barrier::LoopPollWord`,
    // interpreter round i1 wave 26, lane L7): the low byte counts this VM's
    // stop-the-world pause while it is up, the rest is a trigger every frame
    // move on this OS thread bumps. Registered with this VM's barrier here
    // when it is not yet (one compare otherwise), so every pause is counted
    // in it.
    //
    // SAFETY: `loop_poll_word_for_this_thread` returns this OS thread's
    // thread-local word, which lives until the thread's thread-local
    // destructors run -- they cannot start while this loop runs on the thread
    // -- or, when it is already destroyed, a word owned by this VM's barrier,
    // which `shared` keeps alive for the whole call.
    let loop_poll: &LoopPollWord =
        unsafe { &*shared.mem.gc_barrier.loop_poll_word_for_this_thread() };
    // gcj j2/cm2 (`CRATONVM_GC_HANDSHAKE_TARGET_KICK`, opt-in;
    // `gcj-j1cm-proposal-target-only-handshake-kick`): publish the word this
    // loop runs on, so a frame-trace handshake requester kicks only this
    // thread's word. One load of a per-VM bool with the switch off; one
    // compare per loop entry while the word is unchanged.
    // t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, opt-in): the handshake arm
    // is raised on the same publication.
    if shared.threads.thread_registry.handshake_target_kick()
        || shared.threads.thread_registry.handshake_poll_bit()
    {
        thread
            .gc_block_state
            .publish_loop_poll_word(loop_poll as *const LoopPollWord);
    }
    // The poll word, pause count masked off, as the loop last settled it: the
    // loop top compares the WHOLE word against it -- one load and one compare
    // per bytecode for both a stop-the-world pause and a frame move (wave 26;
    // until then the `stw_requested` byte and `FrameStack::code_moves` were
    // loaded and compared separately). Starts one move behind, so the first
    // top takes the slow path.
    let mut poll_gate: u32 =
        (loop_poll.load() & !LoopPollWord::PAUSES).wrapping_sub(LoopPollWord::MOVE_STEP);
    // `FrameStack::code_moves` when the gate was last refreshed, compared by
    // the slow path (`loop_top_poll`) whenever the word sends the loop there:
    // a frame moved onto a translated copy of a body a JVMTI redefinition
    // replaced can land at the ADDRESS of a callee's private copy freed at its
    // pop, which the pointer compare alone would take for the same code
    // (interpreter round i1 wave 23, lane L3). Since wave 24 it is also what
    // frees a moved frame's never-loaded copies safely
    // (`ReplacedBody::fresh_code`: the first top to load a moved copy must
    // refresh, or that copy could be freed while this loop runs it): never
    // drop it. It is compared at the top, before the frame is read, so a
    // conversion that was deferred (the class-manager lock was busy) runs
    // before any bytecode of a stale frame does; it starts one behind, so a
    // loop entered with a conversion pending sees it at its first top.
    let mut fast_gate_code_moves: u32 = thread.frames.code_moves().wrapping_sub(1);
    // A move bumped `code_moves` since the gate's last refresh (set by the
    // slow path at the top, consumed by the refresh below).
    let mut gate_saw_move = false;
    let mut fast_gate_open = false;
    let mut quick: Option<Arc<cratonvm_reader::QuickenedCode>> = None;
    let mut quick_code_ptr: *const u8 = std::ptr::null();
    // (The former `quick_hint` local is gone: `QuickenedCode` now resolves any
    // pc in O(1) via an instruction-start bitmap plus per-block popcount, so
    // the fall-through hint only bought one popcount on the fall-through path
    // at the cost of a compare on every branch, back-edge, handler entry and
    // switch target. `resolve()` is the hint-free form.)
    // ── Conditional-branch arm, written once ──────────────────────────────
    //
    // Each of the interpreter's conditional-branch fast-path arms is the same
    // twenty lines around a one-line predicate: record the branch outcome for
    // PGO, take the branch or fall through, and — when the target is BACKWARD
    // — record the back edge, bump `Frame::backward_count`, and offer the
    // frame to `try_osr_with_backoff`.
    //
    // It is written once here because the copied form had already lost a whole
    // opcode family. `ifnull` (0xc6), `ifnonnull` (0xc7), `if_acmpeq` (0xa5)
    // and `if_acmpne` (0xa6) had no fast-path arm at all, so they fell through
    // to the decoded handler in `opcodes.rs` — which sets `frame.pc` and
    // nothing else. A loop closed by one of those four therefore:
    //
    //   * never recorded a PGO back edge,
    //   * never incremented `Frame::backward_count`, so it could not reach the
    //     `OSR_THRESHOLD` and could never enter an OSR-compiled body, and
    //   * never earned whole-method tier-up credit either, because
    //     `pop_and_recycle_frame_with_reason` feeds `ProfileStore::add_loop_work`
    //     from that same counter.
    //
    // That is the ordinary shape of `do { … } while (p != null)`, of
    // `do { … } while (o != sentinel)`, and of every `for`/`while` emitted by a
    // frontend that puts the loop test at the BOTTOM (ECJ, and the Kotlin and
    // Scala backends) rather than at the top with a closing `goto` the way
    // javac does. The comment on `try_osr_with_backoff` calls itself "the one
    // funnel all fourteen back-edge sites go through" — fourteen was the count
    // of arms that had been written, not of branches that can close a loop.
    //
    // `frame`, `saved_pc`, `b1` and `b2` are parameters rather than captures:
    // `macro_rules!` gives local-variable identifiers definition-site hygiene
    // and those four are bound inside the dispatch loop, below this point.
    // Everything else the body touches (`shared`, `thread`, `frame_idx`,
    // `initial_frame_idx`, `pgo_enabled`, `branch_profile_on`,
    // `pending_java_exception`) is already in scope here.
    //
    // `branch_profile_on` is the executing frame's per-method branch-profile
    // arm (`frame_branch_profile_armed`; set at the method's first nomination
    // while still interpreted, see `jit_bridge::arm_method_branch_profile`).
    // It used to be read per conditional branch — a `FrameInner` match, an
    // `Arc` deref and an atomic load for a bit that flips at most once per
    // method. It is now re-read at every frame switch (the preamble's
    // `fast_gate_code_ptr` branch) and at every BACKWARD branch (here,
    // `goto_arm!` and the decoded path's back-edge accounting), so a method
    // nominated inside a running call tree still starts recording within one
    // loop iteration. What a nomination between two back edges costs is the
    // forward branches of that one iteration.
    let mut branch_profile_on = false;
    // Re-read the debugger gate (`breakpoints_armed`, and `fusion_off` with
    // it). Run at frame switches and at every backward-or-self branch (wave
    // 4): a breakpoint set, or a `VirtualMachine.Suspend` issued, while a
    // call-free loop is running is then seen within one iteration instead of
    // at the loop's next call or return. One relaxed load per back edge in a
    // build with the JDWP surface; constant `false` without it.
    //
    // Wave 5: the three locals are rewritten only when the gate CHANGED.
    // `fusion_off` and `loop_top_hooks` are pure functions of
    // `breakpoints_armed` and two entry-time constants, so an unchanged gate
    // leaves them unchanged too; the unconditional form stored all three on
    // every back edge of every loop (the dispatch loop's locals live in stack
    // slots), where this is one load, one compare and a predicted branch.
    // Note that a workspace-wide `cargo build` enables `experimental-debug`
    // on `cratonvm-vm` for every binary (`libcratonvm` asks for it and Cargo
    // unifies features), so the load is live in such a `cratonvm` build.
    //
    // Wave 6: the JVMTI single-step union flag rides the same refresh (one
    // more load of a static flag), so a SingleStep listener armed while a
    // loop runs is delivered from that loop's next iteration and its fusions
    // stop, instead of after the outermost interpreter entry returns.
    //
    // Wave 7: `$frame` is the executing frame; the gate is armed only when the
    // requests in force concern its method (`debugger_gate_for_frame`, cached
    // against the summary's generation, so an unchanged summary costs one more
    // load while the VM-wide gate is set, and nothing more when it is not).
    //
    // The three locals' update, shared by the refresh below and the hoisted
    // form (`observers_hoisted`, wave 62): rewritten only when the gate
    // changed (wave 5).
    macro_rules! apply_debugger_gate {
        ($armed_now:expr, $step_now:expr) => {{
            let armed_now: bool = $armed_now;
            let step_now: bool = $step_now;
            if armed_now != breakpoints_armed || step_now != (top_gates & TG_SINGLE_STEP != 0) {
                breakpoints_armed = armed_now;
                fusion_off = step_now || armed_now;
                // `single_step_active` and `loop_top_hooks` (wave 82: bits).
                top_gates = with_gate_bit(top_gates, TG_SINGLE_STEP, step_now);
                top_gates = with_gate_bit(
                    top_gates,
                    TG_TOP_HOOKS,
                    armed_now || diag_gates & DG_PAIR_CENSUS != 0,
                );
            }
        }};
    }
    macro_rules! refresh_debugger_gate {
        ($frame:expr) => {{
            let armed_now = breakpoints_armed_now(shared)
                && debugger_gate_for_frame(
                    shared,
                    $frame,
                    &mut debug_gates_seen,
                    &mut debug_frame_concerned,
                );
            let step_now = crate::runtime::jvmti::any_single_step_listener_active();
            apply_debugger_gate!(armed_now, step_now);
        }};
    }
    // Hoisted (`observers_hoisted`): the VM-wide JDWP gate as the loop top's
    // slow path last read it. A frame switch then narrows it to the new
    // method (`debugger_gate_for_frame`) without loading the gate or the
    // single-step union again; both are re-read only at the slow path, which
    // their writers send the loop to.
    let mut debug_vm_gate = breakpoints_armed;
    // ── `frame_idx` is never borrowed in the lean instance (wave 120) ────
    //
    // Interpreter round i6 wave 120, lane L8 (`CRATONVM_INTERP_LEAN_LOOP_TOP`,
    // the `LEAN_TOP` instance; `i1-L1-proposal-dispatch-loop-register-state`):
    // the two helpers that move the loop's frame index take it as `&mut
    // usize`. A borrow anywhere in the function gives the local a stack slot
    // for the whole function (wave 27's finding), so every dispatch reloaded
    // `frame_idx` from memory at the head of its critical chain (`frame_idx`
    // -> `frame_ptr` -> `pc` -> opcode byte -> jump). In the `LEAN_TOP`
    // instance these call sites hand the helper a copy and write the answer
    // back, so the local's address never escapes and LLVM may keep it in a
    // register. The `false` instance keeps the direct borrow, exactly as
    // before. The helpers' behaviour is unchanged either way: they read the
    // index at the call, write it once, and nothing else observes the slot
    // in between.
    macro_rules! osr_backoff_at {
        ($entry_pc:expr) => {{
            let entry_pc: usize = $entry_pc;
            if LEAN_TOP {
                let mut idx = frame_idx;
                let outcome =
                    try_osr_with_backoff(shared, thread, &mut idx, initial_frame_idx, entry_pc);
                frame_idx = idx;
                outcome
            } else {
                try_osr_with_backoff(shared, thread, &mut frame_idx, initial_frame_idx, entry_pc)
            }
        }};
    }
    macro_rules! unwind_here {
        ($exc:expr, $pc:expr) => {{
            let (exc, pc): (ObjectRef, usize) = ($exc, $pc);
            if LEAN_TOP {
                let mut idx = frame_idx;
                let result =
                    unwind_to_handler(shared, thread, &mut idx, initial_frame_idx, exc, pc);
                frame_idx = idx;
                result
            } else {
                unwind_to_handler(shared, thread, &mut frame_idx, initial_frame_idx, exc, pc)
            }
        }};
    }
    // ── Fast-path error routing, written once (wave 5) ───────────────────
    //
    // Every raw-bytecode arm that calls out of the loop (the four invoke
    // opcodes through each of their doors, `getfield`/`putfield`, the
    // constant-pool arm, `monitorenter`/`monitorexit`, `newarray`, `athrow`,
    // `invokedynamic`, `ldc*`) ends the same way: classify the error
    // (`classify_fastpath_invoke_error`, which performs the Runtime / Linkage
    // conversions and settles a thrown exception), park it in the matching
    // `pending_*` slot at the instruction's OWN pc, and `continue`, so the
    // loop top routes it before the safepoint poll. That tail was 22 hand
    // copies, and they had drifted: only the `athrow` and `invokedynamic`
    // copies settled the throwable (see
    // docs/feature-designs/real-jdk-mode.md).
    //
    // `Fatal` returns WITHOUT unwinding this invocation's frames, as every
    // copy did: it carries `VmError::ContinuationYield`, whose leaf frame must
    // survive (see the decoded path's yield arm), and the callers' orphan
    // sweeps (`pop_root_frames_keeping_result`) pop whatever else is left.
    // `$saved_pc` is a parameter for the hygiene reason `cond_branch_arm!`
    // gives.
    macro_rules! route_fast_path_error {
        ($err:expr, $saved_pc:expr) => {{
            match classify_fastpath_invoke_error(shared, thread, $err) {
                FastPathInvokeError::Runtime(re) => {
                    park_runtime!(re, $saved_pc);
                }
                FastPathInvokeError::Java(exc) => {
                    park_java_exception!(exc, $saved_pc);
                }
                FastPathInvokeError::Fatal(fatal) => return Err(fatal),
            }
            continue;
        }};
    }
    macro_rules! cond_branch_arm {
        ($frame:expr, $saved_pc:expr, $b1:expr, $b2:expr, $taken:expr) => {{
            let taken = $taken;
            // The process-wide gate, or this method's own arm (see
            // `branch_profile_on` above).
            if pgo_enabled || branch_profile_on {
                record_branch_for_frame(shared, $frame, $saved_pc, taken);
            }
            if taken {
                // Cast: bytecode operand decoding
                let offset = (($b1 as i16) << 8) | ($b2 as i16);
                // Cast: signed branch offset arithmetic
                $frame.pc = ($saved_pc as isize + offset as isize) as usize;
                // Hoisted, the loop top's slow path refreshes it
                // (`observers_hoisted`).
                if offset <= 0 && diag_gates & DG_OBSERVERS_HOISTED == 0 {
                    refresh_debugger_gate!(&*$frame);
                }
                // A forward exit may leave a loop through its top test; the
                // loop's trip average needs it (`record_loop_exit_for_frame`).
                if offset > 0 && pgo_enabled {
                    record_loop_exit_for_frame(shared, &*$frame, $saved_pc);
                }
                if offset < 0 {
                    // See `branch_arm_possible`.
                    if branch_arm_possible && (branch_reread_always || !branch_profile_on) {
                        branch_profile_on = frame_branch_profile_armed($frame);
                    }
                    // Under the loop's key; not an obsolete body's
                    // (`record_backedge_for_frame`).
                    if pgo_enabled {
                        record_backedge_for_frame(shared, &*$frame, $saved_pc);
                    }
                    // `wrapping_add`: a `u32` bumped once per back edge, which
                    // a long-running loop in one frame can overflow — a panic
                    // under debug overflow checks. Release builds already
                    // wrapped; this keeps that answer and drops the panic.
                    $frame.backward_count = $frame.backward_count.wrapping_add(1);

                    let entry_pc = $frame.pc;
                    // The frame's poll point is read only past the floor.
                    let bc_now = $frame.backward_count;
                    let osr_due = bc_now >= osr_call_floor && bc_now >= $frame.osr_poll_at;
                    let _ = $frame;
                    if osr_due {
                        match osr_backoff_at!(entry_pc) {
                            OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                            OsrBackoffOutcome::ContinueDispatch => continue,
                            OsrBackoffOutcome::ThrowJava(exc) => {
                                park_java_exception!(exc, OSR_FRAME_DECLINED_TO_CATCH);
                                continue;
                            }
                            OsrBackoffOutcome::Skip => {}
                        }
                    }
                    if backedge_poll_needed!() {
                        safepoint_check(shared, thread);
                    }
                }
            } else {
                $frame.pc = $saved_pc + 3;
            }
            continue;
        }};
    }
    // ── Unconditional `goto`, written once ────────────────────────────────
    //
    // The `goto` arm and the `iinc; goto` fusion (javac's loop step) share
    // it. Same back-edge accounting as `cond_branch_arm!` minus the branch
    // profile, which `goto` never recorded: a taken/not-taken record at a
    // pc that cannot fall through would only pollute the JIT's branch data.
    // `$saved_pc` is the `goto`'s OWN pc, for the offset base and the
    // back-edge record alike.
    //
    // Interpreter round i6 wave 118, lane L8: an optional `; { tail }` runs
    // after a BACKWARD goto's accounting (the branch profile, the back-edge
    // record, the OSR offer when it skipped, the poll), right before the
    // `continue`, with `$frame`'s borrow already dropped. The `iinc; goto`
    // fusion uses it for the loop test at the target
    // (`CRATONVM_INTERP_FUSED_LOOP_STEP_TEST`); every other site passes none.
    macro_rules! goto_arm {
        ($frame:expr, $saved_pc:expr, $b1:expr, $b2:expr) => {
            goto_arm!($frame, $saved_pc, $b1, $b2; {})
        };
        ($frame:expr, $saved_pc:expr, $b1:expr, $b2:expr; $tail:block) => {{
            let goto_pc: usize = $saved_pc;
            // Cast: bytecode operand decoding
            let offset = (($b1 as i16) << 8) | ($b2 as i16);
            // Cast: signed branch offset arithmetic
            $frame.pc = (goto_pc as isize + offset as isize) as usize;
            // `<= 0`: `goto +0` is javac's whole translation of
            // `while (true) {}`, and a debugger must still reach it.
            if offset <= 0 && diag_gates & DG_OBSERVERS_HOISTED == 0 {
                refresh_debugger_gate!(&*$frame);
            }
            // A forward `goto` past a loop's closing `goto` is a `break`: one
            // exit of that loop (`record_loop_exit_for_frame`).
            if offset > 0 && pgo_enabled {
                record_loop_exit_for_frame(shared, &*$frame, goto_pc);
            }
            if offset < 0 {
                // Refresh the per-method branch-profile arm (see
                // `branch_profile_on` and `branch_arm_possible`): javac
                // closes its loops with this `goto`.
                if branch_arm_possible && (branch_reread_always || !branch_profile_on) {
                    branch_profile_on = frame_branch_profile_armed($frame);
                }
                // Backward branch — record back-edge for PGO loop trip
                // profiling, under the loop's key (not an obsolete body's;
                // see `record_backedge_for_frame`).
                if pgo_enabled {
                    record_backedge_for_frame(shared, &*$frame, goto_pc);
                }
                // Backward branch — increment OSR counter (see
                // `cond_branch_arm!` for `wrapping_add`).
                $frame.backward_count = $frame.backward_count.wrapping_add(1);

                let entry_pc = $frame.pc;
                // The frame's poll point is read only past the floor.
                let bc_now = $frame.backward_count;
                let osr_due = bc_now >= osr_call_floor && bc_now >= $frame.osr_poll_at;
                let _ = $frame; // drop borrow before try_osr
                if osr_due {
                    match osr_backoff_at!(entry_pc) {
                        OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                        OsrBackoffOutcome::ContinueDispatch => continue,
                        OsrBackoffOutcome::ThrowJava(exc) => {
                            park_java_exception!(exc, OSR_FRAME_DECLINED_TO_CATCH);
                            continue;
                        }
                        OsrBackoffOutcome::Skip => {}
                    }
                }
                if backedge_poll_needed!() {
                    safepoint_check(shared, thread);
                }
                $tail
            }
            continue;
        }};
    }
    // ── Fused int compare-and-branch, written once ────────────────────────
    //
    // `iload{_N, n}; {iload{_N, n} | iconst_* | bipush | sipush |
    // aload{_N, n}; arraylength | ldc | ldc_w}; if_icmp<cond>` for all six
    // conditions (the `arraylength` form — javac's `i < a.length` — since
    // wave 5, under `arraylength_fusion_on`; the `ldc` form — a bound past
    // `sipush`'s range — since i2 wave 63, under `fused_ldc_bound_on`, and
    // only for an int the thread's `prim_const_sites` already holds; the
    // `getstatic` form — a bound in a non-final `static int` — since wave 64,
    // under `fused_getstatic_bound_on`, only for a site the thread's
    // `static_field_sites` already holds and never while a field watch is
    // armed). The
    // caller has already read the first operand (`$vx`, an int-tagged local)
    // and proved `$operand_pc < $code_len`; this
    // decodes the second operand and the branch and, on a match, hands the
    // branch to `cond_branch_arm!` at the BRANCH's own pc, so profiling,
    // back-edge accounting, OSR and the poll are exactly the unfused
    // `if_icmp<cond>` arm's. No match leaves everything untouched and falls
    // out, so the caller runs its plain `iload`. `code_ptr`/`code_len` are
    // parameters for the same hygiene reason as `cond_branch_arm!`'s.
    //
    // Interpreter round i6 wave 115, lane L8
    // (`CRATONVM_INTERP_FUSED_ZERO_BRANCH`, `DOOR_FUSED_ZERO_BRANCH`, default
    // off): the same group with no second operand, `iload{_N, n}; if<cond>`
    // (`if (flag)`, `while (n != 0)`, `if (x < 0)`; the wave-88 census's
    // `iload_1 -> ifne`, in 97 of 169 probes). The branch sits AT
    // `$operand_pc`; it is compared with zero as the `ifeq` .. `ifle` arms
    // compare (`if_zero_taken`) and handed to the same, single
    // `cond_branch_arm!` expansion at its own pc, so this macro still expands
    // the branch once per call site. An `if<cond>` opcode is never a second
    // operand `fused_int_operand` decodes, so neither form takes the other's
    // bytes; with the bit clear the opcode test falls to the two-operand
    // decode, exactly as before.
    macro_rules! fused_icmp_arm {
        ($frame:expr, $code_ptr:expr, $code_len:expr, $vx:expr, $operand_pc:expr) => {{
            let operand_pc: usize = $operand_pc;
            // SAFETY: the caller proved `operand_pc < $code_len`.
            let zero_op = unsafe { *$code_ptr.add(operand_pc) };
            // The branch's pc and whether it is taken, or `None` (nothing
            // read beyond the bytes, nothing written): the caller then runs
            // its plain `iload`.
            let fused_branch: Option<(usize, bool)> = if (0x99..=0x9e).contains(&zero_op) {
                if door_gates & DOOR_FUSED_ZERO_BRANCH != 0 && operand_pc + 2 < $code_len {
                    Some((operand_pc, if_zero_taken(zero_op, $vx)))
                } else {
                    None
                }
            } else if let Some((vy, br_pc)) =
                // SAFETY: the caller proved `operand_pc < $code_len`, so every
                // byte the operand decode reads lies in the padded buffer.
                unsafe {
                fused_int_operand(
                    $frame,
                    $code_ptr,
                    operand_pc,
                    arraylength_fusion_on,
                    site_census,
                    static_ldc_gates & SLG_FUSED_LDC_BOUND != 0,
                    &mut thread.site_tables.prim_const_sites,
                    static_ldc_gates & SLG_FUSED_GETSTATIC_BOUND != 0,
                    field_watch_hoist,
                    &mut thread.site_tables.static_field_sites,
                    |holder, slot| crate::vm::get_static_shared(shared, holder, slot),
                )
            } {
                if br_pc + 2 < $code_len {
                    // SAFETY: br_pc + 2 < code_len, checked above.
                    let br_op = unsafe { *$code_ptr.add(br_pc) };
                    if (0x9f..=0xa4).contains(&br_op) {
                        Some((br_pc, if_icmp_taken(br_op, $vx, vy)))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((br_pc, taken)) = fused_branch {
                // SAFETY: br_pc + 2 < code_len, checked by both forms above.
                let ob1 = unsafe { *$code_ptr.add(br_pc + 1) };
                // SAFETY: as above.
                let ob2 = unsafe { *$code_ptr.add(br_pc + 2) };
                // The state a back-edge safepoint in the unfused
                // sequence would see: the branch is the executing
                // bytecode (liveness and JVMTI location read it).
                $frame.last_instr_pc = br_pc;
                cond_branch_arm!($frame, br_pc, ob1, ob2, taken);
            }
        }};
    }
    // The T19.H1 stack-dump hook's body (see the loop top), written once for
    // its two sites since wave 82 (lane L7): the per-bytecode test of
    // `TG_STACK_DUMP`, and the poll's slow path under
    // `CRATONVM_INTERP_STACK_DUMP_AT_POLL` (`DG_STACK_DUMP_AT_POLL`).
    macro_rules! stack_dump_hook {
        () => {{
            if (!stack_dump_emitted || shared.stack_sample_mode()) && shared.stack_dump_pending() {
                // Wave 83 (lane L7): the dump and the request clear are
                // `serve_stack_dump`, out of line; only the two-load test
                // stays at each site.
                if serve_stack_dump(shared, thread) {
                    stack_dump_emitted = true;
                }
                // Don't park or sleep here — the watchdog aborts the process
                // after a short grace period, and if it doesn't (e.g. crashed
                // mid-way) we'd rather keep running than hang forever. The
                // `stack_dump_emitted` guard ensures we dump at most once per
                // nested `execute()` call so ack counts remain meaningful.
            }
        }};
    }

    loop {
        // `CRATONVM_INTERP_LEAN_LOOP_TOP` (interpreter round i6 wave 116,
        // lane L8; `VmConfig::interp_lean_loop_top`, default off): the
        // `LEAN_TOP` instance of this loop, chosen once per entry by
        // `execute_frame_from_index`. When no gate of `top_gates` is set and
        // the poll word matches, nothing the slow preamble below tests can
        // fire this iteration: the pending-signal test (`TG_TOP_SIGNALS`,
        // and `TG_SLOT_SIGNALS` is clear in this instance), the poll, and the
        // single-step hook (`TG_SINGLE_STEP`). So one test of the byte and
        // the poll compare replace the three bit tests, and the only thing
        // that can set a gate before the bytecode runs is the frame-switch
        // refresh below, which `lean_hit` sends this iteration to. In the
        // `false` instance `lean_quiet` is the constant `false` and this is
        // the loop as it was.
        let mut hot_fp: *mut Frame;
        let saved_pc: usize;
        let lean_quiet = LEAN_TOP && top_gates == 0 && loop_poll.load() == poll_gate;
        if lean_quiet {
            hot_fp = thread.frames.frame_ptr(frame_idx);
            if hot_fp.is_null() {
                // Cold: the same out-of-range error the hoist below raises.
                hot_fp = rederive_hot_frame_ptr(thread, frame_idx)?;
            }
            // SAFETY: non-null and in bounds, as in the hoist below.
            saved_pc = unsafe { (*hot_fp).pc };
            // SAFETY: as above.
            unsafe { (*hot_fp).last_instr_pc = saved_pc };
        } else {
            // Route a pending signal from the previous iteration's fast path —
            // a callee-thrown Java exception, or a `RuntimeError` an arm could not
            // throw in place — before the safepoint poll below. Both are only Rust
            // locals at this point, not present in any GC-scanned frame slot, so
            // polling first can let STW reclaim a Throwable before a caller's catch
            // handler stores it.
            //
            // One branch, not two: `|` tests the OR of both discriminants, and the
            // arms inside each `continue` or return, so the common path (neither
            // set) pays a single predicted-not-taken test per bytecode.
            //
            // Wave 84 (lane L7, `CRATONVM_INTERP_ONE_TOP_SIGNAL_TEST`): every
            // writer of either slot also sets `TG_PENDING` (`park_java_exception!`,
            // `park_runtime!`), and the stack-dump hook's `TG_STACK_DUMP` shares
            // the byte, so with the switch on the whole test is one bit test of
            // `top_gates` (`TG_TOP_SIGNALS`) and the slots are loaded only when it
            // fires. With it off (`TG_SLOT_SIGNALS`) the two slots are tested as
            // well, as before: the belt-and-braces form.
            if top_gates & TG_TOP_SIGNALS != 0
                || (top_gates & TG_SLOT_SIGNALS != 0
                    && (pending_java_exception.is_some() | pending_runtime_error.is_some()))
            {
                if let Some((exc, invoke_pc)) = pending_java_exception.take() {
                    // The bit stays while the other slot still holds a signal
                    // (it is routed at the next top, as before).
                    if pending_runtime_error.is_none() {
                        top_gates &= !TG_PENDING;
                    }
                    // The handler walk and its GC pin live in
                    // `exception_dispatch::unwind_to_handler` — shared with the
                    // `pending_runtime_error` arm below, which used to carry a
                    // byte-identical copy (ARCH-2026-08-04 A4a).
                    unwind_here!(exc, invoke_pc)?;
                    continue;
                }
                // Handle any pending runtime error from the previous iteration's fast path.
                if let Some(parked) = pending_runtime_error.take() {
                    top_gates &= !TG_PENDING;
                    let (re, invoke_pc) = *parked;
                    if aioobe2_dbg() {
                        if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } =
                            &re
                        {
                            let f = &thread.frames[frame_idx];
                            eprintln!(
                                "[AIOOBE2] index={} message={} class={} method={}{} pc={}",
                                index,
                                message.as_deref().unwrap_or("<none>"),
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor(),
                                invoke_pc
                            );
                        }
                    }
                    // Normalize the operand-stack overflow BEFORE throwing, exactly as
                    // the decoded path's conversion does further down.
                    //
                    // That site calls itself "the only point that converts runtime
                    // errors into Java exceptions". It is not, and has not been for as
                    // long as the invoke fast paths have routed through here: this arm
                    // converts too. `ValueStack` reports an overflow as
                    // `NotImplemented { feature: "operand stack overflow" }`, which
                    // `throw_runtime_error` maps to an *uncatchable* internal error, so
                    // a stack overflow arriving through a fast-path arm hard-unwound
                    // the whole call stack instead of surfacing as a catchable
                    // `java.lang.StackOverflowError` that an in-method
                    // `catch (StackOverflowError)` / `catch (Throwable)` can observe.
                    // Two conversion points that disagree is one conversion point too
                    // many; until they are merged they must at least agree.
                    let re = match re {
                        RuntimeError::NotImplemented { feature }
                            if feature == "operand stack overflow" =>
                        {
                            RuntimeError::StackOverflowError
                        }
                        other => other,
                    };
                    let exc_result = super::exceptions::throw_runtime_error(shared, thread, re);
                    match exc_result {
                        MethodCallFailed::ExceptionThrown(exc) => {
                            // Settled as the decoded path settles the throwable it
                            // converts from the same `RuntimeError` (its
                            // `ExceptionThrown` arm): a fresh throwable is always
                            // live, so this is the `native_pending_return` drain
                            // (wave 5; the fast path's AIOOBE, NASE, ASE, NPE
                            // ... used to leave a leftover rooted).
                            let exc = settle_thrown_exception(shared, thread, exc);
                            // Same walk as the `pending_java_exception` arm above, and
                            // now literally the same code (ARCH-2026-08-04 A4a). The
                            // two copies had already drifted in comments only, but the
                            // GC pin they share is subtle enough that a fix landing in
                            // one and not the other is a use-after-free reproducing on
                            // just one of the two throw paths.
                            unwind_here!(exc, invoke_pc)?;
                            continue;
                        }
                        other => return Err(other),
                    }
                }
                // Neither slot held a signal: the test fired for the stack-dump
                // hook below (or, defensively, for a bit no slot backs).
                top_gates &= !TG_PENDING;
                if top_gates & TG_STACK_DUMP != 0 {
                    stack_dump_hook!();
                }
            }

            // T19.H1 — opportunistic stack-dump hook.
            //
            // When the CLI watchdog fires (`--stack-dump-on-timeout=N`), it sets
            // `shared.debug.stack_dump_requested`. Every interpreter thread observes
            // the flag on its next dispatch iteration and self-dumps its frame
            // chain before the watchdog aborts the process. The load is
            // `Ordering::Relaxed` — a single predicted branch per bytecode in
            // the common case (flag always false).
            //
            // Under `--stack-sample-ms` the same hook is driven as a periodic
            // SAMPLER: the per-`execute()` latch is bypassed and the request is
            // consumed here, so the sampler thread's next re-arm produces the
            // next sample. That distinction is the whole point — with the latch
            // in place this hook fires once per nested interpreter entry, and the
            // resulting "profile" ranks methods by call count rather than by time
            // (a cheap method entered 100k times outranks the one that actually
            // burned the wall clock).
            //
            // What the sample POSITION means, because three profiles on
            // docs/feature-designs/real-jdk-mode.md
            // were read wrong: this hook is the first thing a loop iteration does,
            // and an invoke pushes the callee frame and `continue`s. So the time
            // an expensive INVOKE burns is reported against the callee at
            // `pc=0 last_pc=0` — a frame that has executed nothing. Anyone
            // aggregating leaf frames by method name files invoke cost under the
            // callee's name, where it reads as a slow body. Bucket
            // `pc == 0 && last_pc == 0` separately.
            // `probes/InvokeAttributionProbe.java` is the calibration: a
            // three-bytecode callee behind an `invokevirtual` takes 54% of the
            // samples at its entry and one sample anywhere in its body, and that
            // share tracks the separately-timed invoke delta (290-417 ns).
            //
            // Wave 82 (lane L7): the gate is the `TG_STACK_DUMP` bit of
            // `top_gates`; under `CRATONVM_INTERP_STACK_DUMP_AT_POLL` it is clear
            // and the same hook runs in the poll's slow path below instead.
            //
            // Wave 84 (lane L7): the hook's test is part of the pending-signal
            // test above (`TG_TOP_SIGNALS`), and the hook runs there, at this
            // same point of the iteration (every pending arm `continue`s first).

            // T19.H7 diag — opcode counter. Removed; documented findings in
            // roadmap-100.md T19.H7 section. Last localization:
            // `org/jboss/modules/Main.main` pc=1306 dispatched, then a native
            // call from that opcode never returns (interpreter loop never
            // re-entered).
            // Feature-gated (off by default) — see vm/Cargo.toml
            // (See above — diagnostic block removed.)

            // A newly-started or long straight-line frame may not hit an allocation
            // or backward-branch poll before another thread requests STW. Keep the
            // hot path to one atomic load; call the full safepoint machinery only
            // while a pause is actually active.
            //
            // # Why this is never hoisted into a local
            //
            // The obvious optimisation — hoist the poll into a `poll_pending`
            // local set at frame entry, the way `pgo_enabled` above is hoisted —
            // was weighed and REFUSED: the only lever it has is poll FREQUENCY,
            // which is time-to-safepoint, and the failure mode of getting that
            // wrong is a GC that waits forever for a thread that never polls.
            // (Until wave 26 this load was the flag's, with `Acquire`: a plain
            // `mov` on x86-64 but an `ldar` on aarch64. The poll word's load is
            // relaxed on every target.)
            //
            // Since wave 26 (lane L7) the per-bytecode load is not of the flag but
            // of this thread's own poll word (`loop_poll`), which counts a pause in
            // the same barrier critical section that raises the flag and uncounts
            // it in the one that lowers it, so the poll frequency and the
            // time-to-safepoint are what they were; the slow path re-reads the
            // flag with `Acquire` before it calls `safepoint_check`, and runs at
            // every bytecode while the pause lasts, as this loop always did.
            //
            // The same word is bumped by every frame move on this OS thread, which
            // sends the loop to the slow path, where the stack's own move count
            // says whether a frame of THIS stack moved onto another code
            // allocation since the gate's last refresh (a class redefinition;
            // `obsolete_frames`), or a conversion is pending. A pending
            // conversion -- the class-manager lock was busy
            // at the safepoint or blocking-region exit that owed it -- is retried
            // there, before any bytecode of this thread runs again, so a frame
            // running a replaced body never resolves one of its old constant-pool
            // indices in the new pool (interpreter round i1 wave 24, lane L3). The
            // gate below then refreshes.
            //
            // One relaxed load and one compare against a register per bytecode,
            // predicted not taken; everything else is in `loop_top_poll`.
            if loop_poll.load() != poll_gate {
                match loop_top_poll(shared, thread, stw_flag, loop_poll, fast_gate_code_moves) {
                    LoopTopPoll::Settled { word_gate } => poll_gate = word_gate,
                    LoopTopPoll::FramesMoved { moves, word_gate } => {
                        poll_gate = word_gate;
                        fast_gate_code_moves = moves;
                        fast_gate_code_ptr = std::ptr::null();
                        gate_saw_move = true;
                    }
                    LoopTopPoll::RetriedConversion => continue,
                }
                // The hoisted observer gates (`observers_hoisted`): every writer
                // bumps this word after it publishes, so they are re-read here.
                // The fence orders the word's load in `loop_top_poll` before the
                // gates' loads: a gate read stale then implies the word was read
                // before the writer's bump, so the next top comes back here.
                // `CRATONVM_JVMTI_STEP_POLL` (interpreter round i3 wave 81, lane
                // L1; `VmConfig::jvmti_step_poll`): the same re-read without
                // hoisting anything, so a `SingleStep` enabled while this frame
                // runs (from a JNI native it called, or an agent callback on this
                // thread) is posted from the next bytecode, as HotSpot does. Read
                // here, in the slow path, so the per-bytecode path is unchanged.
                if diag_gates & DG_OBSERVER_REREAD != 0 {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                    if let Some(watch) = field_watch_hoist.as_mut() {
                        *watch = crate::runtime::jvmti::any_field_watchpoint_active();
                    }
                    debug_vm_gate = breakpoints_armed_now(shared);
                    let step_now = crate::runtime::jvmti::any_single_step_listener_active();
                    let armed_now = debug_vm_gate
                        && thread.frames.get(frame_idx).is_some_and(|frame| {
                            debugger_gate_for_frame(
                                shared,
                                frame,
                                &mut debug_gates_seen,
                                &mut debug_frame_concerned,
                            )
                        });
                    apply_debugger_gate!(armed_now, step_now);
                }
                // `CRATONVM_INTERP_STACK_DUMP_AT_POLL` (wave 82, lane L7): the
                // stack-dump hook, moved off the per-bytecode path. Its writers
                // (`SharedVm::request_stack_dump`, `request_stack_sample`) bump
                // this VM's loop words after they raise the request, so a loop
                // already running comes here at its next top, and a loop entered
                // later comes here at its first top (the gate starts one behind).
                // The fence orders the word's load before the request's, as for
                // the observer gates above.
                if diag_gates & DG_STACK_DUMP_AT_POLL != 0 {
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                    stack_dump_hook!();
                }
                // `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` (interpreter round
                // i4 wave 95, lane L7; stage 1 of
                // `i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads`):
                // a C agent's `MethodEntry` callback for the frame an invoke arm
                // just pushed recorded a `PopFrame` or a forced return of it
                // (`jvmti_events::fire_method_entry_at_callback_point`, which
                // raised the thread's agent-request word and bumped this poll
                // word). This is that frame's first top: force the suspend point
                // for this pc, which takes the request before bytecode 0 runs, as
                // wave 62 forces it after a `SingleStep` callback's request (the
                // frame switch below keeps the gates forced). The writer is this
                // thread, so a relaxed load sees its own store.
                // Interpreter round i7 wave 136 (lane L1): the checked reader
                // clears the bit only for the frame the record is for (a loop
                // entered by Java run between the raise and that frame's top
                // leaves it), or when the record is gone or stale.
                if diag_gates & DG_METHOD_ENTRY_FRAME_CONTROL != 0
                    && take_method_entry_frame_control_checked(shared, thread, frame_idx)
                {
                    step_frame_control = true;
                    breakpoints_armed = true;
                    top_gates |= TG_TOP_HOOKS;
                    fusion_off = true;
                }
            }

            // ── Frame-pointer hoist, preamble (frame-arena.md §6.1) ──────────
            // `FrameStack` gives frames stable addresses, so one address
            // computation can serve the whole per-bytecode preamble instead of
            // six bounds-checked `imul`-by-`size_of::<Frame>()` re-indexes. The
            // region this covers runs from here to the `code_ptr`/`b2` reads just
            // below and contains no push, no pop and no `&mut` reborrow of
            // `thread.frames`; `hot_fp` is dead by the time the fast-path match
            // takes its real `&mut thread.frames[frame_idx]` borrow (which is
            // deliberately left as a borrow-checked reference, so the arms' `let
            // _ = frame;` discipline before calling back into `thread` keeps its
            // compile-time enforcement). Null iff `frame_idx >= len()`, which
            // routes through `VmError` where indexing would have panicked.
            //
            // Since wave 37 (lane L7) the match takes its `&mut Frame` from
            // `hot_fp` too (`FrameStack::frame_mut_from_ptr`), still borrowing the
            // whole stack, so the arms' discipline is unchanged; the two cold
            // hooks below that reborrow the stack or can run Java on this thread
            // re-derive it.
            hot_fp = thread.frames.frame_ptr(frame_idx);
            if hot_fp.is_null() {
                // Wave 120 (lane L8): the lean instance takes the same error
                // through `rederive_hot_frame_ptr`, which gets the index BY
                // VALUE; the inline `format!` below borrows `frame_idx` (see
                // `osr_backoff_at!`).
                if LEAN_TOP {
                    hot_fp = rederive_hot_frame_ptr(thread, frame_idx)?;
                } else {
                    return Err(MethodCallFailed::InternalError(VmError::Internal {
                        message: format!(
                            "frame index {frame_idx} out of range (depth {})",
                            thread.frames.len()
                        ),
                    }));
                }
            }
            // SAFETY: `hot_fp` is non-null and in bounds (checked above); see the
            // hoist note for the aliasing argument covering every deref below.
            saved_pc = unsafe { (*hot_fp).pc };
            unsafe { (*hot_fp).last_instr_pc = saved_pc };

            // T17.Δ.3 — JVMTI single-step dispatch hook, gated by the hoisted
            // `single_step_active` so the no-agent case (universal) does zero atomics
            // and never materializes the frame borrow here. `fire_jvmti_single_step`
            // re-checks the listener + per-thread step flag when a listener is active.
            if top_gates & TG_SINGLE_STEP != 0 {
                // Deliberately still re-indexed: this call takes `&JvmThread`,
                // which exposes the whole frame stack, so hoist rule 1 forbids
                // routing it through `hot_fp`. Cold and listener-gated — it costs
                // nothing in the universal no-agent case.
                // Wave 62 (lane L1): the callback may pop or force a return of
                // this frame; the suspend point below takes it, before the
                // bytecode runs (the gates stay forced until the next refresh,
                // which the popped or returned frame makes at once).
                let point = enter_callback_point(shared, thread, initial_frame_idx);
                fire_jvmti_single_step(
                    shared.vm_identity,
                    thread,
                    frame_idx,
                    saved_pc,
                );
                if point && leave_callback_point(shared, thread) {
                    step_frame_control = true;
                    breakpoints_armed = true;
                    top_gates |= TG_TOP_HOOKS;
                    fusion_off = true;
                }
                // The indexed borrow above reborrowed the stack (hoist rule 2).
                hot_fp = rederive_hot_frame_ptr(thread, frame_idx)?;
            }
        }

        // --- Fast path: handle hot bytecodes directly from raw bytes ---
        // Pre-read opcode + 2 operand bytes to avoid borrow conflicts with frame.
        // Bytecode is padded with 2 trailing zero bytes, so pc+1 and pc+2 are always
        // safe to read when pc is within the original (unpadded) code region.
        // The raw-bytecode handlers are the common interpreter path for every
        // verified class. Package names are not execution-policy inputs: the
        // old java/jdk/sun/Spring deny-list made identical bytecode select a
        // second implementation with different semantics and ~2.7x lower
        // throughput. Unsupported opcodes and guarded edge cases still fall
        // through to the shared decoded handler below.
        //
        // H7: `use_fast_path` is hoisted above the loop (ARCH-2026-08-04 A4b)
        // — see the note at its binding for what it selects and why the
        // per-class `skip_verification` case still relies on the per-site
        // checks below as a second line of defence.
        //
        // # The two structural proposals for this match, both REFUTED (2026-09-05)
        //
        // Neither is a lever, and both are cheap to re-refute if the idea comes
        // back. Straight-line arithmetic on this path costs ~7.5 ns per
        // bytecode against HotSpot's ~0.95 — the whole remaining floor.
        //
        // * **"Dispatch on the pre-decoded `QuickenedCode` stream instead."**
        //   `--noverify` flips `use_fast_path`, and the decoded path it selects
        //   ALREADY runs on that stream — so the two engines price directly, in
        //   one binary, on one program. `probes/FieldBurn.java`, N = 30 M,
        //   min-of-5, wall ms: arithmetic loop **2312 fast against 5078
        //   decoded**. The stream is 2.2x SLOWER. The cost is
        //   `execute_instruction`'s out-of-line call, its ~200-variant match on
        //   a 16-byte `Instruction`, the `thread.frames[frame_idx]` re-index
        //   per operand and the `Result` round trip; an index-threaded loop
        //   over the same handlers inherits all of it.
        // * **"Give the eight opcodes with no fast arm one."** The ratio above
        //   is per BYTECODE, and a switch executes once per iteration while
        //   doing the work of a whole comparison chain.
        //   `probes/SwitchBurn.java`, N = 20 M: `tableswitch` measures **4.7x**
        //   HotSpot against straight-line arithmetic's 7.9x — BETTER than the
        //   band, on the decoded path — and beats its own `ifchain` equivalent
        //   outright (1752 ms against 2989).
        //
        // The one thing never tested is the INDIRECT BRANCH itself: a single
        // dispatch site gives the predictor one history slot for every opcode
        // transition in every program. That needs a branch-misprediction
        // counter, i.e. a hardware profiler. **Do not build replicated dispatch
        // sites before that number exists** — this note is two refutations long
        // precisely because structural proposals here have not survived contact
        // with a probe.
        // SAFETY (every `hot_fp` deref below): see the hoist note above —
        // reads only, no push, no `&mut` reborrow of the stack in between.
        // Explicit `&` on the place expression: calling `.len()` directly on
        // `(*hot_fp).code` autorefs through the raw pointer, which the
        // `dangerous_implicit_autorefs` lint denies. The borrow is confined to
        // this statement, so it cannot alias a later `&mut Frame`.
        // SAFETY: `hot_fp` addresses the live executing frame and is not
        // invalidated in this push-free region; see the autoref note above.
        let code_ptr = unsafe { (*hot_fp).code.as_ptr() };
        // `CRATONVM_INTERP_LEAN_LOOP_TOP` (wave 116, lane L8): with the gates
        // quiet and the same bytecode allocation as the last refresh, the
        // length is the one that refresh cached (`lean_code_len`; an
        // allocation never changes its length, and every path that could
        // put another method's code under this pointer nulls
        // `fast_gate_code_ptr` first, as the admission verdict already
        // requires), and the refresh and the loop-top hooks are skipped:
        // with `top_gates == 0` neither has anything to do.
        let lean_hit = LEAN_TOP && lean_quiet && code_ptr == fast_gate_code_ptr;
        let code_len = if lean_hit {
            debug_assert_eq!(
                lean_code_len,
                // SAFETY: as for the read below.
                unsafe { (&(*hot_fp).code).len() }.wrapping_sub(2),
                "the lean loop top's cached code length went stale"
            );
            lean_code_len
        } else {
            // SAFETY: `hot_fp` addresses the live executing frame and is not
            // invalidated in this push-free region; see the autoref note above.
            let padded_code_len = unsafe { (&(*hot_fp).code).len() };
            debug_assert!(
                padded_code_len >= 2,
                "bytecode must be padded with at least 2 trailing bytes"
            );
            padded_code_len.wrapping_sub(2) // original unpadded length
        };

        if !lean_hit {
            // Per-method fast-path admission, refreshed only when the bytecode
            // allocation under dispatch changes (every frame switch changes it;
            // a recursive call keeps it, and keeps the same answer), or when a
            // redefinition moved a frame onto another allocation (which may reuse
            // a freed one's address: the move compare at the top nulls the gate).
            // Straight-line code pays one register compare, which replaces the
            // `use_fast_path` test it folds in. See `frame_fast_path_admitted`.
            if code_ptr != fast_gate_code_ptr {
                let moved = std::mem::replace(&mut gate_saw_move, false);
                fast_gate_code_ptr = code_ptr;
                if LEAN_TOP {
                    lean_code_len = code_len;
                }
                // SAFETY: a shared read of the live frame `hot_fp` addresses; no
                // push, pop or `&mut` reborrow of the stack happens while it lives.
                fast_gate_open =
                    use_fast_path && frame_fast_path_admitted(shared, unsafe { &*hot_fp });
                // A frame moved onto a replaced body's copy: this iteration holds
                // its current code, so no iteration can still hold a pointer into
                // the copies it ran before, which its next move may now free
                // (JEP 109; interpreter round i1 wave 23, lane L3). A match on the
                // frame's metadata kind for every other frame.
                // SAFETY: as above.
                if let Some(replaced) = unsafe { &*hot_fp }.replaced_body() {
                    replaced.note_code_reached_top();
                }
                // And every frame below it that this loop runs, when a move is
                // what brought the refresh: no iteration of any loop is
                // suspended in their code either -- enclosing loops' frames lie
                // below `initial_frame_idx`, nested loops are not running while
                // this one is at its top, and a stackless call ended its caller's
                // iteration -- so a caller below a callee that never returns
                // frees the copies its moves retired too (JEP 109; interpreter
                // round i1 wave 24, lane L3). A frame switch without a move pays
                // nothing more.
                if moved {
                    for below in initial_frame_idx..frame_idx {
                        // SAFETY: frames are contiguous in the stack's buffer and
                        // `below` is in bounds (`below < frame_idx`); a shared
                        // read of that live frame in the same push-free region.
                        let frame_below = unsafe { &*hot_fp.sub(frame_idx - below) };
                        if let Some(replaced) = frame_below.replaced_body() {
                            replaced.note_code_reached_top();
                        }
                    }
                }
                // SAFETY: as above.
                branch_profile_on =
                    branch_arm_possible && frame_branch_profile_armed(unsafe { &*hot_fp });
                // The debugger gate rides the same frame switch: a breakpoint
                // armed while a call tree runs is seen at the next call, return,
                // unwind or OSR transfer (a self-recursive call keeps the method,
                // and so waits for the next return or back edge). One relaxed
                // load per switch. The cached per-method answer belongs to the
                // method just left (wave 7).
                debug_gates_seen = u64::MAX;
                let step_was_active = top_gates & TG_SINGLE_STEP != 0;
                if diag_gates & DG_OBSERVERS_HOISTED != 0 {
                    // The VM-wide gate and the single-step union as the slow
                    // path last read them (`debug_vm_gate`), narrowed to the new
                    // method; nothing loaded without a debugger.
                    // SAFETY: as above.
                    let armed_now = debug_vm_gate
                        && debugger_gate_for_frame(
                            shared,
                            unsafe { &*hot_fp },
                            &mut debug_gates_seen,
                            &mut debug_frame_concerned,
                        );
                    apply_debugger_gate!(armed_now, step_was_active);
                } else {
                    // SAFETY: as above.
                    refresh_debugger_gate!(unsafe { &*hot_fp });
                }
                // Interpreter round i2 wave 62, lane L1: a `SingleStep` listener
                // armed since this loop's last refresh (an agent's native enabled
                // it, then the caller invoked this frame) was not seen by the
                // step hook at the top of this iteration, which ran on the stale
                // gate: post this bytecode's step now. HotSpot steps the callee's
                // first bytecode (`L1W61JvmtiStepIntoIntrinsic`'s `target=21`;
                // this VM's first two rows read 20, without `target@0`).
                // One branch on the frame switch for both wave-62 arms (bitwise
                // `|` / `&` on the bools: no short-circuit branch).
                let step_armed_here = (top_gates & TG_SINGLE_STEP != 0) & !step_was_active;
                if step_frame_control | step_armed_here {
                    if step_armed_here {
                        let point = enter_callback_point(shared, thread, initial_frame_idx);
                        fire_jvmti_single_step(
                            shared.vm_identity,
                            thread,
                            frame_idx,
                            saved_pc,
                        );
                        if point && leave_callback_point(shared, thread) {
                            step_frame_control = true;
                        }
                        hot_fp = rederive_hot_frame_ptr(thread, frame_idx)?;
                        // A callback that redefined this method moved the frame
                        // onto its new body: run this bytecode on the decoded
                        // path, as after a breakpoint park below.
                        // SAFETY: `hot_fp` was just re-derived for the live frame.
                        if unsafe { (&(*hot_fp).code).as_ptr() } != code_ptr {
                            fast_gate_open = false;
                            fast_gate_code_ptr = std::ptr::null();
                        }
                    }
                    // Wave 62 (lane L1): a `SingleStep` callback at this frame's
                    // first bytecode asked for frame control; keep the suspend
                    // point armed for this pc (the refresh just read the VM's
                    // gates).
                    if step_frame_control {
                        breakpoints_armed = true;
                        top_gates |= TG_TOP_HOOKS;
                    }
                }
            }
            // Breakpoint delivery and the JDWP suspend point, before the bytecode
            // at `saved_pc` runs (JVMS / JDWP: the event reports a location about
            // to execute). `hot_fp` is dead from here on, so handing out `&mut
            // JvmThread` is sound; a park inside answers safepoints and pushes no
            // frame, so `code_ptr` / `code_len` above stay valid. Cold and gated,
            // it costs nothing without an armed debugger.
            if top_gates & TG_TOP_HOOKS != 0 {
                if breakpoints_armed {
                    // A `ThreadReference.Stop` delivered here (interpreter round
                    // i1 wave 24) is thrown at `saved_pc`, before the bytecode
                    // runs; the loop top routes it before any safepoint.
                    let stopped = deliver_breakpoint_if_set(
                        shared,
                        thread,
                        frame_idx,
                        initial_frame_idx,
                        saved_pc,
                        step_frame_control,
                    );
                    step_frame_control = false;
                    // A stop there parks, and the park answers safepoints, which
                    // move a frame running a body a JVMTI redefinition replaced
                    // (an IDE HotSwap while stopped at this breakpoint) onto its
                    // translated copy: `code_ptr` and the fast-path verdict above
                    // are the OLD bytes', whose operands index the old pool. Run
                    // this bytecode on the decoded path, which reads the frame's
                    // code again, and refresh the gate at the next top (JEP 109;
                    // interpreter round i1 wave 22, lane L3).
                    if thread
                        .frames
                        .get(frame_idx)
                        .is_some_and(|f| f.code.as_ptr() != code_ptr)
                    {
                        fast_gate_open = false;
                        fast_gate_code_ptr = std::ptr::null();
                    }
                    match stopped {
                        Some(DebuggerStop::Throw(exc)) => {
                            park_java_exception!(exc, saved_pc);
                            continue;
                        }
                        // A JDWP `ForceEarlyReturn` (interpreter round i1 wave
                        // 43, lane L1): the frame returns the debugger's value,
                        // left on its operand stack, as the return arms below
                        // would return it.
                        Some(DebuggerStop::ForcedReturn) => {
                            match finish_forced_return(
                                shared,
                                thread,
                                frame_idx,
                                initial_frame_idx,
                                diag_gates & DG_THREAD_IS_VIRTUAL != 0,
                            )? {
                                ForcedReturnStep::Popped => {
                                    frame_idx -= 1;
                                    continue;
                                }
                                ForcedReturnStep::Returned(value) => return Ok(value),
                            }
                        }
                        // A JDWP `PopFrames` (interpreter round i1 wave 44, lane
                        // L1; `debug::pop_frames`): while the thread was parked
                        // the frames down to and including the named one were
                        // popped, and their caller, a frame this loop runs, was
                        // left at its invoke with the arguments back on its
                        // operand stack. It re-runs the invoke.
                        Some(DebuggerStop::FramesPopped) => {
                            frame_idx =
                                thread.frames.len().saturating_sub(1).max(initial_frame_idx);
                            fast_gate_open = false;
                            fast_gate_code_ptr = std::ptr::null();
                            // Wave 62 (lane L1): a C agent's forced return made
                            // after the `PopFrame` returns from the caller, taken
                            // by the suspend point at its invoke.
                            step_frame_control = forced_return_follows_pop(shared, thread);
                            continue;
                        }
                        None => {}
                    }
                }
                if diag_gates & DG_PAIR_CENSUS != 0 && fast_gate_open && saved_pc < code_len {
                    // SAFETY: `saved_pc < code_len`, as the fast path below reads it.
                    let op = unsafe { *code_ptr.add(saved_pc) };
                    note_opcode_pair_in(&mut census_prev, frame_idx, code_ptr, saved_pc, op);
                }
                // A breakpoint park may have run Java on this thread (a debugger's
                // method invocation), which can push frames and move them.
                hot_fp = rederive_hot_frame_ptr(thread, frame_idx)?;
            }
        }
        if fast_gate_open && saved_pc < code_len {
            // SAFETY: code_ptr points to the method's bytecode array; saved_pc is bounds-checked against code_len above, and the bytecode is padded with 2 trailing bytes.
            let opcode = unsafe { *code_ptr.add(saved_pc) };
            let b1 = unsafe { *code_ptr.add(saved_pc + 1) };
            let b2 = unsafe { *code_ptr.add(saved_pc + 2) };
            // SAFETY: `hot_fp` is this iteration's `frame_ptr(frame_idx)`,
            // non-null, re-derived after each hook above that reborrowed the
            // stack or could run Java; nothing since pushed, popped or holds a
            // reference to this frame. The result borrows `thread.frames`
            // exactly as `&mut thread.frames[frame_idx]` did, minus the two
            // bounds checks and the multiply (wave 37, lane L7).
            let frame = unsafe { thread.frames.frame_mut_from_ptr(hot_fp) };
            match opcode {
                // iload_0..3 with superinstruction look-ahead
                0x1a | 0x1b | 0x1c | 0x1d => {
                    let local_idx = (opcode - 0x1a) as usize; // Cast: bytecode operand decoding

                    // Superinstruction fusion executes two to four bytecodes in
                    // one iteration, so the loop-top JVMTI hook sees only the
                    // first pc of the group. With a single-step listener armed
                    // that silently drops the SingleStep events for the fused
                    // tail (the decoded path and HotSpot report one per
                    // bytecode), so fusion is declined while one is active —
                    // and while a debugger breakpoint is armed, which may sit
                    // on the group's interior pc (`fusion_off`) — one test of
                    // a hoisted bool, false in every agent-free run.
                    //
                    // Peek at the next opcode for superinstruction fusion.
                    // b1 is already pre-read as the byte at saved_pc+1.
                    // Check if the next byte is another iload_N (for two-operand fusions).
                    //
                    // Every fusion below also holds the other three rules a
                    // fused group must keep: none of its bytecodes can throw
                    // (so no exception can report a group pc), branch profiling
                    // and back-edge accounting use the branch's OWN pc, and a
                    // branch that lands inside a group simply dispatches that
                    // bytecode unfused, because fusion is decided here, at the
                    // group's first pc, on every execution.
                    if !fusion_off && saved_pc + 1 < code_len {
                        if let Some(vx) = frame.get_local_compact_unchecked(local_idx).as_int() {
                            // iload_X; iload_Y; iadd → push locals[X] + locals[Y]
                            // Round-3: typed int helpers avoid the Value enum
                            // round-trip when both locals are int-tagged.
                            if b1 >= 0x1a
                                && b1 <= 0x1d
                                && saved_pc + 2 < code_len
                                // SAFETY: saved_pc + 2 < code_len, checked first.
                                && unsafe { *code_ptr.add(saved_pc + 2) } == 0x60
                            {
                                // Widening: index conversion
                                let cvy = frame.get_local_compact_unchecked((b1 - 0x1a) as usize);
                                if let Some(vy) = cvy.as_int() {
                                    let sum = vx.wrapping_add(vy);
                                    // ...; istore_Z (wave 4) — javac's
                                    // `s += i` / `c = a + b` over low locals:
                                    // store the sum straight into local Z
                                    // instead of pushing it for the `istore`
                                    // to pop. Checked only once the `iadd`
                                    // matched, so no other `iload` pays for
                                    // it; nothing in the group can throw.
                                    if saved_pc + 3 < code_len {
                                        // SAFETY: saved_pc + 3 < code_len.
                                        let st = unsafe { *code_ptr.add(saved_pc + 3) };
                                        if (0x3b..=0x3e).contains(&st) {
                                            // The `istore_N` arm's own store
                                            // of an int slot, bit for bit.
                                            // Cast: bytecode operand decoding
                                            let z = (st - 0x3b) as usize;
                                            frame.set_local_compact_unchecked(
                                                z,
                                                CompactValue::int(sum),
                                            );
                                            frame.pc = saved_pc + 4;
                                            continue;
                                        }
                                    }
                                    frame.stack.push_int_unchecked(sum);
                                    frame.pc = saved_pc + 3;
                                    continue;
                                }
                            }
                            // iload_S; aload_N; iload_M; <x>aload; iadd |
                            // isub | imul | iand | ior | ixor; istore_Z
                            // (round i3 wave 73, lane L7,
                            // `CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`):
                            // `s += a[i]`, `h ^= b[k]`. Two range tests of
                            // the pre-read bytes, then the per-VM switch; the
                            // decode, the array checks and every decline are
                            // the out-of-line helper's. A decline has written
                            // nothing and pushed nothing, so the plain
                            // `iload` below runs and the `*aload` throws at
                            // its own pc.
                            if matches!(b1, 0x2a..=0x2d)
                                && matches!(b2, 0x1a..=0x1d)
                                && shared.config.interp_fused_array_accumulate
                            {
                                if let Some(zgc) = fast_field_zgc {
                                    if let Some(next_pc) = field_fast::fused_array_accumulate(
                                        shared, zgc, frame, code_len, saved_pc,
                                    ) {
                                        frame.pc = next_pc;
                                        continue;
                                    }
                                }
                            }
                            // iload_X; {iload_Y | iload n | iconst | bipush |
                            // sipush}; if_icmp<cond> — every int compare-and-
                            // branch javac emits for `i < n`, `i != 10`, ...,
                            // including the top-tested loop test
                            // (`iload; iload; if_icmpge exit`) the old
                            // `if_icmplt`-only fusion never matched.
                            // `saved_pc + 1 < code_len` is the test above.
                            fused_icmp_arm!(frame, code_ptr, code_len, vx, saved_pc + 1);
                        }
                    }
                    // iload_X; iconst_1; iadd; istore_X → locals[X] += 1
                    // Round-3: typed int helpers — same wrapping_add, no Value enum.
                    if !fusion_off && b1 == 0x04 && saved_pc + 3 < code_len {
                        // SAFETY: code_ptr points to the method's bytecode array; saved_pc + 3 < code_len is checked above.
                        let next2 = unsafe { *code_ptr.add(saved_pc + 2) };
                        let next3 = unsafe { *code_ptr.add(saved_pc + 3) };
                        // istore_0..3 opcodes are 0x3b..0x3e
                        if next2 == 0x60
                            && next3 >= 0x3b
                            && next3 <= 0x3e
                            // Widening: small integer index -> usize (non-negative, fits in pointer width)
                            && (next3 - 0x3b) as usize == local_idx
                        // Cast: bytecode operand decoding
                        {
                            let cv = frame.get_local_compact_unchecked(local_idx);
                            if let Some(v) = cv.as_int() {
                                frame.set_local_int_unchecked(local_idx, v.wrapping_add(1));
                                frame.pc = saved_pc + 4;
                                continue;
                            }
                        }
                    }
                    // iload_X; {iconst_m1..5 | bipush | sipush}; iadd | isub |
                    // imul | ishl | ishr | iushr | iand | ior | ixor
                    // [; istore_Z] (interpreter round i4 wave 91, lane L7,
                    // `CRATONVM_INTERP_FUSED_ICONST_ARITH`, default on, `=0` off): fib's
                    // `n - 1`, `i + 1` as an argument, `h & 0xff`, `x >>> 3`.
                    // The wave-88 pair census ranks `iconst_1 -> iadd` (56 of
                    // 169 probes) and `bipush -> iand` (34) in its top ten:
                    // the forms the self-increment above and the compare-and-
                    // branch (`fused_icmp_arm!`) do not take. Placed after
                    // both, so each keeps its group. Nothing in the group
                    // throws (`idiv` / `irem` are not taken); a local that is
                    // not int-tagged or any other byte declines before
                    // anything is written, and the plain `iload` below runs.
                    // One range test of `b1` for every other `iload_N`.
                    if matches!(b1, 0x02..=0x08 | 0x10 | 0x11)
                        && static_ldc_gates & SLG_FUSED_ICONST_ARITH != 0
                        && !fusion_off
                        && saved_pc + 2 < code_len
                    {
                        if let Some(vx) = frame.get_local_compact_unchecked(local_idx).as_int() {
                            // SAFETY: saved_pc + 1 < code_len, and the buffer
                            // carries two padding bytes past code_len.
                            if let Some((r, next)) =
                                unsafe { iconst_arith_group(code_ptr, code_len, saved_pc + 1, vx) }
                            {
                                if next < code_len {
                                    // SAFETY: next < code_len, checked above.
                                    let st = unsafe { *code_ptr.add(next) };
                                    if (0x3b..=0x3e).contains(&st) {
                                        // The `istore_N` arm's store of an int
                                        // slot, as the `iadd; istore_Z` form.
                                        // Cast: bytecode operand decoding
                                        let z = (st - 0x3b) as usize;
                                        frame.set_local_compact_unchecked(z, CompactValue::int(r));
                                        frame.pc = next + 1;
                                        continue;
                                    }
                                }
                                frame.stack.push_int_unchecked(r);
                                frame.pc = next;
                                continue;
                            }
                        }
                    }
                    // (An `iload_X; arraylength` "fusion" used to sit here. It
                    // could never fire on verified bytecode — `iload` pushes an
                    // int and `arraylength` requires an arrayref — so it only
                    // cost a compare on every unfused `iload_N`. The useful
                    // shape is `aload_X; arraylength`; see
                    // docs/feature-designs/real-jdk-mode.md.)
                    //
                    // iload_X; <x>aload (wave 25, lane L7) — the element read
                    // whose array is already on the stack: the inner dimension
                    // of `m[i][k]` (`aaload; iload k; iaload`) and every
                    // `this.a[i]` (`getfield; iload i; iaload`), the top
                    // pairs of the matrix kernel in the pair census
                    // (`tools/interp-pair-census`). The index comes straight
                    // from the local; a decline leaves the stack untouched
                    // and the plain `iload` below runs, so the `*aload`
                    // throws at its own pc. One range test for every other
                    // `iload_N`.
                    if matches!(b1, 0x2e..=0x35) && !fusion_off && saved_pc + 1 < code_len {
                        if let (Some(zgc), Some(index)) = (
                            fast_field_zgc,
                            frame.get_local_compact_unchecked(local_idx).as_int(),
                        ) {
                            if field_fast::array_load_top_with_index(
                                zgc,
                                &mut frame.stack,
                                index,
                                b1,
                                ref_array_fast_on,
                                site_census,
                            ) {
                                frame.pc = saved_pc + 2;
                                continue;
                            }
                        }
                    }
                    // iload_M; dup2; <x>aload; ...; <x>astore with the array
                    // already on the stack (round i3 wave 72, lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP`):
                    // `this.counts[i]++`, `m[r][c] += k`. One byte test for
                    // every other `iload_N`, then the per-VM switch; the
                    // base switch, the decode and every decline are the
                    // out-of-line helper's, and a decline leaves the stack
                    // as it was so the plain `iload` below runs.
                    //
                    // Wave 75 (lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`): a pc whose
                    // group the helper already refused on its bytes
                    // (`this.a[j] += SF`) is remembered in the method's
                    // decline memo and costs no decode. (Wave 73's byte screen
                    // stood here; retired in wave 75.) Since wave 76 (lane L7)
                    // the memo is tested inside the helper, first thing: the
                    // inline test was one of the three that grew this
                    // function's frame to its ratchet.
                    if b1 == 0x5c && !fusion_off && shared.config.interp_fused_array_increment_top {
                        if let Some(zgc) = fast_field_zgc {
                            if let Some(next_pc) = field_fast::fused_array_increment_top(
                                shared,
                                zgc,
                                frame,
                                code_len,
                                saved_pc,
                                &mut thread.site_tables.static_field_sites,
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    // No superinstruction matched — fall back to plain iload.
                    // Round-3: push the raw CompactValue (no Value round-trip).
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(local_idx));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // istore_0..3 — Round-3: pop the raw CompactValue and stash directly.
                // No Value enum round-trip; tag is preserved verbatim from the stack slot.
                0x3b => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(0, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3c => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(1, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3d => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(2, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x3e => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(3, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // aload_0..3 — Round-3: branch on compact tag.
                //
                // Object / Null locals are pushed verbatim with zero
                // validation overhead; every other shape (the JNI
                // smuggled-jobject `long`, an int-tagged zero) takes the
                // validated `L` coerce. `load_ref_local` is the one
                // implementation `aload n` shares, so the two spellings
                // cannot disagree (wave 3: this arm used to push an
                // int-tagged zero as an int, which `aload n` read as null).
                0x2a..=0x2d => {
                    // Cast: bytecode operand decoding
                    let local_idx = (opcode - 0x2a) as usize;
                    // aload_N; arraylength (wave 4) — javac's `a.length`, the
                    // bound of every array loop, under the rules every fusion
                    // keeps (`fusion_off`; nothing in the group throws). Wave
                    // 5: the length is read straight out of the local
                    // (`field_fast::arraylength_of_local`) under the hoisted
                    // `arraylength_fusion_on`, instead of pushing the reference
                    // and running `arraylength_fast` over the stack — which
                    // re-tested its kill switch, re-checked the depth and
                    // popped what it had just seen pushed. A null, non-array
                    // or non-`Object` local declines BEFORE anything is
                    // pushed, and the plain `aload_N` below runs, so the
                    // `arraylength` raises its NPE at its own pc.
                    if b1 == 0xbe && arraylength_fusion_on && !fusion_off && saved_pc + 1 < code_len
                    {
                        if let Some(len) = field_fast::arraylength_of_local(
                            frame.get_local_compact_unchecked(local_idx),
                            site_census,
                        ) {
                            frame.stack.push_int_unchecked(len);
                            frame.pc = saved_pc + 2;
                            continue;
                        }
                    }
                    // aload_N; iload_M; <x>aload (wave 24, lane L7) — javac's
                    // `a[i]`, the element read of every array loop, which
                    // otherwise runs as three dispatches that push the array
                    // and the index only for the third to pop them. Both are
                    // read straight out of their locals and the element goes
                    // through the `*aload` arm's own quickened readers
                    // (`array_load_prim`, `array_load_ref` for `aaload`), so
                    // the arm's admission (`fast_field_zgc`, the reference
                    // kill switch inside `array_load_ref`) is unchanged. A
                    // null / non-`Object` array local, a non-int index local,
                    // a negative or out-of-range index, or an element type
                    // the opcode does not name pushes NOTHING and falls to
                    // the plain `aload_N` below, so the `*aload` raises its
                    // NPE / AIOOBE at its own pc, unfused; nothing in the
                    // group throws. Declined under `fusion_off` like every
                    // fusion.
                    if matches!(b1, 0x1a..=0x1d)
                        && matches!(b2, 0x2e..=0x35)
                        && !fusion_off
                        && saved_pc + 2 < code_len
                    {
                        if let Some(zgc) = fast_field_zgc {
                            let arr_cv = frame.get_local_compact_unchecked(local_idx);
                            // Cast: bytecode operand decoding
                            let idx_cv = frame.get_local_compact_unchecked((b1 - 0x1a) as usize);
                            if let (Some(aptr), Some(index)) =
                                (arr_cv.as_object_ptr(), idx_cv.as_int())
                            {
                                if aptr != 0 && index >= 0 {
                                    // SAFETY: a non-null `Object`-tagged local
                                    // slot holds a heap address, the bits
                                    // `aload_N` would push; both readers
                                    // re-validate the header before reading.
                                    let arr_ref = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
                                    // One out-of-line call (wave 26): the
                                    // readers stay inlined in the `*aload`
                                    // arm only.
                                    if field_fast::array_load_fused(
                                        zgc,
                                        &mut frame.stack,
                                        arr_ref,
                                        index,
                                        b2,
                                        ref_array_fast_on,
                                        site_census,
                                    ) {
                                        frame.pc = saved_pc + 3;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    // aload_N; iload n; <x>aload (round i3 wave 75, lane L7,
                    // `CRATONVM_INTERP_FUSED_WIDE_LOCALS`): the same element
                    // read with the index past local 3 -- the for-each
                    // loop's hidden `i$`. One byte test for every other
                    // `aload_N`, then the per-VM switch; the rest of the
                    // shape, both locals and every decline are the
                    // out-of-line helper's, and a decline pushes nothing.
                    //
                    // Wave 76 (lane L7): one six-argument call that tests the
                    // heap option itself (`array_load_wide_locals_at`), so the
                    // arm keeps neither the census nor the reference switch
                    // live for it (frame ratchet).
                    if b1 == 0x15 && !fusion_off && shared.config.interp_fused_wide_locals {
                        if let Some(next_pc) = field_fast::array_load_wide_locals_at(
                            shared,
                            fast_field_zgc,
                            frame,
                            code_len,
                            saved_pc + 1,
                            local_idx,
                        ) {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    // aload_N; iload_M; dup2; <x>aload; <operand>; iadd |
                    // isub | imul | iand | ior | ixor; [i2b | i2c | i2s];
                    // <x>astore (round i3 wave 72, lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT`): `a[i]++`,
                    // `hist[b] += k`, `m[i] |= k`. Only the `dup2` byte and
                    // the `iload_M` range are tested here (javac puts a
                    // `dup2` after `aload_N; iload_M` only for a compound
                    // assignment to `a[i]`), then the per-VM
                    // switch; the decode, the array checks and every decline
                    // are the out-of-line helper's. A decline has written
                    // nothing and pushed nothing, so the plain `aload_N`
                    // below runs and the `*aload` raises its NPE / AIOOBE at
                    // its own pc.
                    //
                    // Wave 74 (lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`): the same
                    // entry opened `aload_N; iload_M; aload_N` (the same
                    // opcode byte twice), `a[j] = a[j] + k`; the helper
                    // checks the repeated `iload_M` and decodes from the
                    // `*aload`. One more byte compare, and only when `b2` is
                    // not a `dup2`; with the switch off the per-VM bool stops
                    // it.
                    //
                    // Wave 75 (lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`): a pc whose
                    // group the helper already refused on its bytes (`a[j]
                    // += SF`, `a[j] = a[j] + b[j]`) is remembered in the
                    // method's decline memo (`field_fast::array_increment_declined`)
                    // and costs no decode; the plain `aload_N` below runs, as
                    // after the call's decline. (Wave 73's byte screen stood
                    // here; it was kept off and retired in wave 75.) Since
                    // wave 76 (lane L7) the helper tests the memo first, so
                    // the arm keeps no inline memo probe (frame ratchet).
                    if (b2 == 0x5c
                        || (b2 == opcode && shared.config.interp_fused_array_update_no_dup))
                        && matches!(b1, 0x1a..=0x1d)
                        && !fusion_off
                        && shared.config.interp_fused_array_increment
                    {
                        if let Some(zgc) = fast_field_zgc {
                            if let Some(next_pc) = field_fast::fused_array_increment_local(
                                shared,
                                zgc,
                                frame,
                                code_len,
                                saved_pc,
                                &mut thread.site_tables.static_field_sites,
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    // aload_N; dup; getfield #F; <int operand>; iadd | isub;
                    // putfield #F (wave 68, lane L7,
                    // `CRATONVM_INTERP_FUSED_FIELD_INCREMENT`): `this.count++`
                    // on a quickened int instance field, read and stored by
                    // `field_fast::fused_field_increment` through the quickened
                    // arms' own accessors, nothing pushed. Only the two opcode
                    // bytes are tested here; the rest of the shape, the site,
                    // the watch union and every decline are the helper's (out
                    // of line, so the loop's frame keeps its size). A decline
                    // has written nothing and pushed nothing, so the plain
                    // `aload_N` below runs and the `getfield` raises a null
                    // receiver's NPE at its own pc.
                    //
                    // Wave 70 (lane L7,
                    // `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP`): the
                    // same group opened `aload_N; aload_N` (the same opcode
                    // byte twice, so the same local), `count = count + 1`,
                    // when the mode byte carries `FIELD_INC_NO_DUP`. It is
                    // tried before the `aload_N; getfield` fusion below would
                    // serve the second load; a decline leaves the stack
                    // untouched, as the `dup` form's does. With the bit clear
                    // the prefix never calls the helper (`this.a = this.b +
                    // k` keeps its old path).
                    //
                    // Wave 71 (lane L7): with the bit set, the prefix is
                    // offered to the helper only when the group's bytes can
                    // be an increment of ONE field
                    // (`field_fast::no_dup_group_may_fuse`: an operand, the
                    // arithmetic, a `putfield` with the `getfield`'s two
                    // index bytes). `this.a = this.b + k` shares the prefix
                    // and paid a declined call per execution (+15% in the
                    // W70 bench); it now falls to the `aload_N; getfield`
                    // fusion below without the call. The `dup` form is not
                    // screened here (unchanged), and a bit-clear mode never
                    // reads a byte of it.
                    if b2 == 0xb4
                        && (b1 == 0x59
                            || (b1 == opcode
                                && fused_field_increment_mode & field_fast::FIELD_INC_NO_DUP != 0
                                && field_fast::no_dup_group_may_fuse(
                                    // SAFETY: `code_ptr` addresses the
                                    // frame's padded bytecode, `code_len + 2`
                                    // bytes; the slice covers the unpadded
                                    // `code_len` bytes only.
                                    unsafe { std::slice::from_raw_parts(code_ptr, code_len) },
                                    saved_pc,
                                    fused_field_increment_mode & field_fast::FIELD_INC_TWO_FIELDS
                                        != 0,
                                )))
                        && fused_field_increment_mode != 0
                        && !fusion_off
                    {
                        if let Some(zgc) = fast_field_zgc {
                            // Wave 83 (lane L7): the `ldc2_w` operand's gate
                            // bit rides above the hoisted mode byte
                            // (`field_fast::FIELD_INC_LDC2W`).
                            if let Some(next_pc) = field_fast::fused_field_increment(
                                zgc,
                                &mut thread.site_tables,
                                frame,
                                code_len,
                                saved_pc,
                                u16::from(fused_field_increment_mode)
                                    | (static_ldc_gates & SLG_FUSED_INCREMENT_LDC2W),
                                field_watch_hoist,
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    load_ref_local(shared, frame, local_idx);
                    // aload_N; ifnull | ifnonnull (interpreter round i6 wave
                    // 115, lane L8, `CRATONVM_INTERP_FUSED_NULL_BRANCH`,
                    // default off): `if (x == null)`, `if (x != null)`,
                    // `while (node != null)` in one dispatch. The reference
                    // `load_ref_local` just pushed is popped and tested by
                    // the `ifnull` arm's own `pop_ref_operand_is_null`, and
                    // the branch goes to `cond_branch_arm!` at its OWN pc
                    // (`saved_pc + 1`), so its profile, back-edge
                    // accounting, OSR offer and poll are the unfused arm's.
                    // Nothing in the pair throws. No other `aload_N` fusion
                    // starts with these bytes, so none loses its group; with
                    // the bit clear an `aload_N` pays one range test of `b1`.
                    if matches!(b1, 0xc6 | 0xc7)
                        && door_gates & DOOR_FUSED_NULL_BRANCH != 0
                        && !fusion_off
                        && saved_pc + 3 < code_len
                    {
                        let is_null = pop_ref_operand_is_null(&mut frame.stack);
                        let br_pc = saved_pc + 1;
                        // SAFETY: saved_pc + 3 < code_len, checked above.
                        let ob2 = unsafe { *code_ptr.add(saved_pc + 3) };
                        // The state a back-edge safepoint in the unfused
                        // pair would see: the branch is the executing
                        // bytecode.
                        frame.last_instr_pc = br_pc;
                        cond_branch_arm!(frame, br_pc, b2, ob2, is_null == (b1 == 0xc6));
                    }
                    // aload_N; {aload_M | iload_M}; putfield | invoke*
                    // (interpreter round i4 wave 89, lane L7,
                    // `CRATONVM_INTERP_FUSED_LOAD_PAIR`, default on; the
                    // pair census's `aload_0 -> aload_1` / `aload_0 ->
                    // iload_1`): the second load in the same dispatch,
                    // and for `putfield` the quickened store too
                    // (`field_fast::fused_load_pair`). Only ahead of a
                    // `putfield` or an invoke, which no fusion of the second
                    // load's own arm starts with, so no existing group loses
                    // its first bytecode. Nothing here throws: a `putfield`
                    // the quickened arm declines is left with both values
                    // pushed and runs at its own pc. `b2` past the code is
                    // padding (0), which matches neither range.
                    if static_ldc_gates & SLG_FUSED_LOAD_PAIR != 0
                        && matches!(b1, 0x1a..=0x1d | 0x2a..=0x2d)
                        && matches!(b2, 0xb5..=0xb9)
                        && !fusion_off
                    {
                        // Interpreter round i6 wave 119, lane L8
                        // (`CRATONVM_INTERP_FUSED_STORE_CHAIN`, default off):
                        // the stores that follow a served `this.x = x`, one
                        // after another, in the same dispatch
                        // ([`fused_store_chain`]).
                        frame.pc = if b2 == 0xb5 && door_gates & DOOR_FUSED_STORE_CHAIN != 0 {
                            fused_store_chain(
                                shared,
                                &mut thread.site_tables.fast_field_sites,
                                frame,
                                code_len,
                                saved_pc,
                                door_gates & DOOR_FUSED_STORE_CHAIN_WIDE != 0,
                            )
                        } else {
                            field_fast::fused_load_pair(
                                shared,
                                &mut thread.site_tables.fast_field_sites,
                                frame,
                                code_len,
                                saved_pc,
                            )
                        };
                        continue;
                    }
                    // aload_N; {iload n | aload n}; putfield [; ...] (interpreter
                    // round i6 wave 120, lane L8,
                    // `CRATONVM_INTERP_FUSED_STORE_CHAIN_WIDE`, default off; on
                    // top of the load pair and the store chain): `this.e = e`
                    // with the value past local 3, a constructor's fifth and
                    // later parameters. The value and the store are
                    // `store_pair_wide`'s, the rest of the chain
                    // `fused_store_chain`'s; a decline leaves the second load
                    // or the `putfield` to run at its own pc. Off: one gate-bit
                    // test for an `aload_N` followed by `iload n` / `aload n`.
                    if matches!(b1, 0x15 | 0x19)
                        && door_gates & DOOR_FUSED_STORE_CHAIN_WIDE != 0
                        && door_gates & DOOR_FUSED_STORE_CHAIN != 0
                        && static_ldc_gates & SLG_FUSED_LOAD_PAIR != 0
                        && !fusion_off
                        && saved_pc + 3 < code_len
                        // SAFETY: saved_pc + 3 < code_len, checked above.
                        && unsafe { *code_ptr.add(saved_pc + 3) } == 0xb5
                    {
                        frame.pc = fused_store_chain(
                            shared,
                            &mut thread.site_tables.fast_field_sites,
                            frame,
                            code_len,
                            saved_pc,
                            true,
                        );
                        continue;
                    }
                    // aload_N; getfield — the pair behind every instance-field
                    // read in an instance method (`aload_0; getfield`), run in
                    // one iteration through the same quickened `getfield_fast`
                    // the `getfield` arm tries first. It reads only the operand
                    // stack and the site cache and cannot throw: a null receiver,
                    // a cold site or an armed field watchpoint is a MISS, which
                    // leaves the stack exactly as `aload_N` left it and
                    // dispatches the `getfield` on its own next iteration, at its
                    // own pc — where the NPE, the resolution and the JVMTI
                    // FieldAccess event all happen as they would unfused.
                    // Declined under single-step or an armed breakpoint for the
                    // same reason as the `iload` fusions (`fusion_off`).
                    if b1 == 0xb4 && !fusion_off && saved_pc + 3 < code_len {
                        if let Some(zgc) = fast_field_zgc {
                            // SAFETY: saved_pc + 3 < code_len, checked above.
                            let lo = unsafe { *code_ptr.add(saved_pc + 3) };
                            let cp_index = ((b2 as u16) << 8) | (lo as u16); // Cast: bytecode operand decoding
                            if field_fast::getfield_fast(
                                shared,
                                zgc,
                                &mut thread.site_tables.fast_field_sites,
                                frame,
                                cp_index,
                                site_census,
                                diag_gates & DG_FIELD_PHASES != 0,
                                field_watch_hoist,
                            ) {
                                // `this.a.b` (wave 116, lane L8,
                                // `CRATONVM_INTERP_FUSED_GETFIELD_CHAIN`): the
                                // getfields that follow, in the same dispatch.
                                // SAFETY: saved_pc + 3 < code_len, so saved_pc
                                // + 4 lies in the padded buffer.
                                if door_gates & DOOR_FUSED_GETFIELD_CHAIN != 0
                                    && unsafe { *code_ptr.add(saved_pc + 4) } == 0xb4
                                {
                                    frame.pc = getfield_chain(
                                        shared,
                                        zgc,
                                        &mut thread.site_tables.fast_field_sites,
                                        frame,
                                        code_len,
                                        saved_pc + 4,
                                    );
                                    continue;
                                }
                                frame.pc = saved_pc + 4;
                                continue;
                            }
                        }
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // astore_0..3 — one implementation with `astore n`
                // (`store_ref_local`): a plain Object / Null slot is stored
                // verbatim; every other shape (the JNI smuggled-jobject `long`
                // that GCs the underlying handle — Letsgo AV after
                // `ConfigurationClassEnhancer.enhance` — an int-tagged zero, a
                // KIND_LONG-marked slot) is decoded kind-aware and stored
                // through the validated `L` coerce. A `returnAddress` passes
                // the coerce unchanged.
                0x4b..=0x4e => {
                    store_ref_local(shared, frame, (opcode - 0x4b) as usize); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iadd — AUDIT CRIT-4: int-typed pop/push, no Value enum round-trip.
                0x60 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_add(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // isub
                0x64 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_sub(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // imul
                0x68 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va.wrapping_mul(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // idiv — needs to fall back to slow path on division by zero
                // so the spec-mandated ArithmeticException is thrown.  Push
                // raw bits back onto the stack and break out of the fast match.
                0x6c => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    if vb != 0 {
                        frame.stack.push_int_unchecked(va.wrapping_div(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Restore stack for the slow path.
                    frame.stack.push_int_unchecked(va);
                    frame.stack.push_int_unchecked(vb);
                }
                // irem — same fallback policy as idiv.
                0x70 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    if vb != 0 {
                        frame.stack.push_int_unchecked(va.wrapping_rem(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    frame.stack.push_int_unchecked(va);
                    frame.stack.push_int_unchecked(vb);
                }
                // iconst_m1..5 — AUDIT CRIT-4: direct CompactValue push, no Value enum.
                0x02 => {
                    frame.stack.push_int_unchecked(-1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x03 => {
                    frame.stack.push_int_unchecked(0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x04 => {
                    frame.stack.push_int_unchecked(1);
                    // iconst_1; goto (interpreter round i4 wave 89, lane L7,
                    // `CRATONVM_INTERP_FUSED_ICONST_GOTO`, default off; the
                    // pair census's `iconst_1 -> goto`, javac's boolean
                    // materialisation `c ? true : false`): the `goto` in the
                    // same dispatch, through `goto_arm!` at the `goto`'s own
                    // pc, so a backward one keeps its back-edge accounting,
                    // OSR offer and poll, exactly as the `iinc; goto` fusion
                    // does. Nothing in the pair throws.
                    if b1 == 0xa7
                        && static_ldc_gates & SLG_FUSED_ICONST_GOTO != 0
                        && !fusion_off
                        && saved_pc + 3 < code_len
                    {
                        // SAFETY: saved_pc + 3 < code_len, checked above.
                        let gb2 = unsafe { *code_ptr.add(saved_pc + 3) };
                        frame.last_instr_pc = saved_pc + 1;
                        goto_arm!(frame, saved_pc + 1, b2, gb2);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x05 => {
                    frame.stack.push_int_unchecked(2);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x06 => {
                    frame.stack.push_int_unchecked(3);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x07 => {
                    frame.stack.push_int_unchecked(4);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x08 => {
                    frame.stack.push_int_unchecked(5);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // bipush — AUDIT CRIT-4: int-typed push.
                0x10 => {
                    let val = b1 as i8 as i32; // Cast: bytecode operand decoding
                    frame.stack.push_int_unchecked(val);
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // sipush — AUDIT CRIT-4: int-typed push.
                0x11 => {
                    let val = ((b1 as i16) << 8) | (b2 as i16); // Cast: bytecode operand decoding
                    frame.stack.push_int_unchecked(val as i32); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    continue;
                }
                // iinc — Round-3: typed int read/write, no Value enum.
                //
                // `get_local_int_unchecked` returns 0 for non-Int slots (mirrors
                // the prior `Value::Int(_)` match — falling through to the slow
                // path on type-mismatch is unnecessary because verified
                // bytecode guarantees Int at this site).
                // iinc — the local index is the raw bytecode operand and the
                // accessors below index `frame.locals` without a bounds check,
                // so this arm needs the same `b1 < max_locals` admission test
                // its `iload`/`istore`/`astore` neighbours carry. Without it a
                // per-class `skip_verification` class (which the global
                // `use_fast_path` gate does NOT cover) reaches
                // `set_local_int_unchecked` with an out-of-range index and
                // PANICS on the `Vec` index — the one outcome this module's
                // zero-panic gate exists to prevent. Out of range falls through
                // to the bounds-checked decoded handler, unchanged.
                // Cast: the u8 local-index operand and the u16 `max_locals` both widen to usize losslessly.
                0x84 if (b1 as usize) < frame.max_locals as usize => {
                    let idx = b1 as usize; // Cast: bytecode operand decoding
                    let inc = b2 as i8 as i32; // Cast: bytecode operand decoding
                    let v = frame.get_local_int_unchecked(idx);
                    frame.set_local_int_unchecked(idx, v.wrapping_add(inc));
                    // iinc; goto — javac's loop step (`i++` then the jump back
                    // to the top-tested condition) in one iteration. `iinc`
                    // cannot throw, and `goto_arm!` accounts the back edge at
                    // the `goto`'s own pc. Declined under single-step, like
                    // every fusion.
                    if !fusion_off
                        && saved_pc + 5 < code_len
                        // SAFETY: saved_pc + 5 < code_len, checked first.
                        && unsafe { *code_ptr.add(saved_pc + 3) } == 0xa7
                    {
                        // SAFETY: as above.
                        let gb1 = unsafe { *code_ptr.add(saved_pc + 4) };
                        let gb2 = unsafe { *code_ptr.add(saved_pc + 5) };
                        frame.last_instr_pc = saved_pc + 3;
                        // ...; and the loop test at the goto's target in the
                        // same dispatch (interpreter round i6 wave 118, lane
                        // L8, `CRATONVM_INTERP_FUSED_LOOP_STEP_TEST`, default
                        // off): javac's `for` loop is `top: iload i; <bound>;
                        // if_icmp<cond> exit; body; iinc i k; goto top`, so a
                        // trip pays two dispatches of loop overhead (this
                        // group and `fused_icmp_arm!`'s at `top`); with the
                        // switch on it pays one. The tail runs after the
                        // goto's own back-edge accounting, and only when the
                        // loop top it skips would have done nothing: no
                        // parked signal or stack-dump request, no single-step
                        // or loop-top hook (`top_gates`), no fusion veto
                        // (`fusion_off`: a step listener or a breakpoint
                        // concerning the method), an unchanged poll word (no
                        // pause, frame move, observer re-read or agent
                        // request since this iteration's poll), and the same
                        // code under the frame (a safepoint the goto polled
                        // may have moved it). The test at `top` then runs
                        // exactly as its own iteration would run it (the same
                        // `fused_icmp_arm!`, at its own pcs); any shape that
                        // does not fuse leaves the frame at `top`, untouched,
                        // and the loop top runs it.
                        goto_arm!(frame, saved_pc + 3, gb1, gb2; {
                            if door_gates & DOOR_FUSED_LOOP_STEP_TEST != 0
                                && !fusion_off
                                && top_gates & (TG_TOP_SIGNALS | TG_SINGLE_STEP | TG_TOP_HOOKS)
                                    == 0
                                && loop_poll.load() == poll_gate
                            {
                                if let Some(frame) = thread.frames.get_mut(frame_idx) {
                                    let top_pc = frame.pc;
                                    if frame.code.as_ptr() == code_ptr && top_pc + 2 < code_len {
                                        // SAFETY: top_pc + 2 < code_len.
                                        let (top_op, top_b1) = unsafe {
                                            (*code_ptr.add(top_pc), *code_ptr.add(top_pc + 1))
                                        };
                                        let head = match top_op {
                                            0x1a..=0x1d => {
                                                Some((usize::from(top_op - 0x1a), top_pc + 1))
                                            }
                                            0x15 if usize::from(top_b1)
                                                < usize::from(frame.max_locals) =>
                                            {
                                                Some((usize::from(top_b1), top_pc + 2))
                                            }
                                            _ => None,
                                        };
                                        if let Some((local, operand_pc)) = head {
                                            if let Some(vx) =
                                                frame.get_local_compact_unchecked(local).as_int()
                                            {
                                                // The loop top's store for `top`.
                                                frame.last_instr_pc = top_pc;
                                                // `operand_pc < code_len`:
                                                // top_pc + 2 < code_len above.
                                                fused_icmp_arm!(
                                                    frame, code_ptr, code_len, vx, operand_pc
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        });
                    }
                    frame.pc = saved_pc + 3;
                    continue;
                }
                // wide iinc (0xc4 0x84 idx16 const16) — `i += 1000`, and any
                // `iinc` on a local >= 256. The other `wide` forms are rare and
                // stay on the decoded path, as does an index outside the
                // frame's locals (the same `max_locals` admission the narrow
                // arm carries). Falling out of the arm without `continue`
                // reaches the decoded handler with `pc` untouched.
                0xc4 => {
                    if b1 == 0x84 && saved_pc + 5 < code_len {
                        // SAFETY: saved_pc + 5 < code_len, checked above.
                        let (i_lo, c_hi, c_lo) = unsafe {
                            (
                                *code_ptr.add(saved_pc + 3),
                                *code_ptr.add(saved_pc + 4),
                                *code_ptr.add(saved_pc + 5),
                            )
                        };
                        let idx = ((b2 as usize) << 8) | (i_lo as usize); // Cast: bytecode operand decoding
                        if idx < frame.max_locals as usize {
                            let inc = (((c_hi as u16) << 8) | (c_lo as u16)) as i16 as i32; // Cast: bytecode operand decoding
                            let v = frame.get_local_int_unchecked(idx);
                            frame.set_local_int_unchecked(idx, v.wrapping_add(inc));
                            frame.pc = saved_pc + 6;
                            continue;
                        }
                    }
                }
                // goto
                0xa7 => {
                    goto_arm!(frame, saved_pc, b1, b2);
                }
                // if_icmpge — AUDIT CRIT-4: int-typed pop, no Value enum match.
                0xa2 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va >= vb);
                }
                // if_icmplt
                0xa1 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va < vb);
                }
                // if_icmple
                0xa4 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va <= vb);
                }
                // if_icmpgt
                0xa3 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va > vb);
                }
                // if_icmpne
                0xa0 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va != vb);
                }
                // if_icmpeq
                0x9f => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, va == vb);
                }
                // ireturn / lreturn / freturn / dreturn / areturn
                0xac..=0xb0 => {
                    // JVMS §2.11.10 structured locking (wave 23, lane L7): a
                    // return with a block monitor still recorded throws
                    // `IllegalMonitorStateException` at this bci, before the
                    // value leaves the stack and before `MethodExit`, as
                    // HotSpot's `remove_activation` does; the unwind then
                    // releases the monitors. When every entry proves stale the
                    // record is now empty and the same return re-dispatches
                    // (`pc` is still `saved_pc`). One `is_empty` test when the
                    // frame took no block monitor.
                    if !frame.held_monitors.is_empty() {
                        let _ = frame;
                        match held_monitors::return_with_held_monitors(shared, thread, frame_idx) {
                            Some(e) => route_fast_path_error!(e, saved_pc),
                            None => continue,
                        }
                    }
                    // A `synchronized` frame releases its method monitor here,
                    // before `MethodExit`, and throws at this bci when the
                    // thread no longer owns it (t1 wave 3, lane K;
                    // `CRATONVM_MONITOREXIT_METHOD_LOCK_PARITY`). One field test
                    // for any other frame (the pop reads the same field).
                    let frame = if frame.monitor_on_exit.is_some()
                    {
                        if let Some(e) =
                            held_monitors::release_method_monitor_at_return(shared, thread, frame_idx)
                        {
                            route_fast_path_error!(e, saved_pc);
                        }
                        &mut thread.frames[frame_idx]
                    } else {
                        frame
                    };
                    // Calibration: two adjacent reads measuring nothing, so the
                    // other phases can be corrected for rdtsc latency instead of
                    // compared against an assumed one.
                    //
                    // Every instrument read in this arm is behind the hoisted
                    // `invoke_phases_on`: `now()` and `charge()` each re-test
                    // the gate themselves, which made an unarmed return pay
                    // seven gate loads for an instrument nobody enabled.
                    if diag_gates & DG_INVOKE_PHASES != 0 {
                        let c0 = invoke_phases::now();
                        let c1 = invoke_phases::now();
                        invoke_phases::charge(invoke_phases::P_CALIB, c0, c1);
                    }
                    let ph_r0 = if diag_gates & DG_INVOKE_PHASES != 0 {
                        invoke_phases::now()
                    } else {
                        0
                    };
                    // Bit-exact return-value transfer. The prior
                    // `pop_unchecked()` + `push_unchecked()` round-tripped the
                    // slot through `to_value()`/`from_value()`, which decoded a
                    // category-2 long whose NaN-box bit pattern collides with a
                    // tagged sub-tag (e.g. `lreturn` of `0xFFFC_…`, a BC safegcd
                    // `Mod.updateDE30`/`updateFG30` accumulator) as `Value::Int`,
                    // dropping the high bits. Copy the raw CompactValue for
                    // i/l/f/d-return; areturn still normalizes jobject-as-Long
                    // handles via `coerce_value_for_return`. See
                    // bc-ec-mod-mododdinverse-investigation.md.
                    //
                    // Underflow guard: an empty operand stack at a value
                    // return means earlier execution desynced the stack
                    // (valid bytecode can't reach here empty). The unchecked
                    // pop would wrap `len` to usize::MAX and PANIC the whole
                    // VM (observed: Gradle ProjectBuilder classes in the
                    // Spring Boot buildSrc suite; kafka bug-03 is the same
                    // family). Surface a diagnosable error instead.
                    // Out of line, with `saved_pc` and `opcode` passed BY
                    // VALUE (wave 27): see `value_return_underflow`.
                    if frame.stack.is_empty() {
                        return Err(value_return_underflow(shared, frame, saved_pc, opcode));
                    }
                    let (mut cv, kind) = frame.stack.pop_with_kind_unchecked();
                    // JVMS §6.5 `ireturn`: in a method declared to return
                    // `boolean`, `byte`, `char` or `short`, the int is narrowed
                    // (`& 1`, i2b, i2c, i2s) before it reaches the invoker. The
                    // baseline JIT already does this (`emit_narrow_int_return`),
                    // so without it a `()Z` body returning 2 answered 2 while
                    // interpreted and 0 once compiled. javac never emits an
                    // un-narrowed value; generated bytecode can.
                    if opcode == 0xac {
                        let ret = frame.return_tag();
                        if matches!(ret, b'Z' | b'B' | b'C' | b'S') {
                            if let Some(v) = cv.as_int() {
                                cv = CompactValue::int(narrow_ireturn(v, ret));
                            }
                        }
                    }
                    let desc_byte = match opcode {
                        0xad => b'J', // lreturn
                        0xae => b'F', // freturn
                        0xaf => b'D', // dreturn
                        0xb0 => b'L', // areturn
                        _ => b'I',    // ireturn (also B/C/S/Z)
                    };
                    // areturn (0xb0): mirror slow-path `Instruction::Areturn` — JNI
                    // / bridges can leave a jobject as `Value::Long` on the stack;
                    // fast-path frames (non-JDK packages) must still normalize
                    // before pushing to the caller or returning from the outer VM.
                    // lreturn (0xad): a KIND_LONG slot is read bit-exact so a
                    // collision-shaped long return keeps its high bits.
                    //
                    // A plain reference passes through as a reference
                    // (`ref_operand_value`): the context-free `to_value` +
                    // coerce this used returned NULL for an `Object`-tagged
                    // payload with no recorded provenance. Other shapes are
                    // decoded kind-aware, as the decoded `Areturn` arm's
                    // `pop()` decodes them.
                    let value = if opcode == 0xb0 {
                        let ret = frame.return_tag();
                        if matches!(ret, b'L' | b'[') {
                            ref_operand_value(shared, cv, kind)
                        } else {
                            coerce_value_for_return_validated(shared, cv.to_value(), ret)
                        }
                    } else {
                        decode_arg_kind_aware(cv, kind, desc_byte)
                    };
                    // `CRATONVM_TRACE_SB_FILTER`, out of line and handed the
                    // value BY VALUE (wave 27): see `trace_sb_filter_return`.
                    if diag_gates & DG_TRACE_SB_FILTER != 0 {
                        trace_sb_filter_return(shared, frame, value);
                    }
                    // Stage 2 of the contiguous interpreter stack (argument
                    // overlap, wave 37 lane L7): an overlapped callee's first
                    // slot is where the push into the parent below lands, so
                    // the callee gives its slots up first and no slot is a
                    // root of two frames across the callbacks
                    // (`Frame::release_caller_overlap`). One bit test of the
                    // frame header when the switch is off.
                    if frame.locals_overlap_caller() {
                        frame.release_caller_overlap();
                        invoke_phases::note_overlap_released();
                    }
                    // Keep the raw-byte and decoded return handlers
                    // observationally identical for JVMTI. The package gate
                    // previously hid this missing MethodExit event for most
                    // JDK/Spring frames.
                    let _ = frame;
                    if frame_idx > initial_frame_idx {
                        // Stackless return: push the value to the parent
                        // FIRST, then fire MethodExit and pop the child.
                        //
                        // Wave 4: the value used to live only in `value`/`cv`
                        // across the MethodExit callback, the synchronized
                        // monitor release and the FramePop callback, none of
                        // which scans a Rust local — a collection an agent
                        // triggered there relocated or reclaimed a returned
                        // reference, and the push below handed the caller a
                        // stale address. In the parent's operand stack it is
                        // an ordinary root. The parent is not the top frame
                        // while the callbacks run, and the root-snapshot cache
                        // reuses a non-top frame's roots while its
                        // `(seq, exec_epoch)` is unchanged, so its epoch is
                        // bumped before the push (`pop_and_recycle_frame`
                        // bumps it again, after the callbacks — too late).
                        // Operand stacks are not part of JVMTI's frame view,
                        // so the earlier push is not observable to an agent.
                        // A remounted continuation may owe this return a
                        // conversion its lambda door would have made (wave 25,
                        // lane L7): a test of a hoisted bool on a platform
                        // thread, one `is_empty` test on a virtual one
                        // (`thread_is_virtual`, wave 26).
                        if diag_gates & DG_THREAD_IS_VIRTUAL == 0
                            || thread.continuation_return_adapters.is_empty()
                        {
                            let parent = &mut thread.frames[frame_idx - 1];
                            parent.exec_epoch = parent.exec_epoch.wrapping_add(1);
                            push_fast_return_value(&mut parent.stack, opcode, cv, value);
                        } else if let Err(e) = push_return_across_continuation_boundary(
                            shared,
                            thread,
                            frame_idx,
                            value,
                            Some((opcode, cv)),
                        ) {
                            route_fast_path_error!(e, saved_pc);
                        }
                        // The listener test first (wave 27, lane L7): the
                        // callee tests the same flag, but `&Some(value)`,
                        // an argument, was built in memory before the call on
                        // every return; now only when an agent listens.
                        if crate::runtime::jvmti::any_method_exit_listener_active() {
                            fire_jvmti_method_exit_normal(
                                shared.vm_identity,
                                thread,
                                frame_idx,
                                &Some(value),
                            );
                        }
                        if diag_gates & DG_INVOKE_PHASES != 0 {
                            let ph_r1 = invoke_phases::now();
                            pop_and_recycle_frame(shared, thread);
                            let ph_r2 = invoke_phases::now();
                            invoke_phases::charge(invoke_phases::P_RET_RECYCLE, ph_r1, ph_r2);
                        } else {
                            pop_and_recycle_frame_gated(
                                shared,
                                thread,
                                || diag_gates & DG_LOOP_WORK != 0,
                                || diag_gates & DG_RETURN_DIAGNOSTICS != 0,
                            );
                        }
                        frame_idx -= 1;
                        if diag_gates & DG_INVOKE_PHASES != 0 {
                            invoke_phases::charge(
                                invoke_phases::P_RET_TOTAL,
                                ph_r0,
                                invoke_phases::now(),
                            );
                        }
                        continue;
                    }
                    // Outermost return: no parent frame to hold the value, so
                    // MethodExit runs with it pinned (the caller of
                    // `execute_frame_from_index` pins it across the pop).
                    let mut return_value = Some(value);
                    fire_method_exit_keeping_value(shared, thread, frame_idx, &mut return_value);
                    return Ok(return_value);
                }
                // return (void)
                0xb1 => {
                    // JIT round 21 wave 16 (lane finalize-reinit,
                    // `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`): a
                    // constructor of a finalizable receiver reached its
                    // `return`, so the receiver's `Object.<init>` returned
                    // (HotSpot's registration point). One gate bit per
                    // return; past it, a length and one byte of the name for
                    // a non-constructor (`finalizer_init_returned`). First in
                    // the arm: a monitor-exit failure below cannot undo an
                    // `Object.<init>` return.
                    if diag_gates & DG_FINALIZER_INIT_RETURN != 0 {
                        finalizer_init_returned(shared, frame, saved_pc);
                    }
                    // Structured locking: see the value-return arm above.
                    if !frame.held_monitors.is_empty() {
                        let _ = frame;
                        match held_monitors::return_with_held_monitors(shared, thread, frame_idx) {
                            Some(e) => route_fast_path_error!(e, saved_pc),
                            None => continue,
                        }
                    }
                    // The method monitor: see the value-return arm above.
                    if frame.monitor_on_exit.is_some() {
                        let _ = frame;
                        if let Some(e) =
                            held_monitors::release_method_monitor_at_return(shared, thread, frame_idx)
                        {
                            route_fast_path_error!(e, saved_pc);
                        }
                    }
                    fire_jvmti_method_exit_normal(
                        shared.vm_identity,
                        thread,
                        frame_idx,
                        &None,
                    );
                    if frame_idx > initial_frame_idx {
                        // Interpreter round i6 wave 115, lane L4: a remounted
                        // `void` leaf may owe its method-handle door's `null`
                        // (`CRATONVM_MH_VOID_TAIL_UNMOUNT`). The value-return
                        // arms' test: a hoisted bit on a platform thread.
                        if diag_gates & DG_THREAD_IS_VIRTUAL != 0
                            && !thread.continuation_return_adapters.is_empty()
                        {
                            if let Err(e) =
                                push_void_return_across_continuation_boundary(thread, frame_idx)
                            {
                                route_fast_path_error!(e, saved_pc);
                            }
                        }
                        pop_and_recycle_frame_gated(
                            shared,
                            thread,
                            || diag_gates & DG_LOOP_WORK != 0,
                            || diag_gates & DG_RETURN_DIAGNOSTICS != 0,
                        );
                        frame_idx -= 1;
                        continue;
                    }
                    return Ok(None);
                }
                // ifle — AUDIT CRIT-4
                0x9e => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val <= 0);
                }
                // ifge
                0x9c => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val >= 0);
                }
                // ifgt
                0x9d => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val > 0);
                }
                // iflt
                0x9b => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val < 0);
                }
                // ifne
                0x9a => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val != 0);
                }
                // ifeq
                0x99 => {
                    let val = frame.stack.pop_int_unchecked();
                    cond_branch_arm!(frame, saved_pc, b1, b2, val == 0);
                }
                // i2l — AUDIT CRIT-4: int-typed pop avoids the 8-arm Value match.
                0x85 => {
                    // i2l; ladd | lsub | lmul | land | lor | lxor | lcmp [;
                    // lstore_Z | lstore n] over the stack's long and int
                    // (interpreter round i5 wave 106, lane L7,
                    // `CRATONVM_INTERP_FUSED_I2L_LONG_TAIL`, default off):
                    // javac's `sum += <int expression>` with a long `sum`,
                    // the `i2l -> ladd` / `ladd -> lstore_1` pairs of the
                    // wave-88 census that the local-operand groups do not
                    // reach. One range test of `b1` and the gate bit; the
                    // decode and every decline are the out-of-line helper's,
                    // and a decline has popped nothing.
                    if matches!(b1, 0x61 | 0x65 | 0x69 | 0x7f | 0x81 | 0x83 | 0x94)
                        && door_gates & DOOR_FUSED_I2L_LONG_TAIL != 0
                        && !fusion_off
                    {
                        if let Some(next_pc) = fused_i2l_long_tail(frame, code_len, saved_pc + 1)
                        {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2l sign-extends int → long (lossless).
                    // Direct CompactValue push avoids any Value → CompactValue
                    // boundary tagging drift on long slots.
                    frame.stack.push_long_unchecked(i64::from(val));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ladd — WP4.3: use as_long_unchecked to bypass tag-erasure.
                // `pop_unchecked()` decodes a CompactValue::long(N) (untagged
                // raw bits) as `Value::Double(<denormal>)`, so the
                // `Value::Long` pattern match below was dead code — every
                // long add was a no-op, leaving sums stuck at 0.
                0x61 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_add(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lsub
                0x65 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_sub(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lmul
                0x69 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va.wrapping_mul(vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lload_0..3 — route raw CompactValue bits straight through
                // (bit-exact, mirroring the slow-path `Lload` and fast-path
                // `lstore_N`). The prior `get_local_unchecked().to_value()` +
                // `push_unchecked()` round-trip decoded a collision-pattern
                // long (e.g. `0xFFFC_….` whose NaN sub-tag reads as Int) to
                // `Value::Int`, dropping bits 32-46 on re-encode. push_compact
                // preserves every bit and skips the Value decode/encode.
                //
                // Wave 82 (lane L7): the arm first screens for the long array
                // accumulate (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`,
                // `VmConfig::interp_fused_array_accumulate_long`):
                // `lload_S; aload_N; iload_M; laload; l<op>; lstore_Z` and the
                // widening `... <x>aload; i2l; l<op>; lstore_Z`. Two range
                // tests of the pre-read bytes, then the admission bit and
                // `fusion_off`; the decode, the kind-mark test, the array
                // checks and every decline are the out-of-line helper's. A
                // decline has written nothing and pushed nothing, so the
                // plain `lload_N` runs.
                //
                // Host rework (wave 83): wave 82 expanded that screen (and
                // its call) into each of four `lload_N` arms and read the
                // switch through `shared.config` in each, and the loop frame
                // grew 16 bytes. Now ONE arm serves `lload_0..3` (the local
                // index is `opcode - 0x1e`, one subtraction) with one screen
                // and one call site, and the switch is bit 12 of the hoisted
                // `static_ldc_gates` word (`SLG_FUSED_ARRAY_ACCUMULATE_LONG`,
                // with the ZGC admission folded in at entry).
                0x1e..=0x21 => {
                    if matches!(b1, 0x2a..=0x2d)
                        && matches!(b2, 0x1a..=0x1d)
                        && static_ldc_gates & SLG_FUSED_ARRAY_ACCUMULATE_LONG != 0
                        && !fusion_off
                    {
                        if let Some(zgc) = fast_field_zgc {
                            if let Some(next_pc) = field_fast::fused_array_accumulate_long(
                                shared, zgc, frame, code_len, saved_pc,
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    let local = usize::from(opcode - 0x1e);
                    // lload_X; {lload_Y | lload n | lconst_0 | lconst_1};
                    // ladd | lsub | lmul | land | lor | lxor [; lstore] and
                    // the same with lcmp (interpreter round i5 wave 101, lane
                    // L7, `CRATONVM_INTERP_FUSED_LONG_ARITH`, default off).
                    // One range test of `b1` for every other `lload_N`; the
                    // decode and every decline are the out-of-line helper's,
                    // and a decline has written nothing.
                    // Wave 105 (`CRATONVM_INTERP_FUSED_LONG_INT_OPERAND`,
                    // default off): the operand may be `{iload_Y | iload n};
                    // i2l`; with the switch off an `iload` operand costs one
                    // more range test and the gate bit.
                    if (matches!(b1, 0x09 | 0x0a | 0x16 | 0x1e..=0x21)
                        || (matches!(b1, 0x15 | 0x1a..=0x1d)
                            && door_gates & DOOR_FUSED_LONG_INT_OPERAND != 0))
                        && door_gates & DOOR_FUSED_LONG_ARITH != 0
                        && !fusion_off
                    {
                        if let Some(next_pc) = fused_long_arith(
                            frame,
                            code_len,
                            saved_pc + 1,
                            local,
                            door_gates & DOOR_FUSED_LONG_INT_OPERAND != 0,
                        ) {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    frame
                        .stack
                        .push_compact_long(frame.get_local_compact_unchecked(local));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lstore_0..3 — WP4.3: route through CompactValue directly so
                // an untagged long bit-pattern keeps its raw bits without
                // detouring via `Value::Double`. The parent slow-path `Lstore`
                // already uses the typed `pop_long`, so this fast-path
                // mirrors that for parity.
                0x3f => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(0, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x40 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(1, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x41 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(2, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x42 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Long(cv.as_long_unchecked());
                    frame.set_local_unchecked(3, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lconst_0, lconst_1
                0x09 => {
                    // Direct CompactValue push — Long is an 8-byte slot on
                    // CompactValue so no category-2 double push is required.
                    frame.stack.push_long_unchecked(0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0a => {
                    frame.stack.push_long_unchecked(1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lcmp — WP4.3: bypass tag-erasure via as_long_unchecked.
                0x94 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    let r = if va > vb {
                        1
                    } else if va < vb {
                        -1
                    } else {
                        0
                    };
                    frame.stack.push_int_unchecked(r);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iand (0x7e) — AUDIT CRIT-4
                0x7e => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va & vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ior (0x80)
                0x80 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va | vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ixor (0x82)
                0x82 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(va ^ vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ishl (0x78) — JVM spec: shift amount masked to 5 bits
                0x78 => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(va.wrapping_shl(vb as u32 & 0x1f));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ishr (0x7a)
                0x7a => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(va.wrapping_shr(vb as u32 & 0x1f));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iushr (0x7c) — unsigned shift right
                0x7c => {
                    let vb = frame.stack.pop_int_unchecked();
                    let va = frame.stack.pop_int_unchecked();
                    frame
                        .stack
                        // Widening: smaller value -> u32 (value fits)
                        .push_int_unchecked(((va as u32).wrapping_shr(vb as u32 & 0x1f)) as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ineg (0x74)
                0x74 => {
                    let val = frame.stack.pop_int_unchecked();
                    frame.stack.push_int_unchecked(val.wrapping_neg());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lneg (0x75) — WP4.3 tag-erasure bypass.
                0x75 => {
                    let cv = frame.stack.pop_compact();
                    let v = cv.as_long_unchecked();
                    frame.stack.push_long_unchecked(v.wrapping_neg());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // land (0x7f)
                0x7f => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va & vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lor (0x81)
                0x81 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va | vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // lxor (0x83)
                0x83 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    frame.stack.push_long_unchecked(va ^ vb);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ldiv (0x6d)
                0x6d => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    if vb != 0 {
                        frame.stack.push_long_unchecked(va.wrapping_div(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Re-push so the slow path can throw ArithmeticException.
                    // `push_compact_long`, not `push_compact`: both operands are
                    // longs, and `push_compact` would re-mark them KIND_UNKNOWN,
                    // so the decoded arm's `pop_long` would read a collision-
                    // shaped dividend through the tag instead of bit-exact.
                    frame.stack.push_compact_long(cva);
                    frame.stack.push_compact_long(cvb);
                }
                // lrem (0x71)
                0x71 => {
                    let cvb = frame.stack.pop_compact();
                    let cva = frame.stack.pop_compact();
                    let va = cva.as_long_unchecked();
                    let vb = cvb.as_long_unchecked();
                    if vb != 0 {
                        frame.stack.push_long_unchecked(va.wrapping_rem(vb));
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Same kind-preserving restore as `ldiv`.
                    frame.stack.push_compact_long(cva);
                    frame.stack.push_compact_long(cvb);
                }
                // fload_0..3 (0x22-0x25) — raw slot copy, like `iload_N`. The
                // former `get_local_unchecked` + `push_unchecked` decoded the
                // slot to `Value::Float` and re-encoded the identical bits.
                0x22 => {
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(0));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x23 => {
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(1));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x24 => {
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(2));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x25 => {
                    frame
                        .stack
                        .push_compact(frame.get_local_compact_unchecked(3));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fstore_0..3 (0x43-0x46) — raw slot copy, like `istore_N`.
                // `set_local_compact_unchecked` writes the same `LKIND_OTHER`
                // mark `set_local_unchecked(Value::Float)` did.
                0x43 => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(0, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x44 => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(1, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x45 => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(2, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x46 => {
                    let cv = frame.stack.pop_compact();
                    frame.set_local_compact_unchecked(3, cv);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dload_0..3 (0x26-0x29) — bit-exact and KIND_DOUBLE-marked,
                // exactly as the wide-index `dload` (0x18) arm below.
                //
                // These went through `get_local_unchecked`, which honours the
                // local's `LKIND_DOUBLE` mark but otherwise decodes by TAG — so
                // a double whose NaN payload collides with the tag space, held
                // in a local that did not carry the mark (a local written by
                // `set_local_compact*`, which never sets `LKIND_DOUBLE`), came
                // back as `Value::Int` / `Value::Object` and was pushed as that
                // type. The `dload 4` spelling of the same load was exact; the
                // `dload_3` spelling was not.
                //
                // One arm for the four (the local is `opcode - 0x26`), with
                // the double arithmetic group screen: `dload_X; {dload_Y |
                // dload n | dconst_0 | dconst_1}; dadd | dsub | dmul | ddiv |
                // drem [; dstore]` and `...; dcmpl | dcmpg` (interpreter
                // round i5 wave 102, lane L7,
                // `CRATONVM_INTERP_FUSED_DOUBLE_ARITH`, default ON, `=0` off). One
                // range test of `b1`; the decode and every decline are the
                // out-of-line helper's, and a decline has written nothing.
                0x26..=0x29 => {
                    let local = usize::from(opcode - 0x26);
                    if matches!(b1, 0x0e | 0x0f | 0x18 | 0x26..=0x29)
                        && door_gates & DOOR_FUSED_DOUBLE_ARITH != 0
                        && !fusion_off
                    {
                        if let Some(next_pc) =
                            fused_double_arith(frame, code_len, saved_pc + 1, local)
                        {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    frame
                        .stack
                        .push_compact_double(frame.get_local_compact_unchecked(local));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dstore_0..3 (0x47-0x4a) — WP4.3 typed-pop routing so an
                // untagged double bit-pattern lands as `Value::Double`.
                0x47 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(0, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x48 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(1, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x49 => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(2, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x4a => {
                    let cv = frame.stack.pop_compact();
                    let v = Value::Double(f64::from_bits(cv.to_bits()));
                    frame.set_local_unchecked(3, v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fadd (0x62) — Round-3: typed float helpers, no Value enum.
                //
                // Verified bytecode guarantees the top two slots are Float-tagged
                // here.  IEEE 754 semantics (NaN propagation, ±Inf on divide-by-
                // zero) are preserved by the underlying `f32` arithmetic.
                0x62 => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(java_fadd(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fsub (0x66)
                0x66 => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(java_fsub(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fmul (0x6a)
                0x6a => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(java_fmul(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fdiv (0x6e) — JVMS: f / 0.0 = ±Inf / NaN (no exception).
                0x6e => {
                    let b = frame.stack.pop_float_unchecked();
                    let a = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(java_fdiv(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dadd (0x63) — Round-3: typed double helpers, no Value enum.
                //
                // Verified bytecode guarantees Double operands. IEEE 754
                // semantics preserved by the underlying `f64` arithmetic.
                0x63 => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(java_dadd(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dsub (0x67)
                0x67 => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(java_dsub(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dmul (0x6b)
                0x6b => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(java_dmul(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // ddiv (0x6f) — JVMS: d / 0.0 = ±Inf / NaN (no exception).
                0x6f => {
                    let b = frame.stack.pop_double_unchecked();
                    let a = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(java_ddiv(a, b));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // i2b (0x91), i2c (0x92), i2s (0x93) — AUDIT CRIT-4
                0x91 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2b narrows int to byte via sign-extension
                    frame.stack.push_int_unchecked(val as i8 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x92 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2c narrows int to char (unsigned 16-bit)
                    frame.stack.push_int_unchecked(val as u16 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x93 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2s narrows int to short via sign-extension
                    frame.stack.push_int_unchecked(val as i16 as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // i2f (0x86), i2d (0x87) — AUDIT CRIT-4 pop side
                0x86 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2f converts int to float (may lose precision)
                    frame.stack.push_float_unchecked(val as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x87 => {
                    let val = frame.stack.pop_int_unchecked();
                    // JVM spec: i2d widens int to double (lossless).
                    // Direct CompactValue push keeps the 8-byte slot
                    // tagged with the Double NaN-box encoding.
                    frame.stack.push_double_unchecked(f64::from(val));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // l2i (0x88) — WP4.3 tag-erasure bypass.
                0x88 => {
                    let cv = frame.stack.pop_compact();
                    let val = cv.as_long_unchecked();
                    // JVM spec: l2i narrows long to int (truncates upper 32 bits)
                    frame.stack.push_int_unchecked(val as i32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // swap (0x5f) — kind-preserving slot moves, the same shape as
                // the decoded `Instruction::Swap` arm and the `dup_x*` arms
                // below, instead of two `Value` decode/encode round trips.
                0x5f => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    frame.stack.push_with_kind_unchecked(v1, k1);
                    frame.stack.push_with_kind_unchecked(v2, k2);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // fconst_0..2 (0x0b-0x0d) — direct CompactValue float push (no Value enum encode).
                0x0b => {
                    frame.stack.push_float_unchecked(0.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0c => {
                    frame.stack.push_float_unchecked(1.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0d => {
                    frame.stack.push_float_unchecked(2.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dconst_0, dconst_1 (0x0e-0x0f)
                0x0e => {
                    // Direct CompactValue push — double is an 8-byte slot.
                    frame.stack.push_double_unchecked(0.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x0f => {
                    frame.stack.push_double_unchecked(1.0);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // nop (0x00)
                0x00 => {
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // dup — slot bits and kind mark copied verbatim, like the six
                // other shuffles below and the decoded `Instruction::Dup` arm.
                //
                // This was `peek()` + `push_unchecked(Value)`: a full decode to
                // the 16-byte `Value` and a re-encode on one of the most
                // frequent opcodes there is (every `new; dup; invokespecial`).
                // A marked long/double slot keeps its mark via
                // `push_with_kind_unchecked`; every other slot (references
                // included) goes through `push_compact`, which writes the same
                // KIND_UNKNOWN mark the original carries AND keeps the
                // `DEADREF`/vacated-frame push instruments seeing `dup` — the
                // `push_compact` doc names `dup` as a path they must cover.
                0x59 => {
                    let (cv, kind) = frame.stack.peek_with_kind_unchecked();
                    if kind == crate::runtime::ValueStack::KIND_MARK_LONG
                        || kind == crate::runtime::ValueStack::KIND_MARK_DOUBLE
                    {
                        frame.stack.push_with_kind_unchecked(cv, kind);
                    } else {
                        frame.stack.push_compact(cv);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // pop — discard the top slot without decoding (no Value enum round-trip).
                0x57 => {
                    frame.stack.pop_compact();
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // aconst_null — push a CompactValue::null() directly (no Value enum encode).
                0x01 => {
                    frame.stack.push_compact(CompactValue::null());
                    frame.pc = saved_pc + 1;
                    continue;
                }
                // iload (0x15), fload (0x17), aload (0x19)
                //
                // H7: `b1` is the raw bytecode operand. The `_unchecked`
                // local accessors index `frame.locals` (len == max_locals)
                // without a bounds check, relying on the verifier. Guard the
                // index here as defence-in-depth for unverified bytecode
                // (per-class `skip_verification` classes not covered by the
                // global `use_fast_path` gate); on an out-of-range operand we
                // skip the unchecked arm and fall through to the
                // bounds-checked slow path (which re-decodes from `saved_pc`,
                // unchanged here).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                //
                // `iload n` / `fload n` copy the raw slot exactly as their
                // `iload_N` / `fload_N` spellings do (they used to decode to
                // `Value` and re-encode the same bits), and `iload n` takes the
                // same compare-and-branch fusion: javac numbers a loop's
                // counter and bound past local 3 in any method with a receiver
                // and two parameters.
                // Cast: the u8 local-index operand and the u16 `max_locals` both widen to usize losslessly.
                0x15 | 0x17 if (b1 as usize) < frame.max_locals as usize => {
                    let local_idx = b1 as usize; // Cast: bytecode operand decoding
                    let cv = frame.get_local_compact_unchecked(local_idx);
                    if opcode == 0x15 && !fusion_off && saved_pc + 2 < code_len {
                        if let Some(vx) = cv.as_int() {
                            // `saved_pc + 2 < code_len` is the test above.
                            fused_icmp_arm!(frame, code_ptr, code_len, vx, saved_pc + 2);
                            // iload n; <x>aload (wave 25, lane L7): the
                            // `iload_N` arm's array-on-the-stack element read,
                            // for an index past local 3 -- the matrix
                            // kernel's `aaload; iload k; iaload`.
                            if matches!(b2, 0x2e..=0x35) {
                                if let Some(zgc) = fast_field_zgc {
                                    if field_fast::array_load_top_with_index(
                                        zgc,
                                        &mut frame.stack,
                                        vx,
                                        b2,
                                        ref_array_fast_on,
                                        site_census,
                                    ) {
                                        frame.pc = saved_pc + 3;
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    frame.stack.push_compact(cv);
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // aload n (0x19) — `load_ref_local`, the implementation
                // `aload_N` shares: an `Object`- or `Null`-tagged slot is pushed
                // verbatim, every other shape through the validated coerce. See
                // docs/feature-designs/real-jdk-mode.md.
                // Cast: the u8 local-index operand and the u16 `max_locals` both widen to usize losslessly.
                0x19 if (b1 as usize) < frame.max_locals as usize => {
                    // aload n; arraylength (wave 5): the `aload_N` pair's
                    // fusion for a reference local past 3, same rules.
                    if b2 == 0xbe && arraylength_fusion_on && !fusion_off && saved_pc + 2 < code_len
                    {
                        // Cast: bytecode operand decoding
                        let local = frame.get_local_compact_unchecked(b1 as usize);
                        if let Some(len) = field_fast::arraylength_of_local(local, site_census) {
                            frame.stack.push_int_unchecked(len);
                            frame.pc = saved_pc + 3;
                            continue;
                        }
                    }
                    // aload n; iload{_M, n}; <x>aload (round i3 wave 75, lane
                    // L7, `CRATONVM_INTERP_FUSED_WIDE_LOCALS`): `s += b[i]`
                    // with the array past local 3. The `aload_N` arm's
                    // wide-index helper; a decline pushes nothing.
                    if matches!(b2, 0x15 | 0x1a..=0x1d)
                        && !fusion_off
                        && shared.config.interp_fused_wide_locals
                    {
                        // The same six-argument entry as the `aload_N` arm's
                        // (wave 76, lane L7).
                        if let Some(next_pc) = field_fast::array_load_wide_locals_at(
                            shared,
                            fast_field_zgc,
                            frame,
                            code_len,
                            saved_pc + 2,
                            b1 as usize, // Cast: bytecode operand decoding
                        ) {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    load_ref_local(shared, frame, b1 as usize); // Cast: bytecode operand decoding
                    // aload n; ifnull | ifnonnull (interpreter round i6 wave
                    // 116, lane L8, `CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE`,
                    // default off): the `aload_N` arm's null-branch fusion
                    // for a reference past local 3, same rules (the
                    // `ifnull` arm's own test, `cond_branch_arm!` at the
                    // branch's own pc, `saved_pc + 2`). Off: one range test
                    // of `b2`.
                    if matches!(b2, 0xc6 | 0xc7)
                        && door_gates & DOOR_FUSED_NULL_BRANCH_WIDE != 0
                        && !fusion_off
                        && saved_pc + 4 < code_len
                    {
                        let is_null = pop_ref_operand_is_null(&mut frame.stack);
                        let br_pc = saved_pc + 2;
                        // SAFETY: saved_pc + 4 < code_len, checked above.
                        let (ob1, ob2) =
                            unsafe { (*code_ptr.add(saved_pc + 3), *code_ptr.add(saved_pc + 4)) };
                        // The state a back-edge safepoint in the unfused
                        // pair would see.
                        frame.last_instr_pc = br_pc;
                        cond_branch_arm!(frame, br_pc, ob1, ob2, is_null == (b2 == 0xc6));
                    }
                                                                // aload n; getfield (round i3 wave 83, lane L7,
                                                                // `CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD`, default
                                                                // off; `i72-L7-proposal-wide-local-forms-of-the-fused-groups`):
                                                                // the `aload_N; getfield` fusion for a receiver past local
                                                                // 3, same rules (the receiver just pushed, a miss leaves
                                                                // it there and the `getfield` runs at its own pc), through
                                                                // the quickened read OUT of line
                                                                // (`field_fast::wide_local_getfield`).
                                                                //
                                                                // Host rework (wave 83): the first version read the
                                                                // switch, the ZGC admission, two index bytes, the census,
                                                                // the field-phase bit and the hoisted watch here and made
                                                                // a seven-argument call, and the loop frame grew 32
                                                                // bytes. Now the arm tests one byte and one bit of the
                                                                // hoisted `static_ldc_gates` word
                                                                // (`SLG_FUSED_WIDE_LOCAL_GETFIELD`: the switch and the
                                                                // ZGC admission, folded at entry) and makes a
                                                                // five-argument call with values it already holds; the
                                                                // helper derives everything else from `shared` and the
                                                                // frame. Off: the byte compare and the bit test.
                    if b2 == 0xb4
                        && static_ldc_gates & SLG_FUSED_WIDE_LOCAL_GETFIELD != 0
                        && !fusion_off
                    {
                        if let Some(next_pc) = field_fast::wide_local_getfield(
                            shared,
                            &mut thread.site_tables.fast_field_sites,
                            frame,
                            code_len,
                            saved_pc,
                        ) {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // lload (0x16), dload (0x18) — bit-exact CompactValue pass
                // through, matching the slow-path `Lload`/`Dload` arms
                // (`get_local_compact` + `push_compact_checked`). Avoids the
                // lossy `to_value()`/`from_value()` round-trip that dropped
                // collision-pattern longs (see lload_0..3 above).
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x16 | 0x18 if (b1 as usize) < frame.max_locals as usize => {
                    // `lload n` as the first operand of the long group
                    // (`CRATONVM_INTERP_FUSED_LONG_ARITH`, see `lload_0..3`).
                    if opcode == 0x16
                        && (matches!(b2, 0x09 | 0x0a | 0x16 | 0x1e..=0x21)
                            || (matches!(b2, 0x15 | 0x1a..=0x1d)
                                && door_gates & DOOR_FUSED_LONG_INT_OPERAND != 0))
                        && door_gates & DOOR_FUSED_LONG_ARITH != 0
                        && !fusion_off
                    {
                        if let Some(next_pc) = fused_long_arith(
                            frame,
                            code_len,
                            saved_pc + 2,
                            usize::from(b1),
                            door_gates & DOOR_FUSED_LONG_INT_OPERAND != 0,
                        ) {
                            frame.pc = next_pc;
                            continue;
                        }
                    }
                    let cv = frame.get_local_compact_unchecked(b1 as usize); // Cast: bytecode operand decoding
                                                                             // Mark the slot's category so a NaN-tag-colliding long is
                                                                             // popped bit-exact (lload) rather than sign-extended.
                    if opcode == 0x16 {
                        frame.stack.push_compact_long(cv);
                    } else {
                        frame.stack.push_compact_double(cv);
                    }
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // istore (0x36), fstore (0x38)
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                // Raw slot copy, as `istore_N` / `fstore_N`.
                0x36 | 0x38 if (b1 as usize) < frame.max_locals as usize => {
                    let cv = frame.stack.pop_compact();
                    // Cast: bytecode operand decoding
                    frame.set_local_compact_unchecked(b1 as usize, cv);
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // astore (0x3a) — reference local; coerce jlong jobject handles.
                // Validated to reject aligned non-heap long bits (Letsgo AV).
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                //
                // `store_ref_local`, the implementation `astore_N` shares.
                0x3a if (b1 as usize) < frame.max_locals as usize => {
                    store_ref_local(shared, frame, b1 as usize); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // lstore (0x37), dstore (0x39) — WP4.3 typed-pop routing.
                // H7: bounds-guard `b1` (see iload/fload/aload above).
                // Widening: small unsigned (u8/u16/i32 index) -> usize (non-negative, fits)
                0x37 | 0x39 if (b1 as usize) < frame.max_locals as usize => {
                    let cv = frame.stack.pop_compact();
                    let v = if opcode == 0x37 {
                        Value::Long(cv.as_long_unchecked())
                    } else {
                        // dstore: untagged slot is raw f64 bits.
                        Value::Double(f64::from_bits(cv.to_bits()))
                    };
                    frame.set_local_unchecked(b1 as usize, v); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 2;
                    continue;
                }
                // xaload: iaload..saload (0x2e..=0x35)
                0x2e..=0x35 => {
                    // Quickened attempt first, entirely on raw slots.
                    //
                    // This arm used to open by decoding BOTH operands into the
                    // 16-byte `Value` enum and only then offer them to the
                    // quickened path — so every array element load paid two
                    // `CompactValue -> Value` conversions before the fast arm
                    // could decline them, and the fall-through pushed both
                    // wide values back. The `*astore` arm below already had
                    // the right shape (peek the raw slots, decode only on the
                    // decline), and this is that shape.
                    //
                    // `index >= 0` is tested here so a negative index still
                    // reaches the general path, which owns the AIOOBE and its
                    // message.
                    if let Some(zgc) = fast_field_zgc {
                        if frame.stack.len() >= 2 {
                            let (idx_cv, idx_kind) = frame.stack.peek_with_kind_at(0);
                            let (arr_cv, arr_kind) = frame.stack.peek_with_kind_at(1);
                            if let (Some(index), Some(aptr)) =
                                (idx_cv.as_int(), arr_cv.as_object_ptr())
                            {
                                if index >= 0 && aptr != 0 {
                                    // SAFETY: an `Object`-tagged operand-stack slot holds a
                                    // heap address; both arms re-validate the header.
                                    let arr_ref = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
                                    frame.stack.pop_compact();
                                    frame.stack.pop_compact();
                                    if field_fast::array_load_prim(
                                        zgc,
                                        &mut frame.stack,
                                        arr_ref,
                                        index,
                                        opcode,
                                    ) || (opcode == 0x32
                                        && field_fast::array_load_ref(
                                            zgc,
                                            &mut frame.stack,
                                            arr_ref,
                                            index,
                                            ref_array_fast_on,
                                            site_census,
                                        ))
                                    {
                                        frame.pc = saved_pc + 1;
                                        continue;
                                    }
                                    // Declined. Restore both operand slots
                                    // bit-for-bit AND kind-for-kind.
                                    //
                                    // `push_compact` would have been correct on
                                    // the verifier's guarantee that an `*aload`
                                    // sees `(arrayref, int)`, neither of which
                                    // is category-2 — but it writes
                                    // `KIND_UNKNOWN`, and the one shape that
                                    // guarantee excludes is exactly the one the
                                    // kind array exists for: a collision-shaped
                                    // `long` is bit-identical to `Value::Int(0)`
                                    // (see `ValueStack::kinds`), so an unmarked
                                    // one with reference-looking bits is what a
                                    // GC root scan would relocate. Restoring a
                                    // slot is not the place to spend a
                                    // guarantee.
                                    frame.stack.push_with_kind_unchecked(arr_cv, arr_kind);
                                    frame.stack.push_with_kind_unchecked(idx_cv, idx_kind);
                                }
                            }
                        }
                    }
                    // The array operand is reference-typed: a plain reference
                    // passes through (`ref_operand_value`), and a decline
                    // restores both slots bit- and kind-exact, as above — the
                    // `Value` re-push this used marked an unrecorded array
                    // reference KIND_LONG, out of the GC root scan's sight.
                    let (idx_cv, idx_kind) = frame.stack.pop_with_kind_unchecked();
                    let (arr_cv, arr_kind) = frame.stack.pop_with_kind_unchecked();
                    let idx_val = stack_slot_value(idx_cv, idx_kind);
                    let arr_val = ref_operand_value(shared, arr_cv, arr_kind);
                    if let (Value::Object(Some(arr_ref)), Value::Int(index)) = (arr_val, idx_val) {
                        if index < 0 {
                            let _ = frame;
                            park_runtime!(
                                // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                                RuntimeError::aioobe(
                                    index,
                                    shared.mem.heap.array_length(arr_ref) as i32,
                                ),
                                saved_pc,
                            );
                            continue;
                        }
                        // The quickened attempt already ran above, on the raw
                        // slots, before either operand was decoded.
                        // Cast: `index >= 0` was checked just above, so i32 -> usize keeps the value.
                        match shared.mem.heap.get_array_element(arr_ref, index as usize) {
                            Ok(value) => {
                                frame.stack.push_unchecked(value);
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                park_runtime!(
                                    // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                                    RuntimeError::aioobe(
                                        i,
                                        shared.mem.heap.array_length(arr_ref) as i32,
                                    ),
                                    saved_pc,
                                );
                                continue;
                            }
                        }
                    }
                    frame.stack.push_with_kind_unchecked(arr_cv, arr_kind);
                    frame.stack.push_with_kind_unchecked(idx_cv, idx_kind);
                }
                // xastore: iastore(0x4f), lastore(0x50), fastore(0x51), dastore(0x52),
                //          bastore(0x54), castore(0x55), sastore(0x56)
                //
                // WP4.3 — opcode-typed pop.  The plain `pop_unchecked()` (=
                // `to_value()`) decodes an untagged long bit-pattern as
                // `Value::Double(<denormal>)`, then `set_array_element` →
                // `write_prim_element` only matches `Value::Long(_)` for a
                // long[] slot and falls through to zero.  That makes
                // `arr[i] = 10L; System.out.println(arr[i])` print `0`.
                // Inspect the opcode and coerce to the JVMS-declared type
                // so the operand-stack tag-erasure cannot regress here.
                0x4f..=0x52 | 0x54..=0x56 => {
                    let (cv, kind_of_popped) = frame.stack.pop_with_kind_unchecked();
                    if let Some(zgc) = fast_field_zgc {
                        if frame.stack.len() >= 2 {
                            let idx_cv = frame.stack.peek_compact();
                            let arr_cv = frame.stack.peek_compact_at(1);
                            if let (Some(index), Some(aptr)) =
                                (idx_cv.as_int(), arr_cv.as_object_ptr())
                            {
                                // SAFETY: an `Object`-tagged operand-stack slot holds a
                                // heap address; `array_store_prim` re-validates it.
                                let arr_ref = unsafe { ObjectRef::from_raw(aptr as *mut u8) };
                                if field_fast::array_store_prim(
                                    zgc,
                                    arr_ref,
                                    index,
                                    opcode,
                                    cv,
                                    kind_of_popped,
                                ) {
                                    frame.stack.pop_compact();
                                    frame.stack.pop_compact();
                                    frame.pc = saved_pc + 1;
                                    continue;
                                }
                            }
                        }
                    }
                    let value = match opcode {
                        0x50 => Value::Long(cv.as_long_unchecked()),
                        0x52 => {
                            // dastore: untagged slot is raw f64 bits, and a slot
                            // the push marked KIND_DOUBLE is raw f64 bits too
                            // even when they collide with the NaN-box tag space
                            // — which is why the mark is consulted before the
                            // tag (see `CompactValue::double_raw`).
                            use crate::types::CompactTag;
                            if kind_of_popped == crate::runtime::ValueStack::KIND_MARK_DOUBLE {
                                Value::Double(f64::from_bits(cv.to_bits()))
                            } else {
                                match cv.tag() {
                                    CompactTag::Double => {
                                        Value::Double(f64::from_bits(cv.to_bits()))
                                    }
                                    CompactTag::Long => {
                                        // Rare: NaN-tagged-collision long landing
                                        // in a double slot; reinterpret the bits.
                                        Value::Double(f64::from_bits(cv.to_bits()))
                                    }
                                    _ => cv.to_value(),
                                }
                            }
                        }
                        // iastore / fastore / bastore / castore / sastore —
                        // the value is single-slot Int / Float, the existing
                        // decode path handles them correctly.
                        _ => cv.to_value(),
                    };
                    // Round-3: typed int pop for the array index — the spec
                    // mandates an int operand for every x-astore.
                    let index = frame.stack.pop_int_unchecked();
                    // Reference-typed array operand: see the `*aload` arm.
                    let (arr_cv, arr_kind) = frame.stack.pop_with_kind_unchecked();
                    if let Value::Object(Some(arr_ref)) =
                        ref_operand_value(shared, arr_cv, arr_kind)
                    {
                        if index < 0 {
                            let _ = frame;
                            park_runtime!(
                                // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                                RuntimeError::aioobe(
                                    index,
                                    shared.mem.heap.array_length(arr_ref) as i32,
                                ),
                                saved_pc,
                            );
                            continue;
                        }
                        match shared
                            .mem.heap
                            .set_array_element(arr_ref, index as usize, value) // Widening: index conversion
                        {
                            Ok(()) => {
                                // Phase 10 #2: the host just wrote this
                                // array, so any device buffer mirroring it
                                // is stale.
                                //
                                // AUDIT 2026-09-02: this arm did not do
                                // this, and it is the arm that RUNS. The
                                // `Instruction::Iastore` arm in
                                // `opcodes.rs` invalidates and so does
                                // `jit::helpers::jit_iastore`, so the hole
                                // was invisible to a reading of either —
                                // but the fast dispatch loop handles every
                                // x-astore before `opcodes.rs` is ever
                                // consulted, and it left the GPU
                                // input-residency cache holding a device
                                // copy the host had moved on from. The
                                // next submit computed from stale data:
                                // silent wrong answers in the default
                                // configuration, with `--gpu` and no other
                                // flag.
                                //
                                // Found by `GpuRuntimeStress`, where four
                                // of six scenarios diverged from HotSpot
                                // and the two that passed were exactly the
                                // two that never mutate an input between
                                // submits.
                                //
                                // Costs one relaxed load and a not-taken
                                // branch when nothing is cached, which is
                                // every run that never submits a kernel —
                                // see `input_cache::ADDR_FILTER`.
                                #[cfg(feature = "gpu-offload")]
                                crate::runtime::offload::input_cache::invalidate(arr_ref);
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                park_runtime!(
                                    // `array_store_fault`, not `aioobe`: the
                                    // store channel's `Err` also carries
                                    // `ARRAY_STORE_OUT_OF_MEMORY` (a boxing
                                    // allocation failed), which is an OOME,
                                    // not a negative index. The decoded arms
                                    // already map it this way.
                                    // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                                    RuntimeError::array_store_fault(
                                        i,
                                        shared.mem.heap.array_length(arr_ref) as i32,
                                    ),
                                    saved_pc,
                                );
                                continue;
                            }
                        }
                    }
                    frame.stack.push_with_kind_unchecked(arr_cv, arr_kind);
                    frame.stack.push_int_unchecked(index);
                    frame.stack.push_unchecked(value);
                }
                // aastore (0x53) — needs SATB pre-barrier + write barrier
                0x53 => {
                    // Quickened attempt first, on the raw slots (wave 22, lane
                    // L7; `field_fast::array_store_ref`): a plain array
                    // reference, an int index and a null-or-plain value whose
                    // store the lock-free covariance verdicts confirm. Any
                    // decline leaves all three slots untouched for the general
                    // arm below, which owns every exception and coercion.
                    if let Some(zgc) = fast_field_zgc {
                        if frame.stack.len() >= 3 {
                            let (val_cv, val_kind) = frame.stack.peek_with_kind_at(0);
                            let idx_cv = frame.stack.peek_compact_at(1);
                            let (arr_cv, arr_kind) = frame.stack.peek_with_kind_at(2);
                            if let (Some(index), Some(arr_ref)) =
                                (idx_cv.as_int(), ref_slot_object(arr_cv, arr_kind))
                            {
                                if ref_array_fast_on
                                    && field_fast::array_store_ref(
                                        shared,
                                        zgc,
                                        arr_ref,
                                        index,
                                        val_cv,
                                        val_kind,
                                        site_census,
                                    )
                                {
                                    frame.stack.pop_compact();
                                    frame.stack.pop_compact();
                                    frame.stack.pop_compact();
                                    frame.pc = saved_pc + 1;
                                    continue;
                                }
                            }
                        }
                    }
                    // The stored value and the array are both reference-typed:
                    // a plain reference passes through (`ref_operand_value`)
                    // where the old `pop_unchecked` + coerce stored NULL for an
                    // unrecorded one. Every other shape is decoded and coerced
                    // exactly as before.
                    let (val_cv, val_kind) = frame.stack.pop_with_kind_unchecked();
                    let raw_value = ref_operand_value(shared, val_cv, val_kind);
                    // A valid `aastore` always receives a reference.  A few
                    // native/reflection bridges can nevertheless surface a raw
                    // primitive at this boundary (notably serialization's
                    // primitive field path).  Do the Java boxing here, while
                    // the executing thread is still available, instead of
                    // letting the GC manufacture an untyped AUTOBOX sentinel.
                    // A refused wrapper allocation is an OutOfMemoryError
                    // thrown at this `aastore` (gc-common w5-c).
                    let value = match normalize_aastore_value(shared, raw_value) {
                        Ok(v) => v,
                        Err(oom) => {
                            let _ = frame;
                            park_runtime!(oom, saved_pc);
                            continue;
                        }
                    };
                    // Round-3: typed int pop for the array index.
                    let index = frame.stack.pop_int_unchecked();
                    let (arr_cv, arr_kind) = frame.stack.pop_with_kind_unchecked();
                    if let Value::Object(Some(arr_ref)) =
                        ref_operand_value(shared, arr_cv, arr_kind)
                    {
                        // JVMS 6.5 fixes the order NPE -> AIOOBE -> ASE, and the
                        // bounds test is TWO-SIDED. This arm checked only
                        // `index < 0`, so an index PAST THE END fell through to
                        // the covariance check below and reported
                        // ArrayStoreException where HotSpot reports
                        // ArrayIndexOutOfBoundsException (measured:
                        // RArrayStoreTiers s15, String[] as Object[], index 5
                        // into length 1).
                        //
                        // The slow-path `Instruction::Aastore` arm in opcodes.rs
                        // received the full two-sided check first. This fast-path
                        // twin is the one the interpreter actually dispatches, so
                        // fixing only the other one changed nothing observable --
                        // the same two-handlers-for-one-opcode drift as the JIT
                        // emitter that never called `jit_aastore`.
                        // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                        let arr_len = shared.mem.heap.array_length(arr_ref) as i32;
                        if index < 0 || index >= arr_len {
                            let _ = frame;
                            park_runtime!(RuntimeError::aioobe(index, arr_len), saved_pc);
                            continue;
                        }
                        // JVMS §aastore covariance check (mirrors the slow-path
                        // `Instruction::Aastore` arm): a non-null element whose
                        // runtime type is not assignment-compatible with the
                        // array's component type throws ArrayStoreException.
                        // `aastore_element_assignable` fails open on imprecise
                        // type info, so this is additive and never a false ASE.
                        if let Value::Object(Some(elem_ref)) = value {
                            if shared.mem.heap.kind_of(arr_ref) == cratonvm_types::ObjectKind::Array
                                && shared.mem.heap.element_type_of(arr_ref)
                                    == ArrayElementType::Reference
                                && !aastore_element_assignable(shared, arr_ref, elem_ref)
                            {
                                let _ = frame;
                                // Name the VALUE'S OWN class, HotSpot-style. On
                                // a reference array the header class id is the
                                // COMPONENT's, so the raw lookup answers
                                // `java.lang.Integer` for an `Integer[]` where
                                // HotSpot answers `[Ljava.lang.Integer;`
                                // (`RArrayStoreTiers` s04). `cce_display_class_
                                // name` is the existing repair for exactly that
                                // — see the long note on the slow-path
                                // `Instruction::Aastore` twin in opcodes.rs.
                                // Separate statements: the helper takes the
                                // class-manager read lock itself.
                                let raw_elem_name = shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(shared.mem.heap.class_id_of(elem_ref))
                                    .map(|c| c.name.to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let elem_cls =
                                    cce_display_class_name(shared, elem_ref, &raw_elem_name);
                                // JIT round 21 wave 24, lane names: by the
                                // value's bottom class's hidden flag.
                                let elem_cls =
                                    crate::runtime::exceptions::array_store_element_text(
                                        shared, elem_ref, elem_cls,
                                    );
                                park_runtime!(
                                    RuntimeError::ArrayStoreException { message: elem_cls },
                                    saved_pc,
                                );
                                continue;
                            }
                        }
                        // SATB pre-barrier: log old array element before overwrite
                        // Widening: index conversion
                        // gci i12/wb items 4-5: only while the queue may log,
                        // and not on G1, whose `set_array_element` publishes
                        // the overwritten element itself (see `opcodes.rs`).
                        // zt t32 (2f): not under load-barrier marking either
                        // (its `set_array_element` applies the plain-word rule).
                        if shared.mem.heap.satb_armed()
                            && !shared.mem.heap.is_g1()
                            && !shared.mem.heap.zgc_load_barrier_marking()
                        {
                            // Cast: `0 <= index < arr_len` was checked above, so i32 -> usize keeps the value.
                            if let Ok(old_elem) =
                                shared.mem.heap.get_array_element(arr_ref, index as usize)
                            {
                                shared.mem.heap.satb_barrier(old_elem);
                            }
                        }
                        match shared
                            .mem.heap
                            .set_array_element(arr_ref, index as usize, value) // Widening: index conversion
                        {
                            Ok(()) => {
                                // write_barrier fires automatically inside set_array_element
                                frame.pc = saved_pc + 1;
                                continue;
                            }
                            Err(i) => {
                                let _ = frame;
                                park_runtime!(
                                    // `array_store_fault`, not `aioobe`: the
                                    // store channel's `Err` also carries
                                    // `ARRAY_STORE_OUT_OF_MEMORY` (a boxing
                                    // allocation failed), which is an OOME,
                                    // not a negative index. The decoded arms
                                    // already map it this way.
                                    // Cast: JVM spec: an array length is a non-negative int, so usize -> i32 is lossless.
                                    RuntimeError::array_store_fault(
                                        i,
                                        shared.mem.heap.array_length(arr_ref) as i32,
                                    ),
                                    saved_pc,
                                );
                                continue;
                            }
                        }
                    }
                    frame.stack.push_with_kind_unchecked(arr_cv, arr_kind);
                    frame.stack.push_int_unchecked(index);
                    frame.stack.push_unchecked(value);
                }
                // arraylength (0xbe) — direct int push on the success path.
                0xbe => {
                    // Quickened arm first: reads the length out of the header
                    // at the address already in the slot, with no `Value`
                    // round trip and no `VmHeap` dispatch. Declines a null or
                    // non-array receiver so the general path below keeps the
                    // NPE and the corrupt-header diagnosis. See
                    // `field_fast::arraylength_fast`. Its admission is the
                    // hoisted `arraylength_fusion_on` (`fast_field_zgc` and
                    // `CRATONVM_JIT_NO_ARRAYLENGTH_FAST`; wave 60, lane L7:
                    // the kill switch was a memo read per `arraylength`).
                    if arraylength_fusion_on
                        && field_fast::arraylength_fast(&mut frame.stack, site_census)
                    {
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    // Reference-typed operand: see the `*aload` arm.
                    let (arr_cv, arr_kind) = frame.stack.pop_with_kind_unchecked();
                    if let Value::Object(Some(arr_ref)) =
                        ref_operand_value(shared, arr_cv, arr_kind)
                    {
                        let len = shared.mem.heap.array_length(arr_ref);
                        frame.stack.push_int_unchecked(len as i32); // Cast: array length to JVM int
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                    frame.stack.push_with_kind_unchecked(arr_cv, arr_kind);
                }
                // invokevirtual — stackless dispatch with monomorphic inline cache
                0xb6 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    if call_counts_on && (pgo_enabled || branch_profile_on) {
                        record_call_for_frame(shared, &*frame, saved_pc);
                    }
                    let _ = frame;
                    // Interpreter round i2 wave 54 (lane L7,
                    // `CRATONVM_SIG_POLY_SITE_FIRST`): a site the thread's
                    // signature-polymorphic route cache may hold is probed
                    // there first; the door and the cached dispatcher below
                    // could only miss it (no invoke-cache entry names a
                    // signature-polymorphic descriptor). One filter-bit test
                    // for every other site.
                    let mut sig_poly_probed = false;
                    if door_gates & DOOR_SIG_POLY_SITE_FIRST != 0
                        && thread.sig_poly_sites.may_hold(cp_index, saved_pc)
                    {
                        sig_poly_probed = true;
                        match sig_poly_site::try_hit_first(
                            shared, thread, frame_idx, cp_index, saved_pc,
                        ) {
                            // Interpreter round i6 wave 122, lane L4: a hit may
                            // push a frame since wave 120
                            // (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`,
                            // `sig_poly_site::try_basic_frame_push`); the loop
                            // must run it, not the caller at a stale index.
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => continue,
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            None => {}
                        }
                    }
                    // What the door read from the inline cache when it declined,
                    // so the general dispatcher below does not probe it again.
                    let mut door_probe = DoorProbe::NotProbed;
                    if door_gates & DOOR_INVOKE_FAST != 0 {
                        match execute_invokevirtual_fast_door(
                            shared,
                            thread,
                            frame_idx,
                            cp_index,
                            false,
                            door_gates & DOOR_NONVIRTUAL_FAST != 0,
                            fast_field_zgc,
                            saved_pc,
                            &mut door_probe,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            None => {}
                        }
                    }
                    // (No non-virtual door here since wave 6. It ran only after
                    // the virtual door above declined — `nonvirtual_fast_door_on`
                    // implies `invoke_fast_door_on` on a platform thread (on a
                    // JDK virtual thread each has its own switch since waves
                    // 114-115, and a private target the virtual door does not
                    // hand off takes the general arm) — and for `invokevirtual`
                    // it serves just a `Bytecode` entry, which the virtual door
                    // already hands to the same door body (`nonvirtual_door_prelude`)
                    // whenever this door is on. So it could only re-probe the
                    // key the virtual door had just read, and decline again.
                    // Since wave 7 the door also hands the entry it read to the
                    // general dispatcher below (`door_probe`), so a decline on
                    // the entry's kind probes the site once, not twice. See
                    // docs/feature-designs/real-jdk-mode.md.)
                    //
                    // PERF: consult the cheap thread-local inline cache FIRST.
                    // A warm monomorphic site hits here and dispatches with one
                    // class-id compare + arg decode + frame push — no locks, no
                    // hierarchy walk. Only on a miss (cold site, or the receiver
                    // class changed) do we fall to the heavier `vtable_fast`
                    // resolution, which re-populates the inline cache. (This is
                    // the order the `execute_invokevirtual_vtable_fast` header
                    // comment always described — "only invoked on its miss
                    // path" — but the dispatch had it inverted, so `vtable_fast`'s
                    // 3 RwLocks + 2 hierarchy walks ran on EVERY virtual call and
                    // the inline cache was never consulted.)
                    let cached_result = execute_invokevirtual_cached_probed(
                        shared, thread, frame_idx, cp_index, saved_pc, false, false, door_probe,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    // A signature-polymorphic site (`VarHandle` access mode,
                    // `MethodHandle.invokeExact`) never has an invoke-cache
                    // entry; its route is cached here (`sig_poly_site`). One
                    // `is_empty` test for a thread that never filled one. Not
                    // again for a key the early probe above already asked.
                    if door_gates & DOOR_SIG_POLY_SITE != 0 && !sig_poly_probed {
                        match sig_poly_site::try_hit(shared, thread, frame_idx, cp_index, saved_pc)
                        {
                            // A pushed frame (wave 122, lane L4; see `try_hit_first`'s arm).
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => continue,
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            None => {}
                        }
                    }
                    // Miss path — VtableManager lock-free dispatch; a hit here
                    // populates invoke_cache for the next call. A dispatch
                    // through it links the `invokevirtual` entry
                    // (`CRATONVM_LINKED_INVOKEVIRTUAL_AME_TEXT`, interpreter
                    // round i7 wave 142, lane L2).
                    match execute_invokevirtual_vtable_fast(
                        shared, thread, frame_idx, cp_index, saved_pc, false,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            dispatch_virtual::note_linked_invokevirtual_site(
                                shared, thread, frame_idx, cp_index,
                            );
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            dispatch_virtual::note_linked_invokevirtual_site(
                                shared, thread, frame_idx, cp_index,
                            );
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    let slow_result = if door_gates & DOOR_SIG_POLY_SITE != 0 {
                        sig_poly_site::execute_invoke_recording(
                            shared, thread, frame_idx, cp_index, saved_pc,
                        )
                    } else {
                        execute_invoke(shared, thread, frame_idx, cp_index, false, saved_pc)
                    };
                    match slow_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                }
                // invokespecial — stackless dispatch with cache
                0xb7 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    if call_counts_on && (pgo_enabled || branch_profile_on) {
                        record_call_for_frame(shared, &*frame, saved_pc);
                    }
                    let _ = frame;
                    // What the door read from the inline cache when it declined
                    // on the entry's kind (interpreter round i1 wave 22, lane
                    // L4), so the general dispatcher below does not probe the
                    // same key again — the `0xb6` arm's hand-over, for `0xb7`.
                    let mut door_probe = DoorProbe::NotProbed;
                    if door_gates & DOOR_NONVIRTUAL_FAST != 0 {
                        match invoke_fast::execute_nonvirtual_fast_door(
                            shared,
                            thread,
                            frame_idx,
                            cp_index,
                            true,
                            saved_pc,
                            fast_field_zgc,
                            &mut door_probe,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            None => {}
                        }
                    }
                    let cached_result = execute_invokevirtual_cached_probed(
                        shared, thread, frame_idx, cp_index, saved_pc, true, false, door_probe,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    match execute_invoke(shared, thread, frame_idx, cp_index, true, saved_pc) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                }
                // invokestatic — stackless dispatch with cache
                0xb8 => {
                    // (A class-name recognizer for one Elasticsearch kernel,
                    // `org/elasticsearch/tdigest/Dist.quantile`/`cdf`, used to
                    // open this arm and collapse its `Integer.valueOf;
                    // Function.apply; checkcast; Double.doubleValue` adapter.
                    // It was a hard-coded class-name allow-list in the hottest
                    // dispatch arm, it matched opcodes without checking their
                    // operands, and it skipped four calls' invocation counters
                    // and receiver profiles. Removed in interpreter round i1,
                    // wave 2; the JIT keeps its own kernel handling, and the
                    // general invoke caches carry the interpreted case. See
                    // docs/feature-designs/real-jdk-mode.md.)
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    if call_counts_on && (pgo_enabled || branch_profile_on) {
                        record_call_for_frame(shared, &*frame, saved_pc);
                    }
                    let _ = frame;
                    if door_gates & DOOR_NONVIRTUAL_FAST != 0 {
                        match invoke_fast::execute_invokestatic_fast_door(
                            shared, thread, frame_idx, cp_index, saved_pc,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            None => {}
                        }
                    }
                    let cached_result =
                        execute_invokestatic_cached(shared, thread, frame_idx, cp_index, saved_pc);
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    match execute_invokestatic(shared, thread, frame_idx, cp_index, saved_pc) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                }
                // invokeinterface — stackless dispatch with monomorphic inline cache
                0xb9 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                                                                     // invokeinterface is 5 bytes: opcode(1) + index(2) + count(1) + 0(1)
                    frame.pc = saved_pc + 5;
                    if call_counts_on && (pgo_enabled || branch_profile_on) {
                        record_call_for_frame(shared, &*frame, saved_pc);
                    }
                    let _ = frame;
                    let mut door_probe = DoorProbe::NotProbed;
                    if door_gates & DOOR_INVOKE_FAST != 0 {
                        match execute_invokevirtual_fast_door(
                            shared,
                            thread,
                            frame_idx,
                            cp_index,
                            true,
                            false,
                            fast_field_zgc,
                            saved_pc,
                            &mut door_probe,
                        ) {
                            Some(Ok(CachedCallResult::FramePushed)) => {
                                frame_idx = thread.frames.len() - 1;
                                continue;
                            }
                            Some(Ok(_)) => {
                                continue;
                            }
                            Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                            // The virtual door has no entry for a lambda proxy
                            // receiver (nothing fills one); the lambda door
                            // answers the eligible shapes from the loop
                            // (`lambda::lambda_door_lookup`).
                            None => match invoke_fast::execute_lambda_door(
                                shared, thread, frame_idx, cp_index, saved_pc,
                            ) {
                                Some(Ok(CachedCallResult::FramePushed)) => {
                                    frame_idx = thread.frames.len() - 1;
                                    continue;
                                }
                                Some(Ok(_)) => {
                                    continue;
                                }
                                Some(Err(e)) => route_fast_path_error!(e, saved_pc),
                                None => {}
                            },
                        }
                    }
                    let cached_result = execute_invokevirtual_cached_probed(
                        shared, thread, frame_idx, cp_index, saved_pc, false, true, door_probe,
                    );
                    match cached_result {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    // Miss path: interface dispatch shares the same vtable fast-path
                    // because the receiver's vtable already carries the
                    // concrete (name, desc) → slot mapping regardless of
                    // whether the call-site is invokevirtual or
                    // invokeinterface.
                    match execute_invokevirtual_vtable_fast(
                        shared, thread, frame_idx, cp_index, saved_pc, true,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(CachedCallResult::Handled) => {
                            continue;
                        }
                        Ok(CachedCallResult::CacheMiss) => {}
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                    // invokeinterface: thread is_interface=true so γ's stash
                    // arms the default-method rescue.
                    match execute_invoke_kind(
                        shared, thread, frame_idx, cp_index, false, true, saved_pc,
                    ) {
                        Ok(CachedCallResult::FramePushed) => {
                            frame_idx = thread.frames.len() - 1;
                            continue;
                        }
                        Ok(_) => {
                            continue;
                        }
                        Err(e) => route_fast_path_error!(e, saved_pc),
                    }
                }
                // ── Fast-path coverage completion (interpreter audit
                // 2026-08-18) ────────────────────────────────────────────
                //
                // Everything below reached the decoded handler until now: the
                // quickened stream had to resolve `saved_pc` (instruction-start
                // bitmap + per-block popcount), the ~200-arm `Instruction` match
                // in `opcodes.rs` had to be entered, and each arm re-indexed
                // `thread.frames[frame_idx]` — bounds-checked, `imul` by
                // `size_of::<Frame>()` — once per operand it touched.
                //
                // The gaps were not chosen, they are what the fast path grew up
                // around: `ishl`/`ishr`/`iushr` had arms but `lshl`/`lshr`/`lushr`
                // did not, `ineg`/`lneg` did but `fneg`/`dneg` did not,
                // `irem`/`lrem` did but `frem`/`drem` did not, `lcmp` did but
                // `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` did not, `i2l`/`i2f`/`i2d`/`l2i`
                // did but the eight remaining conversions did not, and `dup`/`pop`
                // did but the six other shuffles did not. The long shifts alone
                // are the whole of a 64-bit hash mixer (`h ^= h >>> 33`) and of
                // most of the bit-twiddling in the crypto suites this VM runs.

                // ── Reference comparisons: if_acmpeq / if_acmpne ──────────
                // These close a loop in `do { … } while (a != b)` and in any
                // bottom-test frontend's output; see `cond_branch_arm!` for
                // what having no arm here used to cost such a loop.
                //
                // Two plain reference slots (see `acmp_on_raw_slots`) compare
                // by raw payload with no `Value` decode; anything else takes
                // `refs_equal` on the decoded operands, unchanged.
                0xa5 => {
                    let eq = match acmp_on_raw_slots(&frame.stack) {
                        Some(eq) => {
                            frame.stack.pop_compact();
                            frame.stack.pop_compact();
                            eq
                        }
                        None => {
                            let vb = frame.stack.pop_unchecked();
                            let va = frame.stack.pop_unchecked();
                            refs_equal(&va, &vb)
                        }
                    };
                    cond_branch_arm!(frame, saved_pc, b1, b2, eq);
                }
                // The `active_profiles_identity_trace` diagnostic lives on the
                // decoded `IfAcmpne` arm and prints class names for both
                // operands. Decline the fast path while it is armed rather than
                // duplicating it, so turning the flag on still reaches it.
                0xa6 if diag_gates & DG_ACMP_IDENTITY_TRACE == 0 => {
                    let eq = match acmp_on_raw_slots(&frame.stack) {
                        Some(eq) => {
                            frame.stack.pop_compact();
                            frame.stack.pop_compact();
                            eq
                        }
                        None => {
                            let vb = frame.stack.pop_unchecked();
                            let va = frame.stack.pop_unchecked();
                            refs_equal(&va, &vb)
                        }
                    };
                    cond_branch_arm!(frame, saved_pc, b1, b2, !eq);
                }
                // ── Null tests: ifnull / ifnonnull ────────────────────────
                // `ref_operand_is_null` (not `Value::is_null`) so a JNI jobject
                // null carried as `Value::Long(0)` reads as null — identical to
                // the decoded arm this replaces.
                //
                // An `Object`-tagged slot is non-null whatever its kind mark
                // (the `Value` decode of such bits is `Object(Some)`, or a
                // non-zero `Long`/a `Double` under a cat-2 mark — never null),
                // so the common non-null operand skips the `Value` decode; every
                // other shape takes the decode, unchanged.
                // Wave 115 (lane L8): the test is `pop_ref_operand_is_null`,
                // shared with the fused `aload_N; ifnull | ifnonnull`.
                0xc6 | 0xc7 => {
                    let is_null = pop_ref_operand_is_null(&mut frame.stack);
                    cond_branch_arm!(frame, saved_pc, b1, b2, is_null == (opcode == 0xc6));
                }

                // ── Long shifts (JVMS: shift distance masked to 6 bits) ───
                0x79 => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    let v = frame.stack.pop_long_unchecked();
                    frame.stack.push_long_unchecked(v << sh);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x7b => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    let v = frame.stack.pop_long_unchecked();
                    frame.stack.push_long_unchecked(v >> sh);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x7d => {
                    let sh = frame.stack.pop_int_unchecked() & 0x3F;
                    // Widening: unsigned conversion for the logical shift
                    let v = frame.stack.pop_long_unchecked() as u64;
                    // Cast: JIT ABI -- i64 register convention
                    frame.stack.push_long_unchecked((v >> sh) as i64);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Float / double negate and remainder ───────────────────
                // Rust's `%` on floats is fmod, which is exactly what JVMS
                // §6.5 frem/drem specify (NOT the IEEE 754 remainder).
                0x72 => {
                    let vb = frame.stack.pop_float_unchecked();
                    let va = frame.stack.pop_float_unchecked();
                    // fmod, with HotSpot's default NaN for an invalid
                    // remainder (`java_frem` / `java_drem`, wave 98).
                    frame.stack.push_float_unchecked(java_frem(va, vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x73 => {
                    let vb = frame.stack.pop_double_unchecked();
                    let va = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(java_drem(va, vb));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x76 => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_float_unchecked(-v);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x77 => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_double_unchecked(-v);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── The eight remaining primitive conversions ─────────────
                // Narrowings to integer defer to the same saturation helpers
                // the decoded arms use (JVMS §2.8.3: NaN → 0, +inf → MAX,
                // -inf → MIN); widenings are plain casts.
                0x89 => {
                    let v = frame.stack.pop_long_unchecked();
                    // JVM spec: l2f may lose precision
                    frame.stack.push_float_unchecked(v as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8a => {
                    let v = frame.stack.pop_long_unchecked();
                    // JVM spec: l2d may lose precision above 2^53
                    frame.stack.push_double_unchecked(v as f64);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8b => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_int_unchecked(float_to_int(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8c => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_long_unchecked(float_to_long(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8d => {
                    let v = frame.stack.pop_float_unchecked();
                    frame.stack.push_double_unchecked(f64::from(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8e => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_int_unchecked(double_to_int(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x8f => {
                    let v = frame.stack.pop_double_unchecked();
                    frame.stack.push_long_unchecked(double_to_long(v));
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x90 => {
                    let v = frame.stack.pop_double_unchecked();
                    // JVM spec: d2f may lose precision
                    frame.stack.push_float_unchecked(v as f32);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Float / double comparisons ────────────────────────────
                // `l` and `g` differ only in the NaN answer.
                0x95 | 0x96 => {
                    let vb = frame.stack.pop_float_unchecked();
                    let va = frame.stack.pop_float_unchecked();
                    let nan_result = if opcode == 0x95 { -1 } else { 1 };
                    let result = if va.is_nan() || vb.is_nan() {
                        nan_result
                    } else if va > vb {
                        1
                    } else if va == vb {
                        0
                    } else {
                        -1
                    };
                    frame.stack.push_int_unchecked(result);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x97 | 0x98 => {
                    let vb = frame.stack.pop_double_unchecked();
                    let va = frame.stack.pop_double_unchecked();
                    let nan_result = if opcode == 0x97 { -1 } else { 1 };
                    let result = if va.is_nan() || vb.is_nan() {
                        nan_result
                    } else if va > vb {
                        1
                    } else if va == vb {
                        0
                    } else {
                        -1
                    };
                    frame.stack.push_int_unchecked(result);
                    frame.pc = saved_pc + 1;
                    continue;
                }

                // ── Operand-stack shuffles ────────────────────────────────
                // Every one of these moves slot bits AND the kind mark, which
                // is the whole reason they use `*_with_kind_unchecked` rather
                // than a `Value` round-trip: `ValueStack::is_cat2_kind` reads
                // the mark to decide whether a NaN-tag-colliding long is one
                // logical category-2 value or two category-1 slots, and the GC
                // reads it to decide whether a pointer-shaped slot is a
                // reference. Structure mirrors the decoded arms exactly.
                0x58 => {
                    let (val, kind) = frame.stack.pop_with_kind_unchecked();
                    if !crate::runtime::ValueStack::is_cat2_kind(kind, val) {
                        frame.stack.pop_with_kind_unchecked();
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5a => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    frame.stack.push_with_kind_unchecked(v1, k1);
                    frame.stack.push_with_kind_unchecked(v2, k2);
                    frame.stack.push_with_kind_unchecked(v1, k1);
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5b => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k2, v2) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5c => {
                    // dup2; <x>aload; <operand>; <op>; [conv]; <x>astore
                    // with the array and the index already on the stack,
                    // however computed (round i3 wave 72, lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2`):
                    // `hist[x & 255]++`. One range test of the next byte per
                    // `dup2`, then the per-VM switch; the base switch, the
                    // decode and every decline are the out-of-line helper's,
                    // and a decline leaves the stack as it was so the plain
                    // `dup2` below runs.
                    //
                    // Wave 75 (lane L7,
                    // `CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`): a pc whose
                    // group the helper already refused on its bytes is
                    // remembered and costs no decode (tested inside the
                    // helper since wave 76, lane L7: frame ratchet). (Wave
                    // 73's byte screen stood here; retired in wave 75.)
                    if matches!(b1, 0x2e | 0x2f | 0x33..=0x35)
                        && !fusion_off
                        && shared.config.interp_fused_array_increment_dup2
                    {
                        if let Some(zgc) = fast_field_zgc {
                            if let Some(next_pc) = field_fast::fused_array_increment_dup2(
                                shared,
                                zgc,
                                frame,
                                code_len,
                                saved_pc,
                                &mut thread.site_tables.static_field_sites,
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k1, v1) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5d => {
                    let (v1, k1) = frame.stack.pop_with_kind_unchecked();
                    let (v2, k2) = frame.stack.pop_with_kind_unchecked();
                    if crate::runtime::ValueStack::is_cat2_kind(k1, v1) {
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    } else {
                        let (v3, k3) = frame.stack.pop_with_kind_unchecked();
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                        frame.stack.push_with_kind_unchecked(v3, k3);
                        frame.stack.push_with_kind_unchecked(v2, k2);
                        frame.stack.push_with_kind_unchecked(v1, k1);
                    }
                    frame.pc = saved_pc + 1;
                    continue;
                }
                0x5e => {
                    // One classifier, shared with the decoded arm: the four
                    // forms consume different depths and only `value3` tells
                    // FORM 1 and FORM 3 apart. `None` is an unverifiable shape
                    // -- don't `continue`, fall through to the decoded arm so
                    // it reports the error instead of shuffling blind.
                    if let Some(form) = frame.stack.dup2_x2_form() {
                        let mut held = [(CompactValue::int(0), 0u8); 4];
                        for slot in held.iter_mut().take(form.consumed()) {
                            *slot = frame.stack.pop_with_kind_unchecked();
                        }
                        for &depth in form.push_order() {
                            let (cv, kind) = held[depth];
                            frame.stack.push_with_kind_unchecked(cv, kind);
                        }
                        frame.pc = saved_pc + 1;
                        continue;
                    }
                }
                // ── Constant-pool and object opcodes ──────────────────────
                //
                // These had no arm, so each paid the quickened `resolve(pc)`,
                // the frame-pointer hoist and its `code_ptr` compare, a
                // non-inlined call into `execute_instruction`, the ~200-variant
                // `Instruction` match and the post-call diagnostic checks —
                // all before its body ran.
                //
                // The bodies are NOT copied here. Each lives in exactly one
                // `opcodes::op_*` function and BOTH paths call it, so there is
                // still one implementation per opcode. Two copies of an opcode
                // is precisely the shape `difftest`'s `interp-decoded` axis
                // exists to catch — `getfield` alone is 450 lines, and a fix
                // landing in one copy and not the other is the failure that
                // axis was built after.
                //
                // `pc` is advanced BEFORE the call because the decoded path
                // does (it writes `next_pc` ahead of dispatch) and several of
                // these bodies read `frame.pc` back — `monitorenter` and
                // `monitorexit` snapshot it, and the diagnostic blocks print
                // it. Advancing after the call would change what they observe.
                // getfield / putfield — quickened arm first (`field_fast`), the
                // full handler on any miss. The full handler refills the site.
                0xb4 | 0xb5 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    if let Some(zgc) = fast_field_zgc {
                        let hit = if opcode == 0xb4 {
                            field_fast::getfield_fast(
                                shared,
                                zgc,
                                &mut thread.site_tables.fast_field_sites,
                                frame,
                                cp_index,
                                site_census,
                                diag_gates & DG_FIELD_PHASES != 0,
                                field_watch_hoist,
                            )
                        } else {
                            field_fast::putfield_fast(
                                zgc,
                                &mut thread.site_tables.fast_field_sites,
                                frame,
                                cp_index,
                                site_census,
                                field_watch_hoist,
                            )
                        };
                        if hit {
                            // getfield #A; getfield #B [; ...] (interpreter
                            // round i6 wave 116, lane L8,
                            // `CRATONVM_INTERP_FUSED_GETFIELD_CHAIN`, default
                            // off): `this.a.b`, `node.next.value`. The value
                            // just pushed is the next getfield's receiver;
                            // `getfield_chain` serves each following
                            // quickened hit and answers the pc of the first
                            // bytecode it did not run, so a null receiver's
                            // NPE, a cold site or a watch is the unfused
                            // `getfield` at its own pc. Nothing thrown,
                            // nothing safepointed inside the chain; declined
                            // under `fusion_off`. Off: one gate-bit test per
                            // quickened getfield hit.
                            // SAFETY: a verified getfield's operands lie
                            // below `code_len`, so saved_pc + 3 is in the
                            // padded buffer.
                            if opcode == 0xb4
                                && door_gates & DOOR_FUSED_GETFIELD_CHAIN != 0
                                && !fusion_off
                                && unsafe { *code_ptr.add(saved_pc + 3) } == 0xb4
                            {
                                frame.pc = getfield_chain(
                                    shared,
                                    zgc,
                                    &mut thread.site_tables.fast_field_sites,
                                    frame,
                                    code_len,
                                    saved_pc + 3,
                                );
                                continue;
                            }
                            frame.pc = saved_pc + 3;
                            continue;
                        }
                    }
                    frame.pc = saved_pc + 3;
                    let _ = frame;
                    let outcome = if opcode == 0xb4 {
                        op_getfield(shared, thread, frame_idx, cp_index)
                    } else {
                        op_putfield(shared, thread, frame_idx, cp_index)
                    };
                    if let Err(e) = outcome {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // checkcast / instanceof: arms of their own (wave 27, lane L7),
                // so the dispatch's jump table lands on the call. They shared
                // the arm below, whose second `match opcode` re-dispatched
                // every type check through a compare chain on a value the
                // table had already decoded — twice per iteration of a
                // `if (o instanceof T) ((T) o).m()` loop.
                0xc0 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    let _ = frame;
                    if let Err(e) = op_checkcast(shared, thread, frame_idx, cp_index) {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                0xc1 => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 3;
                    let _ = frame;
                    if let Err(e) = op_instanceof(shared, thread, frame_idx, cp_index) {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                0xb2 | 0xb3 | 0xbb | 0xbd => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                                                                     // Wave 66 (lane L7, `CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`):
                                                                     // `getstatic #F; if<cond> L` over a quickened int-typed
                                                                     // static (`if (flag)`, `while (running)`): the int
                                                                     // `getstatic_int_site` reads (the fused bound's probe, read
                                                                     // and fence) is compared with zero and the branch handed
                                                                     // to `cond_branch_arm!` at the BRANCH's own pc, so its
                                                                     // profile, back-edge accounting, OSR and poll are the
                                                                     // unfused `if<cond>` arm's. Nothing is pushed. A watch
                                                                     // armed, a miss or a non-int field falls through to the
                                                                     // unfused `getstatic` below, untouched.
                    if opcode == 0xb2
                        && static_ldc_gates & SLG_FUSED_GETSTATIC_IFZERO != 0
                        && !fusion_off
                        && saved_pc + 5 < code_len
                    {
                        // SAFETY: saved_pc + 5 < code_len, checked above.
                        let br_op = unsafe { *code_ptr.add(saved_pc + 3) };
                        if (0x99..=0x9e).contains(&br_op)
                            && !field_watch_hoist
                                .unwrap_or_else(crate::runtime::jvmti::any_field_watchpoint_active)
                        {
                            if let Some(v) = getstatic_int_site(
                                frame,
                                &mut thread.site_tables.static_field_sites,
                                cp_index,
                                |holder, slot| crate::vm::get_static_shared(shared, holder, slot),
                            ) {
                                let br_pc = saved_pc + 3;
                                // SAFETY: as above (br_pc + 2 == saved_pc + 5).
                                let ob1 = unsafe { *code_ptr.add(br_pc + 1) };
                                // SAFETY: as above.
                                let ob2 = unsafe { *code_ptr.add(br_pc + 2) };
                                // The state a back-edge safepoint in the
                                // unfused pair would see: the branch is the
                                // executing bytecode.
                                frame.last_instr_pc = br_pc;
                                cond_branch_arm!(frame, br_pc, ob1, ob2, if_zero_taken(br_op, v));
                            }
                        }
                    }
                    // Wave 67 (lane L7, `CRATONVM_INTERP_FUSED_STATIC_INCREMENT`):
                    // `getstatic #F; <int operand>; iadd | isub; putstatic #F`
                    // (`x++`, `x -= k`) over a quickened, non-final `I` static
                    // is read, added and stored by `fused_static_increment`
                    // with the quickened arms' own steps (probe, read, fences,
                    // `set_static_shared`), and the pc lands past the
                    // `putstatic`. Only the operand byte is tested here; the
                    // rest of the shape, the site and every decline are the
                    // helper's (out of line, so the loop's frame keeps its
                    // size), and a decline has written nothing, so the
                    // unfused `getstatic` below runs as before.
                    if opcode == 0xb2
                        && static_ldc_gates & SLG_FUSED_STATIC_INCREMENT != 0
                        && !fusion_off
                        && saved_pc + 3 < code_len
                    {
                        // SAFETY: saved_pc + 3 < code_len, checked above.
                        let operand_op = unsafe { *code_ptr.add(saved_pc + 3) };
                        // Wave 68 (lane L7): a long operand (`lconst_0/1`,
                        // `lload`, `lload_N`) is let through only under
                        // `CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE`, read
                        // from the config on that branch alone (no hoisted
                        // local), so a `seq++` on a `long` static pays no
                        // declined call with the switch off. The narrow
                        // shapes share the int operand bytes.
                        // Wave 83 (lane L7): an `ldc2_w` operand only
                        // under `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` (its
                        // gate bit) as well, so with that switch off a
                        // `getstatic; ldc2_w` pays no declined call.
                        if (matches!(operand_op, 0x02..=0x08 | 0x10 | 0x11 | 0x15 | 0x1a..=0x1d)
                            || (matches!(operand_op, 0x09 | 0x0a | 0x16 | 0x1e..=0x21))
                            || (operand_op == 0x14
                                && static_ldc_gates & SLG_FUSED_INCREMENT_LDC2W != 0
                                && saved_pc + 6 < code_len
                                // Host A/B (wave 83): a `double` static's
                                // `ldc2_w; dadd` paid a declined call (+18%
                                // on `staticDouble`); only an `ldc2_w`
                                // followed by `ladd` / `lsub` is offered.
                                // SAFETY: saved_pc + 6 < code_len, checked above.
                                && matches!(unsafe { *code_ptr.add(saved_pc + 6) }, 0x61 | 0x65)))
                            && !field_watch_hoist
                                .unwrap_or_else(crate::runtime::jvmti::any_field_watchpoint_active)
                        {
                            // Wave 69 (lane L7): the two config switches the
                            // helper reads travel as one byte, read here on
                            // this branch alone, as `wide` was.
                            if let Some(next_pc) = fused_static_increment(
                                frame,
                                code_len,
                                saved_pc,
                                with_static_ldc2w(
                                    static_increment_mode(
                                        true,
                                        shared.config.interp_fused_increment_early_decline,
                                    ),
                                    static_ldc_gates & SLG_FUSED_INCREMENT_LDC2W != 0,
                                ),
                                &mut thread.site_tables,
                                |holder, slot| crate::vm::get_static_shared(shared, holder, slot),
                                |holder, slot, value| {
                                    crate::vm::set_static_shared(shared, holder, slot, value)
                                },
                            ) {
                                frame.pc = next_pc;
                                continue;
                            }
                        }
                    }
                    // Wave 65 (lane L7, `CRATONVM_INTERP_GETSTATIC_INLINE_HIT`):
                    // a quickened `getstatic` is answered here, by the probe
                    // `getstatic_site_hit` makes (same table and key, by
                    // reference rather than a clone), the same read and fence,
                    // and the push `push_static_field_value` makes for the
                    // descriptor, through the frame the match holds. A field
                    // watch armed (JVMTI or JDWP `FieldAccess`) declines before
                    // the probe, so `op_getstatic` reports the access; a miss,
                    // a value shape the push would coerce otherwise, or a push
                    // that would overflow (nothing written) falls through to
                    // the call below, which answers exactly as before.
                    if opcode == 0xb2
                        && static_ldc_gates & SLG_GETSTATIC_INLINE != 0
                        && !field_watch_hoist
                            .unwrap_or_else(crate::runtime::jvmti::any_field_watchpoint_active)
                        && getstatic_inline_hit(
                            frame,
                            &mut thread.site_tables.static_field_sites,
                            cp_index,
                            static_ldc_gates & SLG_GETSTATIC_REMAP_TRACE != 0,
                            |holder, slot| crate::vm::get_static_shared(shared, holder, slot),
                        )
                    {
                        frame.pc = saved_pc + 3;
                        continue;
                    }
                    // Wave 66 (lane L7, `CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`):
                    // the store twin. A quickened, non-final `putstatic` is
                    // stored here by `putstatic_site_hit`'s own steps (the pop
                    // by descriptor, the fences around a volatile store,
                    // `set_static_shared` with its SATB pre-barrier). Every
                    // decline (watch armed, miss, final, a pop the call could
                    // fail) is decided before anything is popped, so the call
                    // below then answers exactly as before.
                    if opcode == 0xb3
                        && static_ldc_gates & SLG_PUTSTATIC_INLINE != 0
                        && !field_watch_hoist
                            .unwrap_or_else(crate::runtime::jvmti::any_field_watchpoint_active)
                        && putstatic_inline_hit(
                            frame,
                            &mut thread.site_tables.static_field_sites,
                            cp_index,
                            |holder, slot, value| {
                                crate::vm::set_static_shared(shared, holder, slot, value)
                            },
                        )
                    {
                        frame.pc = saved_pc + 3;
                        continue;
                    }
                    frame.pc = saved_pc + 3;
                    let _ = frame;
                    let outcome = match opcode {
                        0xb2 => op_getstatic(shared, thread, frame_idx, cp_index),
                        0xb3 => op_putstatic(shared, thread, frame_idx, cp_index),
                        0xbb => op_new(shared, thread, frame_idx, cp_index),
                        // 0xbd
                        _ => op_anewarray(shared, thread, frame_idx, cp_index),
                    };
                    if let Err(e) = outcome {
                        // Same classifier the invoke fast paths use: it performs
                        // the Runtime/Linkage conversions the slow path's
                        // per-opcode guard would otherwise have done, which is
                        // the step those arms once skipped and killed the
                        // process over.
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // monitorenter / monitorexit — single-byte, so `pc` advances by
                // one rather than three; otherwise identical to the arm above.
                0xc2 | 0xc3 => {
                    frame.pc = saved_pc + 1;
                    let _ = frame;
                    let outcome = if opcode == 0xc2 {
                        op_monitorenter(shared, thread, frame_idx)
                    } else {
                        op_monitorexit(shared, thread, frame_idx)
                    };
                    if let Err(e) = outcome {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // newarray (0xbc, one-byte `atype`) — `op_newarray`, the body
                // the decoded arm calls (wave 3, lane L1). `anewarray` rides
                // the constant-pool arm above.
                0xbc => {
                    frame.pc = saved_pc + 2;
                    let _ = frame;
                    if let Err(e) = op_newarray(shared, thread, frame_idx, b1) {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // athrow (0xbf) — `op_athrow`, the one body with every throw
                // diagnostic, which never succeeds. The throwable is routed
                // straight into the handler walk at this pc. What the decoded
                // path's `ExceptionThrown` arm adds is kept, by the classifier
                // every arm shares: it drains `native_pending_return` (a
                // leftover must not outlive the throw) and prefers it only
                // when the thrown address is no longer a live object.
                0xbf => {
                    frame.pc = saved_pc + 1;
                    let _ = frame;
                    let e = match op_athrow(shared, thread, frame_idx) {
                        Ok(never) => match never {},
                        Err(e) => e,
                    };
                    route_fast_path_error!(e, saved_pc);
                }
                // invokedynamic (0xba idx16 0 0) — the decoded arm's one call,
                // `invokedynamic::execute_invokedynamic`, which links (or hits
                // its site cache), runs the target to completion and pushes the
                // result; it never leaves a frame pushed, so `frame_idx` is
                // unchanged. `pc` advances first, as the decoded path advances
                // it, and a thrown exception is settled exactly as the decoded
                // path's `ExceptionThrown` arm settles it (by the classifier,
                // as on every arm).
                0xba => {
                    let cp_index = ((b1 as u16) << 8) | (b2 as u16); // Cast: bytecode operand decoding
                    frame.pc = saved_pc + 5;
                    let _ = frame;
                    let outcome = crate::runtime::invokedynamic::execute_invokedynamic(
                        shared, thread, frame_idx, cp_index,
                    );
                    if let Err(e) = outcome {
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // ldc (0x12, 1-byte index) / ldc_w (0x13) / ldc2_w (0x14).
                // `execute_ldc` / `execute_ldc2w` were already factored out, so
                // these arms are pure dispatch removal. The `ldc` family also
                // needs the malformed-constant-pool conversion the decoded arms
                // apply, so it is applied here too rather than dropped.
                0x12 | 0x13 | 0x14 => {
                    let (cp_index, width) = if opcode == 0x12 {
                        (b1 as u16, 2) // Cast: bytecode operand decoding
                    } else {
                        (((b1 as u16) << 8) | (b2 as u16), 3) // Cast: bytecode operand decoding
                    };
                    // Wave 64 (lane L7, `CRATONVM_INTERP_LDC_INLINE_HIT`): a
                    // warm int / float `ldc` / `ldc_w` is answered here, by
                    // the probe `execute_ldc` makes first (same table, key
                    // and epoch validation), pushed through the frame the
                    // match holds with the typed checked push (the same slot
                    // and kind `ValueStack::push(Value::Int|Float)` writes).
                    // A miss, a reference constant, `ldc2_w`, or a push that
                    // would overflow (it returns `Err` having written
                    // nothing) falls through to the call below, which
                    // answers exactly as before (and raises the overflow).
                    if static_ldc_gates & SLG_LDC_INLINE_HIT != 0
                        && opcode != 0x14
                        && ldc_inline_hit(
                            frame,
                            &mut thread.site_tables.prim_const_sites,
                            cp_index,
                            site_census,
                        )
                    {
                        frame.pc = saved_pc + width;
                        continue;
                    }
                    // Wave 65 (lane L7, `CRATONVM_INTERP_LDC2W_INLINE_HIT`):
                    // a warm long / double `ldc2_w` is answered here, by the
                    // probe `execute_ldc2w` makes first and its tag-exact
                    // push (`push_long` / `push_double`). A miss or a push
                    // that would overflow (nothing written) takes the call.
                    if static_ldc_gates & SLG_LDC2W_INLINE_HIT != 0
                        && opcode == 0x14
                        && ldc2w_inline_hit(
                            frame,
                            &mut thread.site_tables.prim_const_sites,
                            cp_index,
                        )
                    {
                        frame.pc = saved_pc + width;
                        continue;
                    }
                    // Wave 63 (lane L7, `CRATONVM_INTERP_LDC_HOISTED_PC`):
                    // the same store, to the same frame, before the same
                    // call, through the `frame` the match already holds
                    // instead of `FrameStack`'s `IndexMut` (bounds check and
                    // multiply) after ending that borrow -- what wave 57 did
                    // for twelve other out-of-line arms. Off: as before.
                    if static_ldc_gates & SLG_LDC_HOISTED_PC != 0 {
                        frame.pc = saved_pc + width;
                        let _ = frame;
                    } else {
                        let _ = frame;
                        thread.frames[frame_idx].pc = saved_pc + width;
                    }
                    let raw = if opcode == 0x14 {
                        execute_ldc2w(shared, thread, frame_idx, cp_index)
                    } else {
                        execute_ldc(shared, thread, frame_idx, cp_index)
                    };
                    if let Err(e) = raw {
                        let e = convert_ldc_class_format_error(shared, thread, e);
                        route_fast_path_error!(e, saved_pc);
                    }
                    continue;
                }
                // tableswitch / lookupswitch (interpreter round i6 wave 118,
                // lane L8, `CRATONVM_INTERP_FAST_SWITCH_ARMS`, default ON since wave 129).
                // Without this arm both run on the decoded path: the
                // quickened lookup there, then `execute_instruction`'s
                // out-of-line call, its match and the `Result` round trip,
                // for an instruction that is one table read. This arm takes
                // the SAME quickened instruction the decoded path would
                // (`quick`, the loop's cache of `quickened_for_frame`, kept
                // under the same code-pointer rule) and answers the same
                // offset (`TableSwitch` bounds, `LookupSwitch::target`), so
                // the answer cannot differ. It serves only a forward target:
                // a backward one (no javac switch has one) falls to the
                // decoded path, which owns the back-edge accounting the
                // `Continue` arm does for a switch, as does a key slot that is
                // not a plain int (the decoded `pop_int`'s errors and
                // coercions stay its own), an empty stack, and a missing
                // quickened stream. Nothing is popped before the decision.
                // A switch neither throws nor polls on a forward target, and
                // the decoded path records no profile for one, so a hit is
                // exactly the decoded execution minus its overhead.
                0xaa | 0xab if door_gates & DOOR_FAST_SWITCH != 0 => {
                    if !frame.stack.is_empty() {
                        let (cv, kind) = frame.stack.peek_with_kind_unchecked();
                        let plain_int = kind != crate::runtime::ValueStack::KIND_MARK_LONG
                            && kind != crate::runtime::ValueStack::KIND_MARK_DOUBLE;
                        if let (Some(key), true) = (cv.as_int(), plain_int) {
                            if code_ptr != quick_code_ptr {
                                quick = quickened_for_frame(&*frame);
                                quick_code_ptr = code_ptr;
                            }
                            let offset = match quick.as_deref().and_then(|q| q.resolve(saved_pc)) {
                                Some((Instruction::Tableswitch(ts), _)) => {
                                    Some(if key >= ts.low && key <= ts.high {
                                        // Widening: index conversion, as the decoded arm.
                                        ts.offsets[(key - ts.low) as usize]
                                    } else {
                                        ts.default
                                    })
                                }
                                Some((Instruction::Lookupswitch(ls), _)) => Some(ls.target(key)),
                                _ => None,
                            };
                            if let Some(offset) = offset.filter(|&o| o > 0) {
                                frame.stack.pop_compact();
                                // Cast: a positive branch offset, as the decoded arm adds it.
                                frame.pc = saved_pc + offset as usize;
                                continue;
                            }
                        }
                    }
                    // Fall out of the match: the decoded path runs it.
                }
                _ => { /* fall through to slow path */ }
            }
        }

        // --- Slow path: pre-decoded (quickened) lookup, else full decode ---
        //
        // The quickened stream is a pure memoization of `Instruction::decode`
        // keyed by bytecode pc: `resolve` only ever hands back the record whose
        // recorded pc equals `saved_pc`, together with exactly the `next_pc`
        // `decode` returned there. Every pc-keyed consumer downstream
        // (exception table, stack maps, line numbers, JVMTI single-step,
        // JIT/OSR entry) therefore sees identical values. Any pc the stream
        // does not know -- dead bytes, a desynchronised landing pad -- falls
        // through to the original decode below.
        // ── Frame-pointer hoist (frame-arena.md §6.1) ────────────────────
        // `JvmThread::frames` is a `FrameStack`, which guarantees that a
        // frame's address never changes unless the backing buffer grows, and
        // that growth happens only inside `reserve_stable`/`push` and always
        // bumps `reloc_epoch()`. That makes it sound to hold one raw frame
        // pointer across the whole dispatch region instead of re-indexing.
        //
        // The region this pointer covers runs from the derivation below to
        // the `execute_instruction` call, and contains **no push, no pop, and
        // no access to `thread.frames` other than through `fp`**. The four
        // aliasing rules therefore hold:
        //   1. No overlapping reference. Every frame access in the region
        //      goes through `fp`; in particular `quickened_for_frame` takes a
        //      single `&Frame`, which we derive from `fp` itself, and nothing
        //      here takes the `&[Frame]` slice view (no `capture_full_trace`,
        //      no root scan, no unwinding — those all live outside).
        //   2. No `&mut` reborrow of the stack, so `fp`'s provenance stays
        //      live; `thread` is next touched by `execute_instruction`, by
        //      which point `fp` is dead.
        //   3. No relocation, hence no epoch change — asserted in debug.
        //   4. Frame still live: the null check below *is* the
        //      `frame_idx < thread.frames.len()` test, and it routes a
        //      violation through `VmError` where indexing would have panicked.
        //
        // This collapses 13 bounds-checked, `imul`-by-`size_of::<Frame>()`
        // re-indexes per executed bytecode down to one address computation.
        #[cfg(debug_assertions)]
        let reloc_epoch_at_hoist = thread.frames.reloc_epoch();
        let fp: *mut Frame = thread.frames.frame_ptr(frame_idx);
        if fp.is_null() {
            return Err(decoded_frame_out_of_range(
                frame_idx,
                thread.frames.len(),
                saved_pc,
            ));
        }
        // SAFETY: `fp` is non-null and in bounds (checked above), and rules
        // 1-3 above hold for every dereference in this region.
        let code_ptr_now = unsafe { (*fp).code.as_ptr() };
        if code_ptr_now != quick_code_ptr {
            // SAFETY: as above. `quickened_for_frame` only reads the frame.
            quick = quickened_for_frame(unsafe { &*fp });
            quick_code_ptr = code_ptr_now;
        }
        // One O(1) call yielding both the pre-decoded instruction and the
        // `next_pc` that `Instruction::decode` reported there, replacing
        // `index_of_pc` + `op` + `next_pc` and their three bounds checks.
        // `move`: the closure copies `saved_pc` instead of borrowing it (see
        // `value_return_underflow` for why no borrow of it may stay).
        let quick_hit: Option<(&Instruction, usize)> =
            quick.as_deref().and_then(move |q| q.resolve(saved_pc));
        // Only populated on the fallback path; keeps the freshly decoded
        // instruction alive for as long as `instruction` borrows it.
        let mut fallback_decoded: Option<Instruction> = None;
        if quick_hit.is_none() {
            // SAFETY: hoist rules 1-4 above. All accesses here are reads of
            // the frame plus the single `pc` write-back; the `&[u8]` borrow of
            // `code` ends when `decode` returns, before that write.
            let decoded_at_pc = unsafe {
                Instruction::decode(&(*fp).code, (*fp).pc).map_err(|e| {
                    MethodCallFailed::InternalError(VmError::Internal {
                        message: format!(
                            "decode error at pc={} in {}.{}: {}",
                            (*fp).pc,
                            (*fp).method_name(),
                            (*fp).method_descriptor(),
                            e
                        ),
                    })
                })
            };
            let (decoded, next_pc) = decoded_at_pc?;
            // SAFETY: `fp` is the hoisted pointer to the executing frame,
            // null-checked at the top of the dispatch region. This region
            // performs no push, so the frame stack cannot have reallocated and
            // moved the frame; nothing else references it here.
            unsafe { (*fp).pc = next_pc };
            fallback_decoded = Some(decoded);
        }
        // Panic-free selection (B3 / NEW-7 zero-panic gate): the `(None, None)`
        // arm is unreachable by construction -- the block above always fills
        // `fallback_decoded` when there is no quickened hit -- but it is routed
        // through `VmError` rather than an `unreachable!`.
        let instruction: &Instruction = match (quick_hit, fallback_decoded.as_ref()) {
            (Some((quickened, next_pc)), _) => {
                // SAFETY: hoist rules 1-4 above.
                unsafe { (*fp).pc = next_pc };
                quickened
            }
            (None, Some(decoded)) => decoded,
            (None, None) => return Err(no_instruction_decoded(saved_pc)),
        };

        // A copy for the event: `trace!` borrows its field values, and a
        // borrow of `saved_pc` itself would put it in memory for the whole
        // loop (see `value_return_underflow`).
        let trace_pc: usize = saved_pc;
        trace!(
            pc = trace_pc,
            instruction = ?instruction,
            // SAFETY: hoist rules 1-4 above.
            stack_depth = unsafe { (*fp).stack.len() },
            "execute"
        );

        // K1 diagnostic: capture per-opcode context (class, method, pc, opcode byte).
        // Debug-only and further gated at runtime by CRATONVM_DEBUG_STACK_TAG=1;
        // release builds compile update_diag_ctx to an empty function.
        #[cfg(debug_assertions)]
        {
            // SAFETY: hoist rules 1-4 above; all three accesses are reads.
            let frame: &Frame = unsafe { &*fp };
            let opcode_byte = frame.code.get(saved_pc).copied().unwrap_or(0);
            crate::runtime::value_stack::update_diag_ctx(
                frame.class_name(),
                frame.method_name(),
                saved_pc,
                opcode_byte,
            );
        }

        // Last use of `fp`: `execute_instruction` takes `&mut thread` and may
        // push frames, so the hoisted pointer must not outlive this point.
        #[cfg(debug_assertions)]
        {
            // Hoist rule 3: nothing in the region above may relocate frames.
            // A bump here would mean `fp` had gone stale mid-region.
            assert_eq!(
                reloc_epoch_at_hoist,
                thread.frames.reloc_epoch(),
                "frame relocation inside the dispatch region would invalidate \
                 the hoisted frame pointer"
            );
        }
        let exec_result = execute_instruction(shared, thread, frame_idx, instruction, saved_pc);

        // The decoded path's two error tripwires (`CRATONVM_DBG_UNDERFLOW`,
        // `CRATONVM_DBG_POPINT`), out of line since wave 27: they formatted
        // `saved_pc` in place, and that borrow kept it in memory for the
        // whole loop (see `value_return_underflow`).
        if exec_result.is_err() {
            decoded_error_tripwires(thread, frame_idx, saved_pc, instruction, &exec_result);
        }

        // Convert RuntimeErrors from native methods into catchable Java exceptions.
        let exec_result = match exec_result {
            Err(MethodCallFailed::InternalError(VmError::Runtime(runtime_err)))
                if !matches!(
                    runtime_err,
                    RuntimeError::NotImplemented { .. } | RuntimeError::StackOverflowError
                ) =>
            {
                let mcf =
                    crate::runtime::exceptions::throw_runtime_error(shared, thread, runtime_err);
                Err(mcf)
            }
            Err(MethodCallFailed::InternalError(VmError::Linkage(linkage_err))) => {
                if dbg_linkage() {
                    dbg_linkage_dump(thread, "slow-path opcode", &format!("{linkage_err:?}"));
                }
                let mcf =
                    crate::runtime::exceptions::throw_linkage_error(shared, thread, linkage_err);
                Err(mcf)
            }
            other => other,
        };

        #[allow(unreachable_patterns)]
        match exec_result {
            Ok(InstructionResult::Continue) => {
                // ── Decoded-path back-edge accounting ───────────────────
                //
                // The raw-bytecode arms above account for their own branches.
                // Anything that reaches the decoded handler and jumps BACKWARDS
                // had no accounting at all: `goto_w`, `tableswitch`,
                // `lookupswitch`, `ret`, and — under `-noverify`, where
                // `use_fast_path` is false — every branch in the instruction
                // set. A loop closed by one of those was invisible to back-edge
                // OSR and to the loop-work tier-up credit
                // `pop_and_recycle_frame_with_reason` computes from
                // `Frame::backward_count`.
                //
                // Testing the resulting pc rather than enumerating opcodes is
                // deliberate: it is the property that actually matters, it
                // cannot be forgotten when an opcode is added, and it is one
                // compare against a value already in hand — on the slow path
                // only, since a fast-path arm never reaches here.
                //
                // `Continue` is the only result this can key off: `FramePushed`
                // and `Return` change the frame under `frame_idx`, and a thrown
                // exception leaves through `Err`, so a handler landing pad
                // below `saved_pc` is not mistaken for a loop back edge.
                let new_pc = thread.frames[frame_idx].pc;
                if new_pc <= saved_pc {
                    // Backward-or-self: the debugger gate (see
                    // `refresh_debugger_gate!`; `goto_w +0` spins too).
                    // Hoisted, the loop top's slow path refreshes it.
                    if diag_gates & DG_OBSERVERS_HOISTED == 0 {
                        refresh_debugger_gate!(&thread.frames[frame_idx]);
                    }
                }
                if new_pc < saved_pc
                    && branch_arm_possible
                    && (branch_reread_always || !branch_profile_on)
                {
                    branch_profile_on = frame_branch_profile_armed(&thread.frames[frame_idx]);
                }
                if new_pc < saved_pc {
                    if pgo_enabled {
                        record_backedge_for_frame(shared, &thread.frames[frame_idx], saved_pc);
                    }
                    // `wrapping_add`: see `cond_branch_arm!`.
                    let bc = &mut thread.frames[frame_idx].backward_count;
                    *bc = bc.wrapping_add(1);
                    let osr_due = {
                        let f = &thread.frames[frame_idx];
                        f.backward_count >= osr_call_floor && f.backward_count >= f.osr_poll_at
                    };
                    if osr_due {
                        match osr_backoff_at!(new_pc) {
                            OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
                            OsrBackoffOutcome::ContinueDispatch => continue,
                            OsrBackoffOutcome::ThrowJava(exc) => {
                                park_java_exception!(exc, OSR_FRAME_DECLINED_TO_CATCH);
                                continue;
                            }
                            OsrBackoffOutcome::Skip => {}
                        }
                    }
                    if backedge_poll_needed!() {
                        safepoint_check(shared, thread);
                    }
                }
                continue;
            }
            Ok(InstructionResult::FramePushed) => {
                // A new bytecode frame was pushed — execute it iteratively
                frame_idx = thread.frames.len() - 1;
                continue;
            }
            Ok(InstructionResult::Return(value)) => {
                // Slow-path return — check for stackless frames. The declared
                // return type also drives the JVMS §6.5 `ireturn` narrowing the
                // fast-path return arm applies; both tiers of the interpreter
                // and the JIT must agree on it.
                // `get`, not indexing: the top-level branch below never read the
                // frame before, and must not start panicking if it is gone.
                let ret = thread
                    .frames
                    .get(frame_idx)
                    .map_or(b'V', |f| f.return_tag());
                if frame_idx > initial_frame_idx {
                    // Argument overlap (wave 37 lane L7): as in the fast
                    // value-return arm, an overlapped callee gives up its
                    // slots before the push into the parent lands on them.
                    if let Some(f) = thread.frames.get_mut(frame_idx) {
                        if f.locals_overlap_caller() {
                            f.release_caller_overlap();
                            invoke_phases::note_overlap_released();
                        }
                    }
                    let value = value.map(|v| {
                        if ret == b'V' {
                            v
                        } else {
                            narrow_ireturn_value(coerce_value_for_return(v, ret), ret)
                        }
                    });
                    if let Some(v) = value {
                        // A continuation return conversion (wave 25, lane L7;
                        // see the fast-path return arm).
                        if diag_gates & DG_THREAD_IS_VIRTUAL != 0
                            && !thread.continuation_return_adapters.is_empty()
                        {
                            if let Err(e) = push_return_across_continuation_boundary(
                                shared, thread, frame_idx, v, None,
                            ) {
                                route_fast_path_error!(e, saved_pc);
                            }
                        } else {
                            // Into the parent BEFORE the child is popped, so the
                            // monitor release and the FramePop callback in
                            // `pop_and_recycle_frame` run with the value rooted
                            // (wave 4; see the fast-path return arm for the epoch
                            // bump).
                            let parent = &mut thread.frames[frame_idx - 1];
                            parent.exec_epoch = parent.exec_epoch.wrapping_add(1);
                            // T18.K4 — route J/D through the tag-exact push so
                            // invoke* callees returning longs/doubles keep
                            // their tag on the caller's operand stack.
                            push_invoke_return_value(&mut parent.stack, v).map_err(|e| {
                                MethodCallFailed::InternalError(VmError::Runtime(e))
                            })?;
                        }
                    } else if ret == b'V'
                        && diag_gates & DG_THREAD_IS_VIRTUAL != 0
                        && !thread.continuation_return_adapters.is_empty()
                    {
                        // Interpreter round i6 wave 115, lane L4: a remounted
                        // `void` leaf's method-handle `null` (see the fast
                        // `return` arm).
                        if let Err(e) =
                            push_void_return_across_continuation_boundary(thread, frame_idx)
                        {
                            route_fast_path_error!(e, saved_pc);
                        }
                    }
                    pop_and_recycle_frame(shared, thread);
                    frame_idx -= 1;
                    continue;
                }
                return Ok(value.map(|v| narrow_ireturn_value(v, ret)));
            }
            Err(MethodCallFailed::InternalError(
                yield_signal @ VmError::ContinuationYield { .. },
            )) => {
                // Yield crosses every nested execute_frame boundary without
                // unwinding a Java frame. In particular, preserve the leaf
                // frame whose native sleep/park issued the signal: its PC is
                // already after the invoke and its operand stack contains the
                // live values needed when the continuation is remounted.
                return Err(MethodCallFailed::InternalError(yield_signal));
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(re))) => {
                // B4 fix (audit `vm-runtime.md`): an operand-stack overflow is
                // reported by `ValueStack::push` as
                // `NotImplemented { feature: "operand stack overflow" }`, which
                // `throw_runtime_error` maps to an *uncatchable* internal error
                // (exceptions.rs) — so a JVM stack overflow on unverified
                // bytecode (or a verifier gap) hard-unwinds the whole call stack
                // instead of surfacing as a Java-catchable
                // `java.lang.StackOverflowError`. Normalize it to the real
                // `StackOverflowError` runtime variant here (the only point that
                // converts runtime errors into Java exceptions) so the routing
                // below builds a real throwable and an in-method
                // `catch (StackOverflowError)` / `catch (Throwable)` observes it.
                let re = match re {
                    RuntimeError::NotImplemented { feature }
                        if feature == "operand stack overflow" =>
                    {
                        RuntimeError::StackOverflowError
                    }
                    other => other,
                };
                // Convert VM-generated RuntimeErrors (AIOOBE, NPE, CCE, etc.)
                // into real Java exception objects so they can be caught by
                // Java try/catch blocks.
                let exc_result = super::exceptions::throw_runtime_error(shared, thread, re);
                match exc_result {
                    MethodCallFailed::ExceptionThrown(exc) => {
                        // Settled like every other throw on both paths (wave
                        // 5): this arm sees only the shapes the conversion
                        // above skipped (`StackOverflowError`, and the stack
                        // overflow `NotImplemented` normalized to it), and was
                        // the one decoded throw that left
                        // `native_pending_return` rooted.
                        let exc = settle_thrown_exception(shared, thread, exc);
                        // The handler walk is `exception_dispatch::unwind_to_handler`,
                        // the same function the two `pending_*` arms at the top of
                        // the loop call. This arm and the `ExceptionThrown` arm
                        // below used to carry their own inline copies, and those
                        // copies had already drifted: the round-9 wave-8 fix
                        // that makes the walk push the PIN SLOT's value (the one
                        // copy a moving collector rewrites) rather than the ref
                        // the search handed back landed in `unwind_to_handler`
                        // only, and an install failure here returned through `?`
                        // without truncating `native_pin_roots`, leaking the pin.
                        unwind_here!(exc, saved_pc)?;
                    }
                    // If we couldn't create the Java exception object, fall back
                    // to the old behavior (unwind as internal error).
                    // `throw_runtime_error` only ever returns the two
                    // `MethodCallFailed` variants above; the match is
                    // exhaustive, so no panicking `_ => unreachable!()`
                    // catch-all is needed (NEW-7 / B3 panic-free gate).
                    other @ MethodCallFailed::InternalError(_) => {
                        while frame_idx > initial_frame_idx {
                            pop_and_recycle_frame_with_reason(shared, thread, true);
                            frame_idx -= 1;
                        }
                        return Err(other);
                    }
                }
            }
            Err(MethodCallFailed::InternalError(e)) => {
                // Non-Runtime internal errors (linkage, classfile, etc.) — unwind.
                if dbg_linkage() {
                    dbg_linkage_dump(thread, "unwind (uncatchable)", &format!("{e:?}"));
                }
                while frame_idx > initial_frame_idx {
                    pop_and_recycle_frame_with_reason(shared, thread, true);
                    frame_idx -= 1;
                }
                return Err(MethodCallFailed::InternalError(e));
            }
            Err(MethodCallFailed::ExceptionThrown(exc)) => {
                // `native_pending_return` roots a native call's returned
                // object across the Rust<->Java boundary until it is pushed
                // onto the caller's operand stack (see `safe_native_call` /
                // `native_return_pushed_to_stack`). It is UNRELATED to the
                // exception being unwound here in the common case — it can
                // still hold a leftover value if the native call that set it
                // never reached the "push to stack" step (e.g. its return
                // value was discarded, or a later, independent exception
                // fired before the pending value was consumed). Previously
                // this code unconditionally preferred `native_pending_return`
                // whenever it was `Some`, which meant a stale leftover object
                // (observed: a `CommonToken` left over from ANTLR HQL
                // parsing) silently replaced the REAL exception being
                // propagated, misattributing the uncaught exception's class
                // in fatal-error reporting.
                //
                // Only fall back to `native_pending_return` when `exc` itself
                // has gone stale (relocated/reclaimed by a moving GC that ran
                // during the failing native call, so `exc`'s address is no
                // longer a valid live object) — mirroring the staleness check
                // `safe_native_call` already performs in `vm_exec.rs`. The
                // slot is always drained via `.take()` so a leftover value
                // can never survive to poison a later, unrelated exception.
                let exc = settle_thrown_exception(shared, thread, exc);
                // Handler search, unwinding through stackless frames: the one
                // shared walk (see the runtime-error arm above for why this is
                // no longer an inline copy). A caller frame is searched at its
                // `last_instr_pc` — the invoke itself, since `pc` has already
                // advanced past the invoke's operand bytes.
                unwind_here!(exc, saved_pc)?;
            }
        }
    }
}

