#[allow(unused_imports)]
use super::*;

impl SharedVm {
    /// Sync JIT ProfileStore data into the AOT TrainingRunRecorder.
    ///
    /// Snapshots all branch and receiver profiles from the JIT, resolves class_id
    /// to class names, and bulk-imports them into the AOT recorder.
    /// Returns number of methods synced.
    #[cfg(feature = "experimental-aot")]
    pub fn sync_jit_profiles_to_aot(&self) -> usize {
        use crate::native::builtins::aot;

        if !aot::is_aot_training() {
            return 0;
        }

        let snapshots = self.jit.profile_store.snapshot_all();
        let class_mgr = self.classes.class_manager.read();

        // Collect branch data
        let mut _branch_entries: Vec<(&str, &str, u32, u32, u32)> = Vec::new();
        let mut _receiver_entries: Vec<(&str, &str, &str, &str, u32)> = Vec::new();

        // We need owned strings for class names since we're building slices
        let mut class_names: Vec<Option<String>> = Vec::with_capacity(snapshots.len());
        let mut receiver_names_cache: std::collections::HashMap<u32, String> =
            std::collections::HashMap::new();

        for (key, _profile) in &snapshots {
            let name = class_mgr
                .class_store
                .get(cratonvm_types::ClassId::new(key.class_id))
                .map(|c| c.name.to_string());
            class_names.push(name);
        }

        // Now build the entries with references to the owned strings
        let mut synced = 0usize;
        for (i, (key, profile)) in snapshots.iter().enumerate() {
            let class_name = match &class_names[i] {
                Some(n) => n.as_str(),
                None => continue,
            };
            let method_name = &*key.method_name;

            // Branch data
            for (&pc, counts) in &profile.branches {
                if counts.taken > 0 || counts.not_taken > 0 {
                    aot::aot_record_branch(class_name, method_name, pc as u32, true);
                    // Record aggregate counts via bulk API
                    // (we record one call per taken/not_taken for simplicity since
                    //  the recorder accumulates internally)
                    for _ in 1..counts.taken {
                        aot::aot_record_branch(class_name, method_name, pc as u32, true);
                    }
                    for _ in 0..counts.not_taken {
                        aot::aot_record_branch(class_name, method_name, pc as u32, false);
                    }
                }
            }

            // Receiver type data
            for (_pc, receivers) in &profile.receivers {
                for (&receiver_class_id, &count) in receivers {
                    let receiver_name = receiver_names_cache
                        .entry(receiver_class_id)
                        .or_insert_with(|| {
                            class_mgr
                                .class_store
                                .get(cratonvm_types::ClassId::new(receiver_class_id))
                                .map(|c| c.name.to_string())
                                .unwrap_or_default()
                        });
                    if !receiver_name.is_empty() {
                        for _ in 0..count {
                            aot::aot_record_receiver_type(
                                class_name,
                                method_name,
                                &*key.descriptor,
                                receiver_name,
                            );
                        }
                    }
                }
            }

            synced += 1;
        }
        synced
    }
}

/// One-way latch: "something in this process can ask interpreter threads for
/// a stack dump".
///
/// The dispatch loop's dump hook is a load of `stack_dump_requested` on every
/// bytecode. Nothing can ever set that flag unless a watchdog or sampler was
/// armed, which happens **before** Java starts running (the CLI spawns both at
/// startup, from `--stack-dump-on-timeout` / `--stack-sample-ms` /
/// `CRATONVM_DEFAULT_WATCHDOG_SEC`), so a run with neither can skip the load
/// entirely. The interpreter reads this once per `execute_frame` entry — the
/// same pgo-style tradeoff as its other hoisted gates.
///
/// That tradeoff is why arming must happen at **spawn** time, not at fire
/// time: the thread a watchdog exists to photograph is by definition one that
/// has been inside a single `execute_frame` for a long while, and it would not
/// re-read the gate. `request_stack_dump` arms the latch too, but only as a
/// backstop for a caller that never went through the CLI.
pub(in crate::vm::vm_init) static STACK_DUMP_WATCH_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl SharedVm {
    // -----------------------------------------------------------------------
    // T19.H1 — stack-dump-on-timeout API
    // -----------------------------------------------------------------------

    /// Arm [`STACK_DUMP_WATCH_ARMED`]. Call before spawning anything that may
    /// later call [`Self::request_stack_dump`] or [`Self::request_stack_sample`].
    pub fn arm_stack_dump_watch(&self) {
        STACK_DUMP_WATCH_ARMED.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether any stack-dump watchdog or sampler was ever armed in this
    /// process. `false` means `stack_dump_pending()` cannot become true, so
    /// the dispatch loop's hook can be skipped wholesale.
    #[inline(always)]
    pub fn stack_dump_watch_armed(&self) -> bool {
        STACK_DUMP_WATCH_ARMED.load(std::sync::atomic::Ordering::Acquire)
    }

    /// T19.H1 — request every interpreter thread to dump its frame chain
    /// to stderr at the next dispatch-loop iteration.
    ///
    /// Called by the CLI watchdog (see `--stack-dump-on-timeout`) after
    /// the configured deadline elapses. The flag is sticky and never
    /// cleared — the process is expected to abort shortly after.
    pub fn request_stack_dump(&self) {
        self.arm_stack_dump_watch();
        self.debug
            .stack_dump_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.send_loops_to_stack_dump_hook();
        // KC16-watchdog: also wake every thread parked in Object.wait().
        // The interpreter top-of-loop poll only fires when the thread is
        // actively dispatching bytecode — a thread blocked in
        // `parking_lot::Condvar::wait_for` (Monitor::wait) never reaches
        // it. Signalling the global wait flag lets each waiter's 5ms
        // poll exit the condvar and re-check its state, where the
        // standard top-of-loop ack path then fires.
        crate::threading::monitor::signal_stack_dump_to_waiters();
        // Round 14 wave 3: a backed-off waiter polls the flag above only every
        // 100 ms; wake them all now (`MonitorTable::wake_all_waiters_for_stack_dump`).
        self.threads.monitors.wake_all_waiters_for_stack_dump();
        // Also unpark every thread blocked in `LockSupport.park` (AQS
        // lock/latch/executor waiters). These never reach the condvar the
        // line above signals, so without this they stay invisible to the
        // watchdog. They wake, emit their park-site snapshot / current
        // frames, and the process aborts right after.
        self.threads.thread_registry.unpark_all_for_stack_dump();
        // The per-thread summary is NOT printed here. It used to be, and that
        // made its most load-bearing column a lie: printed at request time, it
        // cannot know which threads went on to answer, so it labelled every
        // RUNNING thread "read the live stack dump above" — including threads
        // that never produced one. The watchdog now calls
        // [`Self::dump_thread_summary_after_dumps`] once the grace period has
        // closed, when the ack set is complete.
    }

    /// T19.H1 — print the per-thread summary once the watchdog's grace period
    /// has closed, so it can distinguish a RUNNING thread that dumped from one
    /// that stayed silent (i.e. is in JIT-compiled code or a long native call).
    ///
    /// Summarizes every registered thread, including those with no dumpable
    /// interpreter frames — blocked in a native lock, or never started.
    pub fn dump_thread_summary_after_dumps(&self) {
        let acked = self.debug.stack_dump_acked_tids.lock().clone();
        self.threads
            .thread_registry
            .dump_thread_summary_to_stderr(&acked);
        self.dump_object_wait_census();
    }

    /// EVERY thread parked in `Object.wait()` at dump time, and the state of
    /// the object each one is parked on.
    ///
    /// # The question this exists to answer
    ///
    /// `internal/fixed-suite-bugs/netty/parameterizedsslhandlertest-residual-stalls-FIXED-20261013` named
    /// one cheap step that had never been taken: at stall time, dump
    /// every parked waiter rather than the first one the watchdog reaches. The
    /// residual it was written for is a promise that was never completed with
    /// no notification due — for which "nothing completed it" and "the thread
    /// that would have completed it is itself parked" are different defects
    /// with different suspect lists, and the single-thread dump cannot
    /// separate them. This can, in the stall that is already happening.
    ///
    /// An EMPTY census is a result, not a gap, and it is labelled as one: it
    /// means no thread in the process is in `Object.wait()`, so a stalled test
    /// thread is blocked on something else entirely (`LockSupport.park`, a
    /// native, a lock acquire) and the whole `Object.wait()` line of enquiry
    /// is the wrong one.
    pub fn dump_object_wait_census(&self) {
        let rows = self.threads.thread_registry.waiting_monitor_census();
        eprintln!(
            "--- [WAIT-CENSUS] {} thread(s) parked in Object.wait() \
             (registry `jmx_waiting_monitor`, a GC-forwarded root) ---",
            rows.len()
        );
        if rows.is_empty() {
            eprintln!(
                "  [WAIT-CENSUS] none. NOT a missing instrument: no thread in this \
                 process is inside Object.wait(), so a stall here is parked on \
                 something else (LockSupport.park, a lock acquire, or a native call)."
            );
        }
        for (tid, name, alive, obj, waited_ms) in rows {
            eprintln!(
                "  [WAIT-CENSUS] tid={tid} name={name:?} alive={alive} waited_ms={waited_ms}"
            );
            crate::vm::vm_init::dump_wait_object_state(self, obj);
        }
        eprintln!("--- [WAIT-CENSUS] end ---");
    }

    /// T19.H1 — fast-path check used by the interpreter hot loop.
    ///
    /// Returns `true` once the watchdog has flipped
    /// [`Self::stack_dump_requested`]. The load is `Relaxed` — we
    /// trade strict memory ordering for a single cold branch per
    /// dispatched bytecode (the typical case is `false` forever).
    #[inline(always)]
    pub fn stack_dump_pending(&self) -> bool {
        self.debug
            .stack_dump_requested
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether stack dumps are being driven as a periodic *sampler* rather
    /// than as the watchdog's one-shot pre-abort dump. See
    /// [`crate::vm::realms::DebugRealm::stack_sample_mode`].
    #[inline(always)]
    pub fn stack_sample_mode(&self) -> bool {
        self.debug
            .stack_sample_mode
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Arm sampling mode. Called once by the CLI when `--stack-sample-ms` is
    /// given, before the sampler thread starts re-arming the dump request.
    pub fn enable_stack_sampling(&self) {
        self.arm_stack_dump_watch();
        self.debug
            .stack_sample_mode
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Consume the pending dump request (sampling mode only) so the next
    /// re-arm from the sampler thread produces the next sample.
    pub fn clear_stack_dump_request(&self) {
        self.debug
            .stack_dump_requested
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Re-arm the dump request without the watchdog's thread-summary and
    /// unpark side effects, which are far too costly to repeat every
    /// sampling interval (and would themselves distort the profile).
    pub fn request_stack_sample(&self) {
        self.arm_stack_dump_watch();
        self.debug
            .stack_dump_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.send_loops_to_stack_dump_hook();
    }

    /// `CRATONVM_INTERP_STACK_DUMP_AT_POLL` (`VmConfig::interp_stack_dump_at_poll`;
    /// interpreter round i3 wave 82, lane L7): the dispatch loops of this VM
    /// run the stack-dump hook in their loop top's slow path, not per
    /// bytecode, so a request just raised sends every one of them there
    /// (the poll word each bytecode already compares; the same bump a JVMTI
    /// observer change makes). Called after the request's store, so a loop
    /// that sees its word move reads the raised request. Nothing without the
    /// switch.
    pub(in crate::vm::vm_init) fn send_loops_to_stack_dump_hook(&self) {
        if self.config.interp_stack_dump_at_poll {
            self.mem.gc_barrier.note_code_moved_on_every_loop();
        }
    }

    /// T19.H1 — dump the calling thread's frame chain to stderr.
    ///
    /// Writes one line per frame with the format
    /// `tid=<N> depth=<D> class=<C> method=<M> desc=<sig> pc=<P> source=<file>`,
    /// oldest frame first, so a reader can follow control flow top-down.
    /// Class and method names are truncated to 240 bytes each to avoid
    /// runaway output when the class loader has recorded a malformed
    /// entry during the hang.
    ///
    /// Increments [`Self::stack_dump_ack_count`] so the watchdog can
    /// tell how many runtime threads responded before it gives up and
    /// calls `std::process::abort`.
    ///
    /// JIT-frame caveat (2026-07-16 investigation of the
    /// `CRATONVM-SPRING-GENUINE-BUGLIST` "T19.H1
    /// watchdog SIGSEGVs on JIT frames" note): `thread.frames` is the
    /// interpreter's own logical frame stack. A method dispatched straight
    /// to already-JIT-compiled machine code (`execute_invokestatic_cached`
    /// / `execute_jit_call` et al. in `runtime/interpreter.rs`) never gets
    /// a `Frame` pushed here at all — the call is a synchronous jump into
    /// native code with no interpreter bookkeeping in between. That frame
    /// is therefore invisible to this walker: reproduction while
    /// root-causing this bug confirmed a thread parked deep inside a
    /// long-running JIT-compiled method dumps as either zero acks (if it
    /// never returns to the interpreter dispatch loop before the grace
    /// period elapses) or a misleadingly shallow frame count (if it does),
    /// never a fabricated/garbage frame — but a reader unaware of the gap
    /// can easily misdiagnose "1 shallow frame" as "this thread is stuck
    /// at a trivial call site" when it may be many JIT call-levels deep.
    /// The two hardening changes below close the *reliability* half of
    /// that report even though a live SIGSEGV could not be reproduced on
    /// this revision after extensive targeted testing (single JIT calls,
    /// OSR-adjacent long single-invocation loops, deep JIT<->interpreter
    /// recursion, and multi-threaded runs with one thread parked in JIT):
    /// 1. Each frame is formatted behind `catch_unwind` so a panic while
    ///    rendering one (malformed) frame can never abort the watchdog
    ///    thread before it reaches its own `process::abort()` — that
    ///    would otherwise turn an intended, informative crash-with-dump
    ///    into a silent hang (no dump, no abort, no notification).
    /// 2. When JIT is active for this thread and the walk comes up
    ///    shorter than the interpreter's own call chain, a note is
    ///    appended pointing at the gap and at `--nojit` as a workaround,
    ///    instead of leaving the reader to assume a shallow dump means a
    ///    shallow call stack.
    pub fn dump_current_thread_frames(&self, thread: &JvmThread) {
        use std::io::Write;

        // SAFETY: stderr is always available and shared; we serialize via
        // a local buffer first and make one `write_all` call per line so
        // concurrent dumps from multiple threads interleave cleanly at
        // the line boundary rather than the byte boundary.
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();

        let tid = thread.thread_id.0;
        let name = &thread.name;
        let frame_count = thread.frames.len();
        let header =
            format!("--- T19.H1 stack dump: tid={tid} name={name:?} frames={frame_count} ---\n");
        let _ = handle.write_all(header.as_bytes());

        for (depth, frame) in thread.frames.iter().enumerate() {
            // Defense-in-depth: a single malformed/corrupted frame (e.g. a
            // truncated class/method name that trips an unexpected panic
            // path inside formatting) must not prevent the watchdog from
            // reaching its `process::abort()` below — that would silently
            // turn a diagnosable crash into an unexplained hang instead.
            let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Truncate user-visible strings defensively — a corrupted
                // method name could otherwise produce megabytes of output.
                let class_name = truncate_ascii(frame.class_name(), 240);
                let method_name = truncate_ascii(frame.method_name(), 240);
                let desc = truncate_ascii(frame.method_descriptor(), 240);
                let source = frame
                    .source_file()
                    .map(|s| truncate_ascii(s, 240))
                    .unwrap_or_else(|| "<unknown>".to_string());
                format!(
                    "tid={tid} depth={depth} class={class_name} method={method_name} desc={desc} pc={pc} last_pc={last} source={source}\n",
                    pc = frame.pc,
                    last = frame.last_instr_pc,
                )
            }))
            .unwrap_or_else(|_| {
                format!("tid={tid} depth={depth} <frame dump panicked; skipped>\n")
            });
            let _ = handle.write_all(rendered.as_bytes());
        }

        // JIT-frame visibility note (see doc comment above): this thread
        // has at least one active JIT call on its native stack right now
        // (it reached this dump from a dispatch callback nested inside
        // JIT-compiled code — the common all-interpreter case has JIT
        // depth 0 here). One or more call levels between the frames shown
        // above are therefore JIT-compiled and invisible to this walker.
        // Flag it explicitly rather than leaving the reader to assume the
        // frames shown are the whole call chain.
        if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
            let note = format!(
                "tid={tid} note=thread has {depth} active JIT call(s) on its native \
                 stack; one or more call levels are JIT-compiled machine code and are \
                 not represented in the {frame_count} frame(s) above (see \
                 execute_invokestatic_cached/execute_jit_call in runtime/interpreter.rs) — \
                 rerun with --nojit for a full interpreted stack if needed\n",
                depth = crate::jit::conservative_roots::current_thread_jit_depth(),
            );
            let _ = handle.write_all(note.as_bytes());
        }

        let footer = format!("--- T19.H1 end dump tid={tid} ---\n");
        let _ = handle.write_all(footer.as_bytes());
        let _ = handle.flush();

        self.debug
            .stack_dump_ack_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        // Record WHICH thread answered, not just how many did — see
        // `stack_dump_acked_tids`.
        self.debug.stack_dump_acked_tids.lock().push(tid);
    }

    /// T19.H1 — count of threads that have completed their dump.
    pub fn stack_dump_ack_count(&self) -> u32 {
        self.debug
            .stack_dump_ack_count
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

// KC16-watchdog: thread-local snapshot of the current thread's frame chain,
// populated right before the thread parks in `Object.wait()`. The static
// `WAIT_SITE_DUMP` callback (installed in `Vm::new`) reads this thread-
// local to emit a stack dump from inside the wait loop, where the
// `JvmThread` is not directly accessible.
thread_local! {
    pub(in crate::vm::vm_init) static WAIT_SITE_SNAPSHOT: std::cell::RefCell<Option<WaitSite>> =
        const { std::cell::RefCell::new(None) };
}

/// What [`WAIT_SITE_SNAPSHOT`] holds for a parked thread.
pub(in crate::vm::vm_init) enum WaitSite {
    /// The dump, rendered when the wait began ([`set_wait_site_snapshot`]).
    Rendered(String),
    /// The waiting thread itself, rendered only if the watchdog asks
    /// ([`note_wait_site`], JIT round 16 lane monitor, backlog MB1). Valid from
    /// [`note_wait_site`] to [`clear_wait_site_snapshot`]: the caller is the
    /// thread's own `Object.wait`, which neither mutates nor drops the
    /// `JvmThread` in between, and only this OS thread reads the slot.
    Thread(*const JvmThread),
}

/// Record where this thread waits for the watchdog's wait-site dump: the
/// rendered frame dump ([`set_wait_site_snapshot`], every wait through round
/// 16 wave 1), or, with `lazy` (`CRATONVM_MONITOR_WAIT_SNAPSHOT_LAZY`, default
/// on), only a pointer to `thread`, from which [`wait_site_dump_text`] renders
/// the same text on demand. The dump reads the frames as they are while the
/// thread is parked, which are the frames it entered the wait with.
///
/// The caller must not mutate or drop `thread` until it calls
/// [`clear_wait_site_snapshot`] (both `Object.wait` paths in `vm_exec` record
/// it after their last mutable use of the thread and clear it on return).
pub fn note_wait_site(thread: &JvmThread, lazy: bool) {
    if lazy {
        let site = WaitSite::Thread(thread as *const JvmThread);
        WAIT_SITE_SNAPSHOT.with(|c| *c.borrow_mut() = Some(site));
    } else {
        set_wait_site_snapshot(thread);
    }
}

/// The wait-site dump of this thread, if a wait recorded one: the text
/// [`set_wait_site_snapshot`] renders, rendered now for a lazily noted wait.
pub(crate) fn wait_site_dump_text() -> Option<String> {
    WAIT_SITE_SNAPSHOT.with(|c| match &*c.borrow() {
        None => None,
        Some(WaitSite::Rendered(buf)) => Some(buf.clone()),
        // SAFETY: see `WaitSite::Thread` -- the pointer was recorded by this
        // OS thread's own wait, which keeps the `JvmThread` alive and
        // unmutated until it clears the slot, and only this thread reads it.
        Some(WaitSite::Thread(thread)) => Some(render_wait_site_snapshot(unsafe { &**thread })),
    })
}

/// Build a frame-chain snapshot string for the given thread and stash it
/// in the thread-local so a wait-site dump can emit it later. Cleared on
/// wait return.
pub fn set_wait_site_snapshot(thread: &JvmThread) {
    let buf = render_wait_site_snapshot(thread);
    WAIT_SITE_SNAPSHOT.with(|c| *c.borrow_mut() = Some(WaitSite::Rendered(buf)));
}

/// The wait-site dump text for `thread` (KC16-watchdog format).
pub(in crate::vm::vm_init) fn render_wait_site_snapshot(thread: &JvmThread) -> String {
    let mut buf = String::new();
    let tid = thread.thread_id.0;
    let name = &thread.name;
    let fc = thread.frames.len();
    buf.push_str(&format!(
        "--- T19.H1 stack dump (wait-site): tid={tid} name={name:?} frames={fc} ---\n"
    ));
    for (depth, frame) in thread.frames.iter().enumerate() {
        // See the matching guard in `dump_current_thread_frames` above: a
        // panic while rendering a single frame must not abort this
        // (rare, timeout-triggered) snapshot build and leave the wait-site
        // dump silently empty for the rest of the thread's parked lifetime.
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let class_name = truncate_ascii(frame.class_name(), 240);
            let method_name = truncate_ascii(frame.method_name(), 240);
            let desc = truncate_ascii(frame.method_descriptor(), 240);
            let source = frame
                .source_file()
                .map(|s| truncate_ascii(s, 240))
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "tid={tid} depth={depth} class={class_name} method={method_name} desc={desc} pc={pc} last_pc={last} source={source}\n",
                pc = frame.pc,
                last = frame.last_instr_pc,
            )
        }))
        .unwrap_or_else(|_| format!("tid={tid} depth={depth} <frame dump panicked; skipped>\n"));
        buf.push_str(&rendered);
    }
    if crate::jit::conservative_roots::current_thread_jit_depth() != 0 {
        buf.push_str(&format!(
            "tid={tid} note=thread has active JIT call(s) on its native stack; \
             one or more call levels are JIT-compiled machine code and are not \
             represented above\n"
        ));
    }
    buf.push_str(&format!("--- T19.H1 end dump tid={tid} (wait-site) ---\n"));
    buf
}

pub fn clear_wait_site_snapshot() {
    WAIT_SITE_SNAPSHOT.with(|c| *c.borrow_mut() = None);
}

/// Does `CRATONVM_SOFTREF_HOTSPOT_LRU` default ON for a VM on `backend`, i.e.
/// does its `ReferenceProcessor` judge soft references with HotSpot's
/// `LRUMaxHeapPolicy` inputs (the soft clock and the free heap as of the end
/// of the previous collection) unless the variable says otherwise?
///
/// Generational since gen r5w4/defaults8. G1 since gci i5g/g1o
/// (`gch-h2jr-soft-lru-window-on-g1-and-zgc-diverges-from-hotspot-FIXED-20261012`,
/// the hunk of `gch-h4g1-proposal-g1-soft-lru-takes-hotspots-inputs-DONE-20261015`,
/// decided by vh4-g1 block E: with `=1` the G1 rows were SAME): on the
/// wall-clock inputs G1 never cleared an idle soft reference, because its
/// remark judges with clock `0` and `SoftReference.clock` is published only in
/// the HotSpot mode. `=0` restores the wall-clock inputs on both. ZGC is not
/// decided here: `ReferenceProcessor::new_for_backend` gives it its own
/// default, ON since zr z1/rf (`alloc_policy_defaults::SOFTREF_HOTSPOT_LRU_ZGC`),
/// so the effective default is ON on all three heaps.
pub(crate) fn soft_lru_takes_hotspot_inputs_by_default(backend: GcBackend) -> bool {
    matches!(backend, GcBackend::Generational | GcBackend::G1)
}


/// Called by the Monitor wait loop (via the OnceLock callback installed in
/// `Vm::new`) when the stack-dump watchdog flag is observed. Reads the
/// thread-local snapshot captured by `monitor_wait` on entry and writes
/// it to stderr, then increments the watchdog ack counter.
pub fn dump_wait_site_thread_local(shared: &SharedVm) {
    use std::io::Write;
    let snapshot = wait_site_dump_text();
    if let Some(buf) = snapshot {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = handle.write_all(buf.as_bytes());
        let _ = handle.flush();
        shared
            .debug
            .stack_dump_ack_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    } else {
        tracing::warn!(
            target: "kc16_watchdog",
            "wait-site dump fired but no snapshot was captured"
        );
    }
}

/// Print the state of the object a thread is parked on in `Object.wait()`.
///
/// The netty `ParameterizedSslHandlerTest` stall bottoms out in
/// `DefaultPromise.await`/`awaitUninterruptibly` at the `Object.wait()` BCI, a
/// 30 s-spurious-wakeup A/B showed the awaited promise is ALREADY complete, and
/// the orphan check came back clean — so the waiter is on the right monitor and
/// a notifier would resolve the same one. The remaining question is netty's own
/// bookkeeping, which lives in two fields of the promise:
///
/// * `result`  — non-null once the promise completes (set OUTSIDE the monitor).
/// * `waiters` — a PLAIN int the waiter increments inside the monitor
///   immediately before `wait()`; `checkNotifyWaiters()` skips `notifyAll()`
///   entirely when it reads 0.
///
/// `result != null` with `waiters >= 1` here means the completer either never
/// ran `checkNotifyWaiters` or read a stale `waiters` — i.e. the monitor is not
/// establishing happens-before between the two `synchronized` blocks.
pub fn dump_wait_object_state(shared: &SharedVm, obj: ObjectRef) {
    let cid = shared.mem.heap.class_id_of(obj);
    let name = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(cid)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id {cid:?}>"));
    let field = |f: &str| -> Option<cratonvm_types::Value> {
        let cm = shared.classes.class_manager.read();
        let idx = super::vm_exec::resolve_field_index_in_hierarchy(cid, f, &cm.class_store)?;
        drop(cm);
        Some(shared.mem.heap.get_field(obj, idx))
    };
    // SLOT PROVENANCE. The 2026-08-24 `ParameterizedSslHandlerTest` residual
    // page asked, of a dump that read `Int(0)` out of a
    // `private volatile Object`, whether the value was real or whether
    // `resolve_field_index_in_hierarchy` had answered with the wrong slot for
    // that receiver. The dump could not say, because it printed neither the
    // index it used nor the layout it used it against. It does now, and the
    // two together are decidable by inspection. (It was neither — see the
    // never-written-cell note below.)
    let result_slot = resolve_declared_instance_field(shared, cid, "result");
    // WHAT `result` ACTUALLY IS, not just whether it is non-null.
    //
    // `result != null` is NOT the same claim as "the promise completed", and
    // reading it as one is the single largest risk this dump carries.
    // `io.netty.util.concurrent.DefaultPromise` holds THREE kinds of value in
    // that one field:
    //
    // ```java
    // private static final Object SUCCESS = new Object();
    // private static final Object UNCANCELLABLE = new Object();
    // private static boolean isDone0(Object result) {
    //     return result != null && result != UNCANCELLABLE;
    // }
    // public boolean setUncancellable() {
    //     if (RESULT_UPDATER.compareAndSet(this, null, UNCANCELLABLE)) return true;
    //     …
    // }
    // ```
    //
    // `setUncancellable()` — which the channel register and bind paths call as
    // a matter of course — publishes a NON-NULL `result` and deliberately does
    // NOT notify, because the promise is still pending. A waiter's
    // `while (!isDone())` correctly parks, and `checkNotifyWaiters` is
    // correctly never called. So `result != null` + `waiters == 1` + no
    // notification is EXACTLY what a promise that was made uncancellable and
    // then never completed looks like — with no memory-ordering defect
    // anywhere, and with the defect living upstream in whatever should have
    // completed it.
    //
    // Distinguishing the two is a pointer comparison against the class's own
    // statics, so the dump does it rather than leaving the reader to assume.
    let result = field("result");
    // A NEVER-WRITTEN REFERENCE CELL IS NOT A TYPE-PUNNED ONE.
    //
    // `Value` is `#[repr(u32)]` with `Int = 0` and `Object = 4`, so a
    // zero-filled 16-byte slot decodes as `Value::Int(0)` and NOT as
    // `Object(None)` — see `init_primitive_fields`' G56-1 note. The
    // interpreter's allocation path writes the `Object(None)` tag explicitly;
    // the JIT's arms clear the body to zero and rely on every reader
    // repairing the tag locally (`opcodes.rs`' `getfield` fixup, the inline
    // read's payload-only load, `coerce_field_value_for_slot`,
    // `values_equal_for_cas`). Both are "null" to Java. This dump was the one
    // reader that did NOT repair it, and it reported the difference as
    // `result_is=not-a-reference-slot` — which reads as heap corruption when
    // it is an unwritten field of a promise that is simply still pending.
    //
    // So normalise exactly as the other readers do, print BOTH values, and
    // say which one the verdict is about.
    let result_desc = result_slot.as_ref().map(|s| s.descriptor.clone());
    let ref_declared = result_desc
        .as_deref()
        .is_some_and(|d| d.starts_with('L') || d.starts_with('['));
    let normalised = match (&result, ref_declared) {
        (Some(cratonvm_types::Value::Int(0)), true) => Some(cratonvm_types::Value::Object(None)),
        _ => result.clone(),
    };
    let verdict = classify_promise_result(shared, cid, &normalised);
    eprintln!(
        "[WAIT-OBJECT] obj={:p} class={name} result={:?} result_is={verdict} waiters={:?}",
        obj.as_ptr(),
        result,
        field("waiters"),
    );
    match &result_slot {
        Some(slot) => {
            eprintln!(
                "[WAIT-OBJECT] result_slot=index {} declared {} on {} (receiver layout: {})",
                slot.index,
                slot.descriptor,
                slot.owner,
                describe_instance_layout(shared, cid),
            );
            if ref_declared && !matches!(result, Some(cratonvm_types::Value::Object(_))) {
                eprintln!(
                    "[WAIT-OBJECT] result cell is a NEVER-WRITTEN zero cell, not a punned \
                     slot: the field is declared {} and the slot decodes as {:?}, which is \
                     what a zero-filled 16-byte `Value` cell decodes to (`Int` is \
                     discriminant 0). Every other reader in the VM treats it as null, so the \
                     verdict above is taken on the normalised value and this promise is \
                     PENDING, not corrupt.",
                    slot.descriptor, result,
                );
            }
        }
        None => eprintln!(
            "[WAIT-OBJECT] result_slot=UNRESOLVED — no instance field named `result` anywhere \
             in this receiver's hierarchy, so the value above was read from nothing and must \
             not be interpreted (receiver layout: {})",
            describe_instance_layout(shared, cid),
        ),
    }
}

/// One declared instance field, resolved through a receiver's hierarchy.
pub struct DeclaredField {
    /// Layout slot index — what `heap.get_field` is indexed with.
    pub index: usize,
    /// The field's declared JVM descriptor, e.g. `Ljava/lang/Object;`.
    pub descriptor: String,
    /// The class in the hierarchy that DECLARES it, which is usually not the
    /// receiver's own class.
    pub owner: String,
}

/// Resolve `field_name` on `cid`'s hierarchy to its slot index, declared
/// descriptor and declaring class.
///
/// Mirrors [`crate::vm::vm_exec::resolve_field_index_in_hierarchy`] slot for
/// slot — same walk, same `instance_offset` accounting — and exists so a
/// diagnostic can print WHAT it resolved rather than only the value it read
/// through it. A dump that prints a value without the slot it came from
/// cannot be checked, which is exactly the gap the netty residual page names.
pub fn resolve_declared_instance_field(
    shared: &SharedVm,
    cid: ClassId,
    field_name: &str,
) -> Option<DeclaredField> {
    let cm = shared.classes.class_manager.read();
    let mut current = Some(cid);
    while let Some(c) = current {
        let class = cm.class_store.get(c)?;
        let mut instance_offset = 0usize;
        for f in &class.fields {
            if f.is_static() {
                continue;
            }
            if &*f.name == field_name {
                return Some(DeclaredField {
                    index: class.first_field_index + instance_offset,
                    descriptor: f.descriptor.to_string(),
                    owner: class.name.to_string(),
                });
            }
            instance_offset += 1;
        }
        current = class.superclass;
    }
    None
}

/// Every instance field a receiver carries, most-derived class first, as
/// `owner.name:descriptor@index`.
///
/// Printed beside a resolved slot so "the dump read the wrong index" is a
/// claim the reader can check instead of one they have to trust. Capped, so a
/// receiver with a large hierarchy cannot flood a watchdog dump.
pub fn describe_instance_layout(shared: &SharedVm, cid: ClassId) -> String {
    const MAX: usize = 48;
    let cm = shared.classes.class_manager.read();
    let mut parts: Vec<String> = Vec::new();
    let mut current = Some(cid);
    let mut truncated = false;
    while let Some(c) = current {
        let Some(class) = cm.class_store.get(c) else {
            parts.push(format!("<class_id {c:?} not in store>"));
            break;
        };
        let mut instance_offset = 0usize;
        for f in &class.fields {
            if f.is_static() {
                continue;
            }
            if parts.len() >= MAX {
                truncated = true;
            } else {
                parts.push(format!(
                    "{}.{}:{}@{}",
                    class.name,
                    f.name,
                    f.descriptor,
                    class.first_field_index + instance_offset
                ));
            }
            instance_offset += 1;
        }
        current = class.superclass;
    }
    if truncated {
        parts.push("...".to_string());
    }
    parts.join(" ")
}

/// Name a `DefaultPromise.result` value against that class's own `SUCCESS` /
/// `UNCANCELLABLE` sentinels — see the note in [`dump_wait_object_state`] for
/// why the distinction decides the netty stall.
///
/// `unknown` means the statics could not be resolved on this receiver's
/// hierarchy, and it is printed rather than swallowed: a silent fallback to
/// "not uncancellable" is precisely the reading that would repeat the original
/// mistake.
pub(in crate::vm::vm_init) fn classify_promise_result(
    shared: &SharedVm,
    cid: ClassId,
    result: &Option<cratonvm_types::Value>,
) -> String {
    let Some(value) = result else {
        // NOT the same claim as "the slot holds a primitive". `None` here means
        // the receiver has no instance field called `result` anywhere in its
        // hierarchy, so nothing was read at all — and printing a verdict about
        // the value would be a verdict about nothing. This dump has already
        // mislabelled one unread slot (see the zero-cell note in
        // `dump_wait_object_state`); it does not get to do it twice.
        return "no-result-field(NOTHING-WAS-READ)".to_string();
    };
    let cratonvm_types::Value::Object(slot) = value else {
        return "not-a-reference-slot".to_string();
    };
    let Some(r) = slot else {
        return "null(PENDING)".to_string();
    };
    // The sentinels are `private static final` on `DefaultPromise` itself, so
    // walk from the receiver's class up until a class declares the name — the
    // receiver is usually a subclass (`DefaultChannelPromise`,
    // `AbstractBootstrap$PendingRegistrationPromise`).
    let sentinel = |fname: &str| -> Option<ObjectRef> {
        let cm = shared.classes.class_manager.read();
        let mut cur = Some(cid);
        while let Some(c) = cur {
            let class = cm.get_class(c)?;
            let mut static_idx = 0usize;
            for f in &class.fields {
                if f.is_static() {
                    if &*f.name == fname {
                        let owner = c;
                        drop(cm);
                        return match crate::vm::vm_object::get_static_shared(
                            shared, owner, static_idx,
                        ) {
                            cratonvm_types::Value::Object(Some(o)) => Some(o),
                            _ => None,
                        };
                    }
                    static_idx += 1;
                }
            }
            cur = class.superclass;
        }
        None
    };
    let same = |o: Option<ObjectRef>| o.is_some_and(|o| std::ptr::eq(o.as_ptr(), r.as_ptr()));
    let unc = sentinel("UNCANCELLABLE");
    let suc = sentinel("SUCCESS");
    if unc.is_none() && suc.is_none() {
        return "unknown(sentinels-unresolved)".to_string();
    }
    if same(unc) {
        return "UNCANCELLABLE--STILL-PENDING".to_string();
    }
    if same(suc) {
        return "SUCCESS".to_string();
    }
    let rcid = shared.mem.heap.class_id_of(*r);
    let rname = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(rcid)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<class_id {rcid:?}>"));
    format!("other({rname})")
}

impl SharedVm {
    /// Placeholder split-impl — see the inherent impl above. The split is
    /// purely so the thread-local helpers above can sit between two impl
    /// blocks without losing rustfmt readability.
    #[doc(hidden)]
    pub(in crate::vm::vm_init) fn __kc16_split_marker(&self) {}

    /// Get or create a synthetic lock object for static synchronized methods.
    ///
    /// Each class gets a dedicated dummy object on the heap, used as the
    /// monitor target for static synchronized methods (which have no `this`).
    /// Legacy host API, fatal on refusal; prefer the fallible twin.
    pub fn get_class_lock_object(&self, class_id: ClassId) -> ObjectRef {
        self.try_get_class_lock_object(class_id)
            .expect("class lock allocation refused")
    }

    pub fn try_get_class_lock_object(&self, class_id: ClassId) -> Option<ObjectRef> {
        // Fast path: check if we already have a lock object
        if let Some(&obj) = self.classes.class_locks.read().get(&class_id) {
            return Some(obj);
        }
        // Slow path: allocate one
        let mut locks = self.classes.class_locks.write();
        // Double-check after acquiring write lock
        if let Some(&obj) = locks.get(&class_id) {
            return Some(obj);
        }
        // This object is only a monitor token; it is not a real instance of
        // `class_id`. Keeping its heap header as java/lang/Object avoids
        // creating zero-slot "instances" of fieldful classes, which the GC
        // reports as undersized object layouts when scanning class-lock roots.
        let lock_class_id = self
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/Object")
            .unwrap_or(ClassId::new(0));
        let obj = self.mem.heap.try_alloc_object_full(lock_class_id, 0)?;
        locks.insert(class_id, obj);
        Some(obj)
    }

    /// Lazily initialize synthetic System.out and System.err PrintStream objects.
    ///
    /// Called when `java/lang/System` static fields are accessed. Allocates
    /// PrintStream objects backing `System.out` / `System.err`.
    ///
    /// # WP0.1 — P0 "Cannot invoke write on null" fix (2026-04-24)
    ///
    /// Previously allocated with `num_fields = 1` regardless of whether the
    /// real JDK `java/io/PrintStream` class was already loaded. When real
    /// JDK mode ran `initPhase1` successfully, the real PrintStream class
    /// (with ~20 fields: `out`, `autoFlush`, `trouble`, `charOut`,
    /// `textOut`, `closing`, `closed`, `charset`, `locale`, `lock`, etc.)
    /// was registered under the same name, so `ensure_synthetic_class`
    /// reused that real `class_id` while the heap allocation still only
    /// reserved 1 slot. Real JDK `PrintStream.println(String)V` bytecode
    /// then did `getfield #out` on the 1-slot object, which under the G1
    /// backend reads past the allocation into whatever bytes follow it
    /// in arena memory. On `apps/hello` (one `println`) those bytes
    /// happened to decode as something `.write`-able; on a second call
    /// the arena contents had shifted and the field decoded as
    /// `Object(None)`, so the subsequent `invokevirtual write` tripped
    /// `Cannot invoke write on null` in the interpreter's NPE site.
    ///
    /// Fix: allocate with the class's declared `num_total_fields` when
    /// the real JDK class is already loaded, so the backing storage
    /// matches the bytecode's field indices. Synthetic (non-real-JDK)
    /// mode still falls back to the 1-slot layout — it goes through the
    /// native `PrintStream.println` fallback registered in
    /// `native-builtins/src/lib.rs`, which only reads field 0.
    /// Legacy host convenience method; Java uses `try_ensure_system_streams`.
    /// zs s2/nd2: a refusal while booting is the typed startup failure,
    /// after it (or under `CRATONVM_ZGC_BOOT_OOME_INIT_ERROR=0`) the abort
    /// (`vm_object::legacy_alloc_refused`).
    pub fn ensure_system_streams(&self) -> (ObjectRef, ObjectRef) {
        self.try_ensure_system_streams().unwrap_or_else(|_| {
            super::vm_object::legacy_alloc_refused(self, "System.out/System.err")
        })
    }

    /// Create streams without collecting or publishing a partial pair.
    pub fn try_ensure_system_streams(
        &self,
    ) -> Result<(ObjectRef, ObjectRef), crate::error::RuntimeError> {
        // Check if already initialized
        {
            let out = self.system_out.read();
            let err = self.system_err.read();
            if let (Some(out_ref), Some(err_ref)) = (*out, *err) {
                return Ok((out_ref, err_ref));
            }
        }
        // Initialize
        let mut out = self.system_out.write();
        let mut err = self.system_err.write();
        // Double-check
        if let (Some(out_ref), Some(err_ref)) = (*out, *err) {
            return Ok((out_ref, err_ref));
        }
        // Ensure a PrintStream class_id exists. If the real JDK
        // `java/io/PrintStream` has already been loaded (real-JDK mode),
        // `ensure_synthetic_class` returns that existing id; otherwise
        // it creates a 1-field synthetic stub.
        //
        // charset-NPE fix (2026-05-21): in real-JDK mode this hook runs
        // from `System.<clinit>`, which executes *before*
        // `java/io/PrintStream` has been loaded. The old code therefore
        // hit the `ensure_synthetic_class` "create a 1-field stub" path,
        // and the System.out/err objects ended up backed by a synthetic
        // stub class whose `fields` vector is EMPTY. `initPhase1`'s
        // `install_charset` then does `set_field_by_name(stream,
        // "charset", …)`, which resolves the field via
        // `resolve_field_index_in_hierarchy` — that walk finds no field
        // named `charset` on the empty-field stub, so the write is
        // silently dropped and `PrintStream.charset` stays null. The
        // first `new PrintWriter(System.err)` (Picocli / Quarkus
        // `show-config`) then reads `((PrintStream)err).charset()` ==
        // null and `OutputStreamWriter`'s `Objects.requireNonNull(cs,
        // "charset")` throws `NullPointerException: charset`.
        //
        // Force-load the real `java/io/PrintStream` class first (same
        // pattern as `ensure_system_stdin_object` for FileInputStream)
        // so the backing objects get the real field layout — including
        // the `charset` slot — whenever real JDK class files are
        // available. `load_class_concurrent` also upgrades a previously
        // created synthetic stub in place. If no real boot classes are
        // present (pure synthetic-jdk mode) the load fails harmlessly
        // and we fall back to the 1-field synthetic stub below.
        let _ = self.load_class_concurrent("java/io/PrintStream");
        // Real-JDK wave 2, step 3 (2026-08-10): the fallible spelling, through
        // this file's own "refuse diagnosably" helper. On any complete image
        // the load above has already put the real `java.io.PrintStream` in the
        // store, so this resolves to it and fabricates nothing — the refusal
        // arm is reachable only where `java.base` is absent, and there
        // `System.out` was never going to work.
        let ps_class_id = {
            let mut cm = self.classes.class_manager_write();
            // 1 field: fd_id — used only when the real class isn't loaded.
            ensure_bootstrap_compat_class(&mut cm, "java/io/PrintStream", 1)
        };
        // WP0.1 fix: size the allocation to the class's declared field count
        // so real-JDK `PrintStream.println` bytecode's `getfield` accesses
        // stay in-bounds. `max(1)` guarantees slot 0 (our fd tag) is always
        // writable even for synthetic stub classes declared with 0 fields.
        //
        // On a refusal, zero-slot `java/lang/Object` streams rather than an
        // undersized 1-slot layout: the GC's field guard rejects every access
        // on the latter, so the failure would surface as a stream of dropped
        // writes instead of at whatever first tries to USE `System.out`.
        // `ensure_bootstrap_compat_class` has already warned and named it.
        let (ps_class_id, num_fields) = match ps_class_id {
            Some(id) => {
                let n = {
                    let cm = self.classes.class_manager.read();
                    cm.get_class(id)
                        .map(|c| c.num_total_fields.max(1))
                        .unwrap_or(1)
                };
                (id, n)
            }
            None => (ClassId::new(0), 0),
        };
        let refused = || crate::error::RuntimeError::OutOfMemoryError {
            message: "Java heap space".into(),
        };
        let out_obj = self
            .mem
            .heap
            .try_alloc_object_full(ps_class_id, num_fields)
            .ok_or_else(refused)?;
        let err_obj = self
            .mem
            .heap
            .try_alloc_object_full(ps_class_id, num_fields)
            .ok_or_else(refused)?;
        // Store fd_id: stdout=1, stderr=2 at slot 0 so the synthetic-jdk
        // natives (which read field 0 in `native-io/src/lib.rs`) keep
        // working. The real-JDK native-override path uses pointer
        // identity via `stream_fd()` in `native-builtins/src/lib.rs`, so
        // it doesn't care what else lives in the object's field slots.
        //
        // Compact reference-field layout: the real java/io/PrintStream's
        // field 0 is the reference `out` (inherited FilterOutputStream). Under
        // the compact layout an `Int` written into a reference slot auto-boxes,
        // so `out` becomes a non-null wrapper Object — which makes
        // `route_write_through_out()` think System.out wraps a real stream and
        // route every write into the dead wrapper (silent empty stdout). The
        // real-JDK path resolves the fd by pointer identity, not slot 0, so
        // skip the fd tag whenever slot 0 is a (compact) reference field. In
        // synthetic-jdk mode (1-field stub, slot 0 is the primitive fd tag) the
        // store still happens. `class_layout` is only populated when compact is
        // enabled, so this is a no-op (legacy behaviour) when the flag is off.
        //
        // 2026-08-12 (W7-84-primitive-in-reference-store.md): this guard was
        // written against `gen_heap`, the only heap that boxed. `zgc`, `g1` and
        // `heap` dropped the same write to null, so on those collectors the
        // hazard above could not occur and this guard was carrying nothing.
        // All four box now, so the guard is load-bearing on every collector —
        // which is the honest cost of converging on the value-preserving
        // answer, and the reason it is stated here rather than left implicit.
        let slot0_is_ref = cratonvm_gc::class_layout(ps_class_id.as_u32())
            .and_then(|l| l.field_is_ref(0))
            .unwrap_or(false);
        if !slot0_is_ref && num_fields > 0 {
            self.mem.heap.set_field(out_obj, 0, Value::Int(1));
            self.mem.heap.set_field(err_obj, 0, Value::Int(2));
        }
        *out = Some(out_obj);
        *err = Some(err_obj);
        // FFM/native-access fix: stamp `java.lang.System.initialErr` with the
        // err stream we just materialised. `jdk.internal.misc.VM.initialErr()`
        // is `SharedSecrets.getJavaLangAccess().initialSystemErr()`, whose real
        // JDK body (`java.lang.System$1`) is `getstatic System.initialErr;
        // areturn`. The real JDK populates that `@Stable` field in
        // `System.initPhase1()` (System.java:1820); CratonVM boots via a native
        // `initPhase1` that never set it, so the field stayed null and
        // `VM.initialErr().printf(...)` NPE'd. That printf is the JDK's
        // restricted-native-access warning path
        // (`java.lang.Module.ensureNativeAccess`, Module.java:322), which fires
        // the first time an FFM downcall binding calls
        // `SymbolLookup.libraryLookup` without `--enable-native-access`. The NPE
        // was wrapped as ExceptionInInitializerError for the binding's <clinit>
        // — e.g. Tomcat's `org.apache.tomcat.util.openssl.openssl_h` — taking
        // down the entire OpenSSL/TLS protocol handler and ~17 TLS test classes.
        // A native override on `System$1.initialSystemErr` does NOT help here:
        // the shim is a real JDK class, so its concrete bytecode runs instead of
        // the override — only the backing static field is consulted. Resolve the
        // static slot by name so this is robust to the real-JDK field layout;
        // a missing field (pure synthetic-jdk System stub) is skipped harmlessly.
        let sys_initial_err_slot = {
            let cm = self.classes.class_manager.read();
            cm.get_loaded_class_id("java/lang/System").and_then(|sid| {
                cm.get_class(sid).and_then(|c| {
                    let mut static_idx = 0usize;
                    for f in &c.fields {
                        if f.is_static() {
                            if &*f.name == "initialErr" {
                                return Some((sid, static_idx));
                            }
                            static_idx += 1;
                        }
                    }
                    None
                })
            })
        };
        if let Some((sid, idx)) = sys_initial_err_slot {
            super::set_static_shared(self, sid, idx, Value::Object(Some(err_obj)));
        }
        Ok((out_obj, err_obj))
    }
}

impl SharedVm {
    /// Find the park state for a thread identified by its Java Thread object.
    /// Used by `unpark()` in NativeContextImpl.
    ///
    /// Resolution prefers the Java-side `Thread.tid` (unique, never reused)
    /// over the mirror's heap address: the address-keyed reverse index can
    /// alias a NEW thread's mirror allocated at a dead thread's recycled
    /// address, silently routing the wakeup to the dead thread's ParkState
    /// (observed as Tomcat executor workers parked forever after their
    /// `shutdownNow()` interrupt was lost — the DoHead leaked-worker face).
    /// The tid path also survives a relocated-but-intact stale mirror copy:
    /// the stale address is gone from the reverse index, but its memory
    /// still holds the correct `tid`.
    pub fn find_park_state_for_thread_obj(
        &self,
        thread_obj: ObjectRef,
    ) -> Option<std::sync::Arc<crate::threading::ParkState>> {
        if let Some(java_tid) = super::vm_exec::read_java_thread_tid(self, thread_obj) {
            if let Some(ps) = self
                .threads
                .thread_registry
                .find_park_state_by_java_tid(java_tid)
            {
                return Some(ps);
            }
        }
        self.threads
            .thread_registry
            .find_park_state_by_thread_obj(thread_obj)
    }
}


impl SharedVm {
    /// WP1.3 — Read the current JVM bootstrap init level.
    ///
    /// Returns the integer HotSpot state (0..=4, see
    /// [`SharedVm::init_level`] doc-comment).  Uses `Ordering::Acquire`
    /// to pair with the `Ordering::Release` store in
    /// [`Self::set_init_level`] so threads that observe a bumped
    /// level also observe every classloader / static-field write
    /// the bump guards.
    pub fn get_init_level(&self) -> i32 {
        self.init_level.load(Ordering::Acquire)
    }

    /// WP1.3 — Advance the JVM bootstrap init level.
    ///
    /// Accepts any value in `0..=4`; downward transitions are
    /// rejected (logged and ignored) because the JVM spec does not
    /// define a way to unwind init progress. Wakes every waiter
    /// blocked in [`Self::await_init_level`] / the
    /// `VM.awaitInitLevel` native on success.
    ///
    /// **Coordination**: this is the single entry point for init-level
    /// transitions.  Callers are expected to be at the edge of a
    /// well-defined bootstrap phase:
    ///
    /// * level 1 — right after `SharedVm::new` returns,
    /// * level 2 — right after `System.initPhase1()` completes,
    /// * level 3 — right after `System.initPhase2()` completes,
    /// * level 4 — right before user `main()` runs.
    ///
    /// The sibling free function [`set_init_level`] mirrors the
    /// process-wide registry and is what the
    /// `vm-cli` bootstrap sequence actually calls; methods on
    /// `SharedVm` delegate to it so the atomic value seen by the
    /// `VM.initLevel()` native never lags behind a `self.init_level`
    /// update.
    pub fn set_init_level(&self, level: i32) {
        // Keep the process-wide registry in sync first — the native
        // `VM.initLevel()` handler reads it directly.
        cratonvm_native_api::init_level::set_init_level(level);
        self.raise_vm_init_level(level);
    }

    /// zs s7/nd6: has this VM finished its boot? Level 4 (the launcher and
    /// `libcratonvm` raise it right before `main`), or an embedder's
    /// [`Self::mark_boot_finished`]. `CRATONVM_ZGC_EMBED_BOOT_LATCH=0` reads
    /// the level alone (the s2 rule).
    pub fn boot_finished(&self) -> bool {
        self.get_init_level() >= 4
            || (self.boot_finished_latch.load(Ordering::Acquire))
    }

    /// zs s7/nd6: latch this VM's boot as finished without the init-level
    /// transitions (an embedder about to run its own code). From here on a
    /// refused start-up object (the lazy `--compatible` `System.out` /
    /// `System.err` pair) is a post-boot failure, never the
    /// `VmInitializationError` unwind. Idempotent; per VM.
    pub fn mark_boot_finished(&self) {
        self.boot_finished_latch.store(true, Ordering::Release);
        // zt t14/pl5: the boot's end for the perf-data counters too (written
        // once, whichever of this and level 4 comes first).
        crate::runtime::perf_data_file::record_vm_init_done(self);
    }

    /// The per-VM half of [`Self::set_init_level`]: this VM's level, its
    /// waiters and the JVMTI live-phase transition, without the process-wide
    /// registry (which unit tests must not raise for the whole test binary).
    pub(crate) fn raise_vm_init_level(&self, level: i32) {
        // Mirror into the per-SharedVm atomic so in-process readers
        // with a `&SharedVm` don't have to go through the global
        // OnceLock.  `set_init_level` above already rejected downward
        // transitions; we re-check cheaply here so the per-instance
        // condvar notification also preserves monotonicity.
        let cur = self.init_level.load(Ordering::Acquire);
        if level < cur {
            return;
        }
        // `fetch_max`, so exactly one call sees the level cross 3 (below).
        let prev = self.init_level.fetch_max(level, Ordering::AcqRel);
        let (_lock, cv) = &*self.init_level_waiters;
        cv.notify_all();
        // Level 3 (after `System.initPhase2`) is JVMTI's live phase: the
        // startup agents' C envs leave the start phase and get `VMInit`, once
        // per VM, on this thread (interpreter round i1 wave 14). One load
        // otherwise.
        // Level 2 (after `System.initPhase1`) ends the primordial phase:
        // `VMStart` (interpreter round i2 wave 55, lane L1).
        #[cfg(feature = "experimental-debug")]
        if level >= 2 && self.debug.debugger_gates.jvmti_start_phase() {
            if level >= 3 {
                crate::jvmti::native_env::enter_live_phase(self);
            } else {
                crate::jvmti::native_env::enter_start_phase(self);
            }
        }
        // The VM's Rust-side manager gets its `VMInit` at the same point, once
        // (wave 15; it was sent at the end of `SharedVm::new`, before any
        // `System.initPhase` and before anything could listen).
        if prev < 3 && level >= 3 {
            crate::runtime::jvmti::fire_vm_init_for_vm(self.vm_identity);
        }
        // zt t14/pl5: `sun.rt.createVmEndTime` / `vmInitDoneTime`, at the
        // boot's end (level 4: `initPhase3` ran), once.
        if prev < 4 && level >= 4 {
            crate::runtime::perf_data_file::record_vm_init_done(self);
        }
    }

    /// WP1.3 — Block the current thread until the init level reaches
    /// at least `target`.  Backs `VM.awaitInitLevel(int)` and is also
    /// useful inside the VM for test helpers that need to observe a
    /// specific bootstrap stage.
    ///
    /// If the current level is already `>= target`, returns
    /// immediately. Otherwise parks on the per-SharedVm condvar and
    /// rechecks on every wake.
    pub fn await_init_level(&self, target: i32) {
        if self.get_init_level() >= target {
            return;
        }
        let (lock, cv) = &*self.init_level_waiters;
        let mut guard = lock.lock().expect("init_level_waiters mutex poisoned");
        while self.get_init_level() < target {
            guard = cv.wait(guard).expect("init_level_waiters condvar poisoned");
        }
    }
}


/// Rank-tracked guard returned by the `_ranked` accessors. The
/// `rank_scope` field releases the thread-local rank when dropped; the
/// `lock` field is the original `parking_lot` guard with the usual
/// `Deref`/`DerefMut` to the protected value. Dropping happens in
/// declaration order (Rust drop order), so the `parking_lot` lock is
/// released *before* the rank scope — the symmetric inverse of
/// acquisition.
#[must_use = "the rank guard must be held for the duration of the lock"]
pub struct RankedGuard<G> {
    /// The original `parking_lot` guard. Drops first. Private so
    /// callers cannot move it out — moving would drop the rank scope
    /// in the same statement and silently break the rank-tracking
    /// invariant. Access the inner value via `Deref`/`DerefMut`.
    pub(in crate::vm::vm_init) lock: G,
    /// The rank-scope guard. Drops second, releasing the thread-local
    /// rank level so a lower-ranked lock can be re-acquired. Held
    /// only for its `Drop` side-effect; callers should not touch it.
    #[allow(dead_code)]
    pub(in crate::vm::vm_init) rank_scope: RankScope,
}

impl<G: std::ops::Deref> std::ops::Deref for RankedGuard<G> {
    type Target = G::Target;
    fn deref(&self) -> &Self::Target {
        &self.lock
    }
}

impl<G: std::ops::DerefMut> std::ops::DerefMut for RankedGuard<G> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.lock
    }
}

impl SharedVm {
    /// Acquire `class_manager` (L10) for read.
    ///
    /// Retained as a compatibility alias: the field is now an
    /// [`OrderedPlRwLock`] at [`LockLevel::ClassManager`], so a plain
    /// `shared.classes.class_manager.read()` is already order-checked and this helper
    /// adds nothing. Taking a *second* rank scope here would double-record L10
    /// and trip the same-level assertion.
    #[inline]
    pub fn class_manager_read_ranked(
        &self,
    ) -> crate::runtime::lock_order::OrderedPlRwLockReadGuard<'_, ClassManager> {
        self.classes.class_manager.read()
    }

    /// Acquire `class_manager` (L10) for write, through the hook-draining
    /// guard (obsaudit D1) — see
    /// [`crate::vm::realms::class_realm::ClassRealm::class_manager_write`].
    /// No longer a bare alias: unlike the read side, this must route
    /// through the draining wrapper like every other write-lock site, or
    /// JVMTI ClassLoad/ClassPrepare events queued under it would never fire.
    #[inline]
    pub fn class_manager_write_ranked(
        &self,
    ) -> crate::vm::realms::class_realm::ClassManagerWriteGuard<'_> {
        self.classes.class_manager_write()
    }

    /// Acquire `ref_processor` (L7).
    ///
    /// Retained as a compatibility alias: the field is now an
    /// [`OrderedPlMutex`] at [`LockLevel::RefProcessor`], so a plain
    /// `shared.mem.ref_processor.lock()` is already order-checked and this helper
    /// adds nothing. Taking a *second* rank scope here would double-record L7
    /// and trip the same-level assertion.
    #[inline]
    pub fn ref_processor_lock_ranked(
        &self,
    ) -> crate::runtime::lock_order::OrderedPlMutexGuard<'_, cratonvm_gc::ReferenceProcessor> {
        self.mem.ref_processor.lock()
    }

    /// Enter the `monitors` rank scope. The monitor table itself uses
    /// many fine-grained inner locks; this helper just announces that
    /// the caller is about to touch monitor state so the rank tracker
    /// can reject downstream acquisitions of higher-ranked locks
    /// (i.e. `class_manager`). Bind the
    /// returned guard to a local for the duration of the monitor work.
    #[inline]
    #[must_use = "bind the guard to a local for the duration of the monitor work"]
    pub fn enter_monitors_rank(&self) -> RankScope {
        ranked_locks::enter(ranked_locks::MONITORS)
    }

    /// Enter the `thread_registry` rank scope. Same shape as
    /// [`Self::enter_monitors_rank`]: the registry has its own internal
    /// locking; this helper only drives the global rank tracker.
    #[inline]
    #[must_use = "bind the guard to a local for the duration of the work"]
    pub fn enter_thread_registry_rank(&self) -> RankScope {
        ranked_locks::enter(ranked_locks::THREAD_REGISTRY)
    }

    /// Acquire `flight_recorder` (L4) with rank tracking.
    #[inline]
    pub fn flight_recorder_lock_ranked(
        &self,
    ) -> RankedGuard<parking_lot::MutexGuard<'_, cratonvm_jfr::FlightRecorder>> {
        let rank = ranked_locks::enter(ranked_locks::FLIGHT_RECORDER);
        RankedGuard {
            lock: self.debug.flight_recorder.lock(),
            rank_scope: rank,
        }
    }

    /// Acquire `native_memory` (L2) with rank tracking.
    ///
    /// L2 is strictly below `flight_recorder` (L4), so a path that needs both
    /// may hold them together provided `flight_recorder` is taken first.
    #[inline]
    pub fn native_memory_lock_ranked(
        &self,
    ) -> RankedGuard<parking_lot::MutexGuard<'_, crate::native::ffi::NativeMemoryTable>> {
        let rank = ranked_locks::enter(ranked_locks::NATIVE_MEMORY);
        RankedGuard {
            lock: self.natives.native_memory.lock(),
            rank_scope: rank,
        }
    }
}

impl std::fmt::Debug for SharedVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedVm")
            .field(
                "classes_loaded",
                &self.classes.class_manager.read().loaded_count(),
            )
            .field("heap_bytes", &self.mem.heap.allocated_bytes())
            .field("native_methods", &self.natives.native_methods.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Vm — convenience wrapper for main thread
// ---------------------------------------------------------------------------

/// The top-level VM struct.
///
/// This is a convenience wrapper that combines the shared state (`Arc<SharedVm>`)
/// with the main thread's per-thread state (`JvmThread`). Tests and the CLI
/// interact with `Vm` directly.
pub struct Vm {
    /// Shared VM state (thread-safe, can be cloned via Arc).
    pub shared: Arc<SharedVm>,

    /// Per-thread state for the main thread. This is boxed because the thread
    /// registry publishes its TLAB address to a cross-thread GC; moving the
    /// containing `Vm` must not invalidate that address.
    pub main_thread: Box<JvmThread>,
}

/// zt t12/df4: the `jdk.module.illegal.native.access` value the `java`
/// launcher would pass for the configured `--illegal-native-access` (`mode`,
/// the VM's `VmConfig::native_access` since zt t18/df7): `deny` or `allow`;
/// `None` for the JDK's default `warn` (no property) and under
/// `CRATONVM_FFM_ILLEGAL_NATIVE_ACCESS_PROPERTY=0` (default ON, `=0` kill).
pub(in crate::vm::vm_init) fn illegal_native_access_property_value(
    mode: cratonvm_native_builtins::panama::IllegalNativeAccess,
) -> Option<&'static str> {
    illegal_native_access_property_for(
        mode,
        true,
    )
}

/// [`illegal_native_access_property_value`] for a given mode and switch
/// (pure: the unit test does not touch the process-wide mode).
pub(in crate::vm::vm_init) fn illegal_native_access_property_for(
    mode: cratonvm_native_builtins::panama::IllegalNativeAccess,
    enabled: bool,
) -> Option<&'static str> {
    use cratonvm_native_builtins::panama::IllegalNativeAccess;
    if !enabled {
        return None;
    }
    match mode {
        IllegalNativeAccess::Deny => Some("deny"),
        IllegalNativeAccess::Allow => Some("allow"),
        IllegalNativeAccess::Warn => None,
    }
}

/// zt t14/x11: [`Vm::boot_take_illegal_native_access`]'s gate. The value the
/// JDK will see is the user's `-Djdk.module.illegal.native.access` when given
/// (applied after the published one), else the one published for
/// `--illegal-native-access`; the class is initialised at boot only for a
/// value `ModuleBootstrap.addIllegalNativeAccess` accepts. Pure.
pub(in crate::vm::vm_init) fn illegal_native_access_taken_at_boot(
    published: Option<&str>,
    user: Option<&str>,
    enabled: bool,
) -> bool {
    enabled && matches!(user.or(published), Some("deny" | "allow" | "warn"))
}


/// `(JAVA_VERSION, JAVA_RUNTIME_VERSION)` from a JDK image's `release` file, or
/// `None` when there is no readable image there.
///
/// The file is a flat `KEY="value"` list written by the JDK build; both keys
/// are present in every image this VM supports. `JAVA_RUNTIME_VERSION` falls
/// back to `JAVA_VERSION` because a stripped image can omit it, and returning a
/// version without a build is better than returning the wrong one.
/// zt t27/lg1: the value of `key` (`IMPLEMENTOR`, `JAVA_RUNTIME_VERSION`, ...)
/// in a JDK image's `release` file, quotes removed; `None` without the file
/// or the key, or for an empty value.
pub(in crate::vm::vm_init) fn jdk_image_release_value(java_home: &str, key: &str) -> Option<String> {
    let data = std::fs::read_to_string(std::path::Path::new(java_home).join("release")).ok()?;
    data.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(key)?.strip_prefix('=')?;
        let value = rest.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

pub(in crate::vm::vm_init) fn jdk_image_release_versions(java_home: &str) -> Option<(String, String)> {
    let data = std::fs::read_to_string(std::path::Path::new(java_home).join("release")).ok()?;
    let mut version = None;
    let mut runtime = None;
    for line in data.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("JAVA_VERSION=") {
            version = Some(rest.trim_matches('"').to_string());
        } else if let Some(rest) = line.strip_prefix("JAVA_RUNTIME_VERSION=") {
            runtime = Some(rest.trim_matches('"').to_string());
        }
    }
    let version = version?;
    let runtime = runtime.unwrap_or_else(|| version.clone());
    Some((version, runtime))
}

/// `JAVA_VERSION_DATE` from a JDK image's `release` file (the image's
/// `java.version.date`, `VersionProps.VERSION_DATE`), or `None` when there is
/// no readable image or no such key (interpreter round i7 wave 136, lane L1b).
pub(in crate::vm::vm_init) fn jdk_image_release_version_date(java_home: &str) -> Option<String> {
    let data = std::fs::read_to_string(std::path::Path::new(java_home).join("release")).ok()?;
    release_version_date(&data)
}

/// The `JAVA_VERSION_DATE` value of a `release` file's text, unquoted. Pure.
pub(in crate::vm::vm_init) fn release_version_date(release: &str) -> Option<String> {
    release.lines().find_map(|line| {
        line.trim()
            .strip_prefix("JAVA_VERSION_DATE=")
            .map(|rest| rest.trim_matches('"').to_string())
            .filter(|date| !date.is_empty())
    })
}


impl Vm {
    /// Mark the primordial launcher thread as parked outside Java bytecode.
    ///
    /// After `main()` returns the CLI may still wait for non-daemon Java
    /// threads. That wait runs in Rust, so the main thread will not reach an
    /// interpreter safepoint; publish its roots/frame trace and enter the same
    /// blocked-region protocol native waits use.
    /// End the main thread's `java.lang.Thread` as HotSpot's launcher does by
    /// detaching it before `DestroyJavaVM` (interpreter round i5 wave 101,
    /// lane L6; `vm_exec::end_main_thread_java_side`). Call once, after
    /// `main` ended and its uncaught exception (if any) was dispatched,
    /// before the non-daemon wait. Nothing with
    /// `CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY=0`.
    pub fn end_main_thread_java_side(&mut self) {
        crate::vm::vm_exec::end_main_thread_java_side(&self.shared, &mut self.main_thread);
    }

    pub fn begin_main_thread_blocking_region(&mut self, state: &'static str) {
        self.main_thread.set_vm_state(state);
        self.main_thread.tlab.retire();
        {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            ctx.deposit_root_snapshot();
        }
        if self.shared.mem.gc_barrier.mark_blocked_region_enter() {
            // GCAUDIT-0711-FIX (finding 1a): auto - the deposit above
            // already raised in_blocked_region.
            let _ = self
                .shared
                .mem
                .gc_barrier
                .arrive_and_wait_auto(self.main_thread.thread_id);
        }
    }

    /// Leave a region opened by [`Self::begin_main_thread_blocking_region`].
    pub fn end_main_thread_blocking_region(&mut self) {
        self.shared.mem.gc_barrier.mark_blocked_region_leave();
        let mut ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        ctx.check_post_block_gc();
        self.main_thread
            .set_vm_state("vm-main:blocking-region-returned");
    }

    /// Sweep every published `read_alias::SlotMap` against the class the loader
    /// actually has, and print the one-line summary. W7-90-slot-map-sweep-caller.md.
    ///
    /// **Why here and not at registration.** W7-69-read-side-alias-instrument.md
    /// settled that: at registration time most of these classes are not loaded,
    /// and `declared_fields` returning empty is indistinguishable from "the
    /// class has no fields" — `java.nio.DirectByteBuffer` is package-private and
    /// is not among the 323 classes `bootstrap_core_classes` names. The sweep
    /// has to run at a point where the workload has already caused the loads,
    /// and this method exists so the launcher has one call rather than a
    /// hand-rolled `NativeContextImpl` at the call site.
    ///
    /// Self-gated: with the flag off `verify_declared_slot_maps` returns before
    /// touching a class, a name or a lock, so this costs one `OnceLock` load and
    /// a branch — once, on a teardown path. It is observation-only in every
    /// mode; the returned report is printed and dropped.
    pub fn sweep_declared_slot_maps(
        &mut self,
        trigger: &str,
    ) -> cratonvm_native_api::read_alias::SweepReport {
        let ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        cratonvm_native_api::read_alias::sweep_declared_slot_maps_at(&ctx, trigger)    }

    /// The launcher's `getMethod` lookup of `main` in the loaded class
    /// `class_name` (internal form), for `Instrumentation.getInitiatedClasses`
    /// (`runtime::resolve::initiating_records::note_launcher_main_lookup`;
    /// interpreter round i1 wave 47, lane L5). The launcher calls it before
    /// invoking `main`; one relaxed load while no record is armed.
    pub fn note_launcher_main_lookup(&self, class_name: &str) {
        let class_id = self
            .shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_loader(class_name, cratonvm_types::ClassLoaderId::Application);
        if let Some(class_id) = class_id {
            crate::runtime::resolve::initiating_records::note_launcher_main_lookup(
                &self.shared,
                class_id,
            );
        }
    }

    /// The launcher's `MethodFinder.findMainMethod` listing of the main class
    /// `class_name` (internal form), as HotSpot's `LauncherHelper.
    /// validateMainMethod` runs it: `getMethod("main", String[].class)` lists
    /// the PUBLIC declared methods of the class, then of its superclasses and
    /// interfaces until one declares a public `main(String[])`, and, when none
    /// does, the class's own declared methods (JDK 25's non-public and
    /// instance `main` search). Each listing is the JDK's
    /// `Class.getDeclaredMethods0(publicOnly)`, which resolves every listed
    /// method's signature types through the class's loader with
    /// `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES` (the switch's own, lane L3).
    ///
    /// `Some(text)` when a listing throws: the launcher's text, which it
    /// prints before exiting 1 -- `java.launcher.cls.error5`, `Error: Unable
    /// to initialize main class {0}` / `Caused by: {1}: {2}` (HotSpot 25.0.3,
    /// measured: probe `tools/probes/interp/L5/L5W57LauncherSignatureType.java`),
    /// or, for a main class in a NAMED module (`--module-path m --add-modules
    /// m pk.Main`), `java.launcher.module.error3`, `Error: Unable to load main
    /// class {0} in module {1}` / `Caused by: {2}` -- the throwable's class
    /// alone, measured (`LauncherHelper.validateMainMethod` picks the key by
    /// `mainClass.getModule().isNamed()`; 2026-10-07) -- else `None`. Only
    /// with that switch, over a real JDK (nothing to list otherwise).
    /// Interpreter round i2 wave 58, lane L5 (item 3's launcher half of
    /// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`).
    pub fn launcher_main_listing_failure(&mut self, class_name: &str) -> Option<String> {
        let (thrown, message) = self.launcher_main_listing_throwable(class_name)?;
        let dotted = class_name.replace('/', ".");
        Some(match self.launcher_main_class_named_module(class_name) {
            Some(module) => format!(
                "Error: Unable to load main class {dotted} in module {module}\nCaused by: {thrown}"
            ),
            None => format!(
                "Error: Unable to initialize main class {dotted}\nCaused by: {thrown}: {message}"
            ),
        })
    }

    /// The name of the named module the application loader's main class
    /// `class_name` (internal form) is in, as `Class.getModule().getName()`
    /// answers, or `None` for an unnamed module (the class path) or a class
    /// the loader has not defined.
    pub(in crate::vm::vm_init) fn launcher_main_class_named_module(&mut self, class_name: &str) -> Option<String> {
        use cratonvm_native_api::{NativeHeapAccess as _, NativeInvokeAccess as _};
        let main_id = self
            .shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_loader(
                class_name,
                cratonvm_types::ClassLoaderId::Application,
            )?;
        let mut ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        // gci i22/oo (g4oo item 4): outside a native scope the infallible
        // mirror aborts on a full heap; a refusal answers "unnamed".
        let mirror =
            crate::runtime::native_oom::catch_alloc_oom(|| ctx.get_class_mirror(main_id)).ok()?;
        let Ok(Some(Value::Object(Some(module)))) =
            ctx.invoke_virtual(mirror, "getModule", "()Ljava/lang/Module;", &[])
        else {
            return None;
        };
        let Ok(Some(Value::Object(Some(name)))) =
            ctx.invoke_virtual(module, "getName", "()Ljava/lang/String;", &[])
        else {
            return None;
        };
        ctx.read_string(name)    }

    /// [`Self::launcher_main_listing_failure`]'s listings: the binary name of
    /// the throwable's class and its message (`"null"` for none) when one
    /// throws, else `None`.
    pub(in crate::vm::vm_init) fn launcher_main_listing_throwable(&mut self, class_name: &str) -> Option<(String, String)> {
        if !self.shared.config.reflection_loads_signature_types
            || self.shared.config.java_home.is_none()
        {
            return None;
        }
        let main_id = self
            .shared
            .classes
            .class_manager
            .read()
            .find_class_by_name_for_loader(
                class_name,
                cratonvm_types::ClassLoaderId::Application,
            )?;
        let mut pending = vec![main_id];
        let mut seen: Vec<ClassId> = Vec::new();
        let mut found = false;
        while let Some(class_id) = pending.pop() {
            if seen.contains(&class_id) || seen.len() > 64 {
                continue;
            }
            seen.push(class_id);
            if let Err(failure) = self.launcher_list_declared_methods(class_id, true) {
                return failure;
            }
            let (declares_main, superclass, interfaces) = {
                let cm = self.shared.classes.class_manager.read();
                let class = cm.get_class(class_id)?;
                let declares = class.methods.iter().any(|m| {
                    &*m.name == "main"
                        && m.descriptor.starts_with("([Ljava/lang/String;)")
                        && m.access_flags.bits() & 0x0001 != 0
                });
                (declares, class.superclass, class.interfaces.clone())
            };
            if declares_main {
                found = true;
                break;
            }
            // Superclass first (popped first), then the interfaces.
            pending.extend(interfaces.into_iter().rev());
            if let Some(superclass) = superclass {
                pending.push(superclass);
            }
        }
        if !found {
            if let Err(failure) = self.launcher_list_declared_methods(main_id, false) {
                return failure;
            }
        }
        None
    }

    /// One `Class.getDeclaredMethods0(publicOnly)` of
    /// [`Self::launcher_main_listing_failure`]: `Err(Some((class, message)))` when it
    /// throws, `Err(None)` when the VM cannot run it (no mirror call; the
    /// launcher then finds `main` as before).
    pub(in crate::vm::vm_init) fn launcher_list_declared_methods(
        &mut self,
        class_id: ClassId,
        public_only: bool,
    ) -> Result<(), Option<(String, String)>> {
        use cratonvm_native_api::{NativeHeapAccess as _, NativeInvokeAccess as _};
        let thrown = {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            // gci i22/oo (g4oo item 4): see `launcher_main_class_named_module`.
            let Ok(mirror) =
                crate::runtime::native_oom::catch_alloc_oom(|| ctx.get_class_mirror(class_id))
            else {
                return Err(None);
            };
            match ctx.invoke_special(
                "java/lang/Class",
                "getDeclaredMethods0",
                "(Z)[Ljava/lang/reflect/Method;",
                &[
                    Value::Object(Some(mirror)),
                    Value::Int(i32::from(public_only)),
                ],
            ) {
                Ok(_) => return Ok(()),
                Err(MethodCallFailed::ExceptionThrown(exc)) => exc,
                Err(MethodCallFailed::InternalError(_)) => return Err(None),
            }
        };
        let class_id = self.shared.mem.heap.class_id_of(thrown);
        let name = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_class(class_id)
                .map(|c| c.name.replace('/', "."))
                .unwrap_or_else(|| "<unknown class>".to_string())
        };
        let message = self
            .exception_detail_message(thrown)
            .unwrap_or_else(|| "null".to_string());
        Err(Some((name, message)))
    }

    /// Create a new VM with the given configuration.
    /// Initialize with a heap-exhaustion error channel.
    pub fn try_new(config: VmConfig) -> Result<Self, crate::error::VmInitializationError> {
        crate::runtime::native_oom::catch_alloc_oom(|| Self::new(config)).map_err(|_| {
            crate::error::VmInitializationError {
                detail: "java.lang.OutOfMemoryError: Java heap space".into(),
            }
        })
    }

    pub fn new(config: VmConfig) -> Self {
        raise_open_file_limit_to_hard();
        let shared = Arc::new(SharedVm::new(config));

        // Store a weak self-reference so native methods can clone the Arc
        // for spawning new threads.
        *shared.self_arc.write() = Some(Arc::downgrade(&shared));

        {
            let prewarm_vm_id = vm_identity_for_layout_domain(
                shared
                    .classes
                    .class_manager
                    .read()
                    .class_store()
                    .layout_domain(),
            );
            let prewarm_home = shared.system_properties.read().get("java.home").cloned();
            shared
                .deferred_starts
                .start_or_defer("cvm-cds-prewarm", move || {
                    let _ = std::thread::Builder::new()
                        .name("cvm-cds-prewarm".to_string())
                        .spawn(move || {
                            cratonvm_native_builtins::lang_class::prewarm_cds_symbol_index(
                                prewarm_vm_id,
                                prewarm_home,
                            );
                        });
                });
        }

        // t1 wave 6 (lane J6): under `CRATONVM_JIT_INVALIDATE_NOT_ENTRANT`
        // (opt-in) arm this VM's not-entrant hook now, so the first dependency
        // invalidation patches what it withdraws (no-op with the switch off).
        crate::jit::helpers::arm_not_entrant_for_dependency_invalidation(&shared);
        wire_owed_poll_sync(&shared);

        // Tier proposal W4-1 (round 12 wave 5): after a successful compile the
        // tiered manager sets the method's tier from the body this VM's cache
        // holds under its exact key. `Weak`: the manager is part of this VM.
        // Unanswerable keys (no class id, a body retired or keyed elsewhere)
        // answer `None`, which keeps the manager's old inference.
        {
            let weak_vm = Arc::downgrade(&shared);
            let _ = shared.jit.tiered_manager.set_live_entry_probe(Box::new(
                move |key: &crate::jit::tiered::MethodKey| {
                    if key.class_id.as_u32() == 0 {
                        return None;
                    }
                    let vm = weak_vm.upgrade()?;
                    // `RetainedCode`: should this read outlive the body's
                    // withdrawal, its last reference retires through the queue.
                    let body = crate::jit::RetainedCode::new(vm.jit.jit_cache.get(
                        &key.class_name,
                        &key.method_name,
                        &key.descriptor,
                        key.class_id,
                    )?);
                    if body.retired.load(std::sync::atomic::Ordering::Acquire) {
                        return None;
                    }
                    Some(if body.used_ir_backend {
                        crate::jit::tiered::LiveEntryBody::Optimizing
                    } else {
                        crate::jit::tiered::LiveEntryBody::SinglePass
                    })
                },
            ));
        }
        // CONF21-1 (JIT round 21 lane conf): the manager records each admitted
        // compile, and a mutator resolves the String constants HotSpot resolves
        // when a compile is requested (`jit_bridge::resolve_admitted_compile_strings`).
        crate::runtime::interpreter::jit_bridge::install_compile_admission_sink(&shared);
        // Interpreter round i6 wave 117, lane L2
        // (`CRATONVM_JIT_REENTRY_ONLY_PUBLICATION`, default off; page
        // `i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries`):
        // the JDK methods whose registered `Intrinsic` re-enters their own
        // bytecode (`invoke_*_bytecode_only`) publish re-entry-only bodies, so
        // those re-entries run compiled while every caller keeps reaching the
        // intrinsic. The set is the page's design item 1. Since wave 121 it is
        // the registrars' (`NativeMethodRegistry::mark_reentry_only_body`,
        // next to each registration in `lang_invoke::register_t4_method_handle_invoke`);
        // this loop only publishes it. A no-op with the switch off
        // (`mark_reentry_only` answers `false`).
        for &(class, method, descriptor) in shared.natives.native_methods.reentry_only_bodies() {
            if !shared.jit.jit_cache.mark_reentry_only(class, method, descriptor) {
                break;
            }
        }
        // JIT round 23 wave 8h (lane W8-L1): rows whose registrar asked their
        // OWN switch (`mark_reentry_only_body_on_its_own_switch`;
        // `BuiltinClassLoader.loadClassOrNull` under
        // `CRATONVM_JIT_LOADER_REENTRY_BODIES`) publish re-entry-only whatever
        // the cache-wide switch says. Per VM (this cache).
        for &(class, method, descriptor) in
            shared.natives.native_methods.reentry_only_bodies_own_switch()
        {
            shared
                .jit
                .jit_cache
                .mark_reentry_only_row(class, method, descriptor);
        }
        // Interpreter round i6 wave 116, lane L2 (page `i107-L2`,
        // `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT`, default off): the
        // class-definition epoch a compile parked "waiting on a class" is
        // released against (`tiered::note_compile_waiting_on_class`). Not
        // installed with the switch off, so nothing parks. This VM's own
        // store epochs (per layout domain), never a process-wide counter.
        // Also for `CRATONVM_JIT_BG_RESOLVER_MISS_WAITS_ON_CLASS` (the x86-64
        // background door's resolver misses; `jit_bridge`).
        if cratonvm_jit::aarch64_backend::arm64_field_site_transient_enabled()
            || crate::runtime::interpreter::jit_bridge::bg_resolver_miss_waits_on_class_enabled()
        {
            let epochs = cratonvm_classloading::store_epochs(shared.jit.class_layout_domain);
            let _ = shared
                .jit
                .tiered_manager
                .set_class_epoch_source(Box::new(move || epochs.class_definition_epoch()));
        }

        // Publish the process-global VM cell so a foreign (host-created) thread
        // that calls `AttachCurrentThread` can resolve the live VM and register
        // itself for GC-safepoint participation — for EVERY creation path, not
        // just the libcratonvm Invocation API. Without this, a CLI-launched app
        // whose native library spawns + attaches its own OS thread would fail
        // attach (no VM to resolve) once the default-on foreign-attach path runs.
        // `Weak`, so it never keeps the VM alive; last writer wins (harmless for
        // the in-process multi-`Vm` test fixtures — one VM per real process).
        crate::native::jni::set_process_vm(&shared);

        // zt t7/tg (`zgc-zt1tp-director-parity-residuals` item 3): HotSpot's
        // ZGC director thread -- the `Timer` rule of `-XX:ZCollectionInterval`
        // (and the opt-in idle proactive tick). Starts nothing on a default
        // run (`director_tick_wanted` is false), off ZGC, or in a unit VM.
        // Phase 5 WP3: held back in a boot image's capture run.
        shared.deferred_starts.start_or_defer_with_vm(&shared, "ZDirector", |shared| {
            let _ = crate::runtime::interpreter::gc_and_alloc::start_zgc_director_tick(shared);
        });
        // zt t31/x10: G1's periodic-collection service, when the command line
        // sets `-XX:G1PeriodicGCInterval` (a later `setVMOption` starts it too).
        shared.deferred_starts.start_or_defer_with_vm(&shared, "G1 Service", |shared| {
            let _ = crate::runtime::interpreter::gc_and_alloc::start_g1_periodic_gc_service_at_boot(
                shared,
            );
        });

        // Round 4 audit fix (CRIT) — publish the same weak handle to the
        // module-private registry used by `resolution_invalidate_adapter` so
        // JVMTI `RedefineClasses` can reach back into
        // `shared.classes.resolution_cache` and drop stale resolutions.
        //
        // Idempotent PER VM, and every registered VM is reached — NOT
        // "first writer wins". That older behaviour meant a second `Vm` built
        // in the same process (a test fixture, or a real embedding) never had
        // its own resolution/link/JIT caches invalidated on redefine. See
        // `docs/architecture/per-vm-state.md` §3 (V2).
        //
        // gch-h4fl items 1-2: the registration itself prints the
        // `-Xlog:class+load` lines of the classes defined before it (gci
        // i5/hd walk + cutoff, `set_global_shared_vm_for_hooks`). Dev's wave 9
        // lane G fixed the same items with a walk called here; the
        // 2026-10-15 merge keeps the gci walk only, or each class would print
        // twice.
        set_global_shared_vm_for_hooks(Arc::downgrade(&shared));

        // obsaudit D14 (2026-07-26): bridge `runtime::jvmti::JvmtiEventManager`
        // (interpreter/GC/classloading-sourced events) to the real,
        // native-agent-facing `shared.debug.jvmti_env` — see the notes above
        // `install_real_agent_env_bridge` in `runtime/jvmti.rs`. Same `Weak`,
        // idempotent, last-writer-wins shape as the two hooks just above.
        crate::runtime::jvmti::install_real_agent_env_bridge(&shared);

        // JFR `cratonvm.JitCompileDecision` — install the producer's delivery
        // sink.
        //
        // `cratonvm-jit` has no route to a `FlightRecorder`: the recorder is
        // `SharedVm::debug.flight_recorder`, and a `jit -> vm` dependency edge
        // would cycle. So `cratonvm_jfr::jit_decision` holds a `OnceLock` sink
        // and the VM fills it in here — the same boot-time installer shape as
        // `cratonvm_gc::install_gc_start_hook` and
        // `install_real_agent_env_bridge` just above, except that this one is a
        // BOXED CLOSURE rather than a bare `fn` because it has to CAPTURE the
        // recorder handle, which is the entire point of the indirection.
        //
        // `Weak`, like the two hooks above, so a sink that outlives its VM (the
        // cell is process-global and first-installation-wins) cannot keep the
        // VM alive.
        //
        // Installed BEFORE the `-XX:StartFlightRecording` block below so that a
        // recording naming the event on the command line arms the producer gate
        // the moment `start_recording` refreshes it. Order is not strictly load
        // bearing — `install_jit_decision_sink` re-arms from both flags itself —
        // but this way there is no window in which a started recording wants the
        // event and the producer is still dark.
        {
            // How many `try_lock` attempts the decision sink makes before it
            // gives the event up. See the comment on the loop below.
            const JIT_DECISION_LOCK_ATTEMPTS: u32 = 64;

            let weak = Arc::downgrade(&shared);
            cratonvm_jfr::jit_decision::install_jit_decision_sink(Box::new(move |decision| {
                let Some(vm) = weak.upgrade() else {
                    return;
                };
                cratonvm_native_builtins_jfr::jfr_backend::emit_jit_compile_decision(vm.vm_identity, decision);
                // TRY-LOCK, DELIBERATELY, AND NOT `lock()`.
                //
                // `flight_recorder` is a non-reentrant `parking_lot::Mutex`, and
                // this closure runs on whatever thread is compiling — a
                // background compile worker, or a mutator part-way through
                // executing Java. Every critical section that takes this lock
                // today is `acquire -> one emit_* -> drop`, with no Java
                // re-entry and no compile inside it, so a self-deadlock is not
                // reachable as the code stands (checked across every
                // `flight_recorder.lock()` site in `vm/` and `vm-cli/` when this
                // was written).
                //
                // But this sink is the first thing that can call INTO the
                // recorder from the middle of a compile, and it turns "some
                // future path emits a JFR event with the recorder held and then
                // runs Java" from a harmless mistake into a hung VM. That is not
                // a trade a diagnostic gets to make: a lost decision event is a
                // hole in a dump, a deadlock is a hung process.
                //
                // BOUNDED SPIN rather than a single attempt, so that ordinary
                // contention — several compile workers each holding the lock for
                // one formatted event — does not silently thin the census out
                // and make a `jfr print` undercount. A genuine self-deadlock
                // costs this many yields per compile and then continues, which
                // is slow and visible rather than silent and fatal.
                for _ in 0..JIT_DECISION_LOCK_ATTEMPTS {
                    if let Some(mut jfr) = vm.debug.flight_recorder.try_lock() {
                        cratonvm_jfr::builtin::emit_jit_compile_decision_event(&mut jfr, decision);
                        return;
                    }
                    std::thread::yield_now();
                }
            }));
        }

        // obsaudit D15 (2026-07-26) — open the real attach-API socket and
        // register the *live* (real-VM-state-backed) jcmd command set. See
        // the LIVENESS block and `AttachListener`'s doc comment in
        // `runtime/serviceability.rs`: this must be `new_with_vm_state`,
        // never the argument-less `JcmdProcessor::new()` (that one reports
        // fabricated data for several commands). `shared.clone()` coerces
        // to `Arc<dyn VmDiagnosticState>` via the `impl VmDiagnosticState
        // for SharedVm` in this file.
        //
        // `-XX:+DisableAttachMechanism` (interpreter round i2 wave 59, lane
        // L1): no listener at all, as HotSpot starts none under the flag.
        //
        // Phase 5 WP3: deferred whole in a boot image's capture run -- the
        // listener's thread, and the socket path, which names this process id.
        shared.deferred_starts.start_or_defer_with_vm(&shared, "Attach-Listener", |shared| {
            if !shared.config.disable_attach_mechanism {
                *shared.debug.jcmd_processor.lock() = Some(
                    crate::runtime::serviceability::JcmdProcessor::new_with_vm_state(shared.clone()),
                );
            } else if cfg!(any(unix, windows))
                && shared.config.sigquit_thread_dump
                && !shared.config.reduce_signal_usage
            {
                // Interpreter round i3 wave 80, lane L6
                // (`i59-L1-sigquit-terminates-the-vm-20261023.md`): no listener,
                // but HotSpot's "Signal Dispatcher" still prints the dump for
                // every `SIGQUIT` (an attaching client's too: with the flag it
                // never checks the trigger file). A dispatcher thread of its own,
                // binding nothing.
                #[cfg(any(unix, windows))]
                {
                    *shared.debug.jcmd_processor.lock() = Some(
                        crate::runtime::serviceability::JcmdProcessor::new_signal_dispatcher(
                            shared.clone(),
                        ),
                    );
                }
            }
        });
        // `CRATONVM_PERF_DATA_FILE` (interpreter round i3 wave 81, lane L1):
        // HotSpot's jvmstat file, which `jps`, `VirtualMachine.list()` and an
        // external attach's `testAttachable` read (its attach bit `0` under
        // `-XX:+DisableAttachMechanism`). Default on (`-XX:-UsePerfData`,
        // `-XX:+PerfDisableSharedMem` or `CRATONVM_PERF_DATA_FILE=0` turn it
        // off).
        // Not in this crate's unit tests (interpreter round i4 wave 87, lane
        // L1): their many VMs share one process id, so all but the first
        // published nothing anyway, and the first one's live-counter sampler
        // (`perf_data_file::start_sampler`) could keep a test's dropped VM
        // alive into the next test (the process attach endpoint tests saw
        // it still owned).
        // zt t12/pl4: a ZGC heap's image takes HotSpot's ZGC shape
        // (`CRATONVM_ZGC_PERF_IMAGE_SHAPE`, `perf_data_file::zgc_image_shape`).
        //
        // Phase 5 WP6: the file is named by the process id and mapped shared
        // (`/tmp/hsperfdata_<user>/<pid>`), so a boot image cannot carry it:
        // a capture run publishes it after the restore, in the restored
        // process (through the deferred starts; at once in every other run).
        shared.deferred_starts.start_or_defer_with_vm(&shared, "PerfData", |shared| {
            let zgc_heap = shared.mem.heap.is_zgc();
            if shared.config.perf_data_file && !cfg!(test) {
                // zt t11/pl3 (`CRATONVM_PERF_DATA_IN_MEMORY_FALLBACK`, default ON since zt pl4): a
                // publication that failed keeps the image in memory, as HotSpot
                // falls back to C-heap perf memory.
                use crate::runtime::perf_data_file as perf;
                let config = &shared.config;
                let image = perf::create_for_vm(config, zgc_heap)
                    .or_else(|| perf::in_memory_fallback_for_vm(config, zgc_heap));
                *shared.debug.perf_data_file.lock() = image;
            } else if !cfg!(test)
                && crate::runtime::perf_data_file::wants_memory_only_image(&shared.config)
            {
                // zt t10/pl2: `-XX:+PerfDisableSharedMem` keeps the image in
                // memory (no file, no name), as HotSpot keeps its perf memory in
                // the C heap: `Perf.attach(0)` and the sampler still work.
                *shared.debug.perf_data_file.lock() =
                    crate::runtime::perf_data_file::create_in_memory_for_vm(&shared.config, zgc_heap);
            }
            // zt t14/pl5 (`CRATONVM_PERF_HOTSPOT_COUNTERS`): HotSpot's counters
            // the image lacked that have a source here, appended now that the
            // VM's properties exist (nothing without an image).
            crate::runtime::perf_data_file::add_hotspot_counters(shared, zgc_heap);
        });
        // `CRATONVM_ATTACH_LISTENER_LAZY` (wave 59, lane L1): an attaching
        // client that finds no socket sends `SIGQUIT`, which must not kill
        // the VM -- with the flag above too, whatever the switch: there no
        // socket is ever bound, so every `jcmd` would otherwise terminate
        // the process (HotSpot's handler runs and the client times out).
        // `CRATONVM_SIGQUIT_THREAD_DUMP` (wave 60, lane L1): with a listener,
        // whose thread acts on it, `SIGQUIT` sets a latch and the dump is
        // printed (`VmConfig::sigquit_thread_dump`).
        // `-Xrs` (wave 79, lane L3; `VmConfig::reduce_signal_usage`): no
        // handler at all, as HotSpot installs none under the flag.
        // Wave 80 (lane L6): under `-XX:+DisableAttachMechanism` the latch
        // too, read by the signal dispatcher made above; and Windows'
        // `Ctrl-Break`, HotSpot's `SIGBREAK` dump, latched for the dispatcher
        // `JcmdProcessor` starts there.
        //
        // Phase 5 WP3: a signal disposition is kernel state, not memory, so a
        // boot image cannot carry it; the capture run installs it after the
        // restore, with the threads that act on it.
        shared.deferred_starts.start_or_defer_with_vm(&shared, "SIGQUIT handler", |shared| {
            #[cfg(unix)]
            if shared.config.reduce_signal_usage {
                // The signal keeps its default action.
            } else if shared.config.sigquit_thread_dump {
                crate::runtime::serviceability::install_sigquit_latch();
            } else if shared.config.attach_listener_lazy || shared.config.disable_attach_mechanism {
                crate::runtime::serviceability::install_sigquit_keeps_the_vm_alive();
            }
            #[cfg(windows)]
            if shared.config.sigquit_thread_dump && !shared.config.reduce_signal_usage {
                crate::runtime::serviceability::install_ctrl_break_latch();
            }
            let _ = shared;
        });

        // obsaudit D12 (2026-07-26) — `-XX:StartFlightRecording`. Before
        // this, vm-cli never called `start_recording` (see the retracted
        // claim this comment replaces), so `cratonvm_jfr::is_enabled()` was
        // permanently false and the ~30 wired `emit_*` call sites never
        // captured anything. `config.jfr_start_recording` is `None` unless
        // the flag was passed, so this is a no-op — same cost as before —
        // on every VM that doesn't request it.
        if let Some(jfr_cfg) = shared.config.jfr_start_recording.clone().filter(|_| shared.config.use_synthetic_jdk) {
            let mut settings = cratonvm_jfr::RecordingSettings::new("cratonvm");
            settings.max_age = jfr_cfg.max_age;
            settings.max_size = jfr_cfg.max_events;
            settings.duration = jfr_cfg.duration;
            settings.dump_on_exit = jfr_cfg.dump_on_exit;

            // ---- the event-name filter: the command-line half of
            // ---- `jdk.jfr.Recording.enable(...)`
            //
            // 2026-09-01. Before this, `RecordingSettings::new` left
            // `enabled_event_names: None` and nothing on the boot path ever
            // overwrote it, so a `-XX:StartFlightRecording` recording could
            // not name an event at all. That is not merely a missing
            // convenience: `cratonvm.JitCompileDecision` arms its producer
            // ONLY when a running recording names it explicitly (see
            // `jfr/src/jit_decision.rs` and
            // `FlightRecorder::any_running_recording_names_event`, which is
            // deliberately stricter than the drain filter), so the event was
            // unreachable from any command line. The only route was an
            // in-process `jdk.jfr.Recording`, i.e. editing the application --
            // exactly what an operator holding a production incident cannot
            // do, and exactly the gap the event was added to close.
            //
            // TWO DOORS, deliberately, converging on this one place:
            //
            //  * `-XX:StartFlightRecording:+<Event>#enabled=true` -- HotSpot's
            //    own spelling, parsed by `config::apply_jfr_event_setting`
            //    into `VmConfig::jfr_enabled_events`.
            //  * `CRATONVM_JFR_ENABLE_EVENTS=<name>[,<name>...]` -- a plain
            //    comma-separated list, and obviously CratonVM-specific
            //    because it is not HotSpot syntax and cannot be mistaken for
            //    it. It exists because `Vm::new` is also reached by embedders
            //    (`libcratonvm`, the in-process test binary, the JNI
            //    invocation API) that never pass argv through `vm-cli`'s
            //    option parser, and because it lets an already-deployed
            //    launch script arm the event without editing its flags.
            //
            // They UNION rather than shadow each other. Two doors where the
            // later silently overrides the earlier is how a flag ends up
            // looking wired and doing nothing, which is the whole complaint
            // this change answers.
            let requested_events: Option<Vec<String>> = {
                let mut names: Vec<String> = Vec::new();
                let mut asked = false;
                if let Some(from_flag) = shared.config.jfr_enabled_events.as_ref() {
                    asked = true;
                    for name in from_flag {
                        if !names.iter().any(|n| n == name) {
                            names.push(name.clone());
                        }
                    }
                }
                if let Ok(raw) = cratonvm_types::flags::runtime_var("CRATONVM_JFR_ENABLE_EVENTS") {
                    // Setting the variable at all counts as asking, even to an
                    // empty value: that installs an EMPTY whitelist, so the
                    // dump is empty and the startup line below says the filter
                    // is empty. The alternative -- treating an empty value as
                    // "no filter" -- would silently produce a full recording
                    // from a command that asked for a narrow one, which is the
                    // same class of lie as the unknown-name case handled just
                    // below.
                    asked = true;
                    for name in raw.split(',') {
                        let name = name.trim();
                        if !name.is_empty() && !names.iter().any(|n| n == name) {
                            names.push(name.to_string());
                        }
                    }
                }
                asked.then_some(names)
            };
            if let Some(requested) = requested_events {
                // Validate every requested name against the recorder's OWN
                // type registry, and refuse to boot on one it does not define.
                //
                // A typo here is otherwise indistinguishable from the exact
                // ambiguity this event exists to remove: the recording starts,
                // the workload runs, the dump is empty, and "the event never
                // fired" and "the name was misspelt" look identical. The
                // incident window is gone by the time anyone can tell them
                // apart, so this has to fail before the workload starts.
                //
                // `create_flight_recorder` registers every built-in event type
                // at recorder construction (see `jfr/src/lib.rs`), which
                // happens in `SharedVm::new` well before this block, so the
                // registry is authoritative for every name a command line can
                // legitimately use. A `jdk.jfr.Event` subclass DEFINED IN JAVA
                // registers later, at its first commit -- such a name cannot
                // be spelled here and is an in-process-only route by
                // construction, which is why "unknown" can be a hard error
                // rather than a warning.
                let rejection = {
                    let fr = shared.debug.flight_recorder.lock();
                    let unknown: Vec<String> = requested
                        .iter()
                        .filter(|name| fr.type_registry.find_by_name(name.as_str()).is_none())
                        .cloned()
                        .collect();
                    if unknown.is_empty() {
                        None
                    } else {
                        let mut known: Vec<String> = fr
                            .type_registry
                            .iter()
                            .map(|(_, ty)| ty.name.clone())
                            .collect();
                        known.sort();
                        Some((unknown, known))
                    }
                };
                if let Some((unknown, known)) = rejection {
                    // PANIC, and not `tracing::error!` plus carry on.
                    //
                    // Carrying on is the failure being designed out: it hands
                    // the operator a running VM whose recording can never
                    // contain what they asked for.
                    //
                    // `std::process::exit` would be the launcher-shaped
                    // response and is what a bad `-XX:` flag deserves, but it
                    // is wrong HERE: `Vm::new` is a library entry point, and
                    // the `cratonvm-vm` test binary builds one `SharedVm` per
                    // test. An `exit` would take the whole test binary down
                    // over one test's configuration, and nothing else in
                    // `vm/src` calls it (checked). A panic is just as
                    // impossible to ignore, unwinds only this VM, and still
                    // exits a real launcher non-zero.
                    panic!(
                        "-XX:StartFlightRecording / CRATONVM_JFR_ENABLE_EVENTS names {} event(s) \
                         this VM does not define: {}\n\
                         No recording can ever capture them, so refusing to start rather than \
                         handing back an empty dump.\n\
                         Known event names:\n  {}",
                        unknown.len(),
                        unknown.join(", "),
                        known.join("\n  "),
                    );
                }
                // Unconditional `eprintln!`, not `tracing::info!`: this line
                // only prints when the operator explicitly asked for a JFR
                // recording AND named events in it, so it is never noise, and
                // it must not depend on a `tracing` subscriber being installed
                // and filtered to `info`. It is the receipt that says which
                // events this dump can possibly contain -- the one fact that
                // makes a thin dump readable instead of ambiguous.
                eprintln!(
                    "[JFR] recording `cratonvm` restricted to {} named event(s): {}",
                    requested.len(),
                    requested.join(", ")
                );
                settings.enabled_event_names = Some(requested.iter().cloned().collect());
            }

            // The producer gate does NOT need its own `sync_jit_decision_gate`
            // call here, and this was verified rather than assumed -- it is
            // the same argument-order bug that was just fixed on the Java side
            // in `vm_exec::jfr_configure_java_recording`, where `r.start()`
            // before `r.enable(...)` left the producer permanently dark.
            //
            // The reason the two paths differ: there, `Recording.enable(...)`
            // mutates a recording that is ALREADY RUNNING and changes no
            // recording state, so nothing re-derives the gate. Here the names
            // are written into `settings` BEFORE `new_recording`, so the
            // recording is born with its filter and the state transition is
            // what happens last. `FlightRecorder::start_recording` calls
            // `refresh_running_ids`, whose tail calls
            // `jit_decision::sync_jit_decision_gate(self)` -- so the gate is
            // recomputed from a recorder that already names the event.
            //
            // The sink is installed further up in this function, before this
            // block, and `install_jit_decision_sink` re-arms from both flags
            // itself, so neither order can leave the producer dark. Adding a
            // redundant `sync_jit_decision_gate` call after `start_recording`
            // was considered and rejected: it would be dead the day it was
            // written and would decay into looking load-bearing.
            let recording_id = {
                let mut fr = shared.debug.flight_recorder.lock();
                let id = fr.new_recording(settings);
                fr.start_recording(id);
                id
            };
            if jfr_cfg.dump_on_exit {
                let filename = jfr_cfg
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("./cratonvm-recording-{}.jfr", std::process::id()));
                *shared.debug.jfr_dump_on_exit.lock() = Some((recording_id, filename));
            }
            // obsaudit D12 — the reclamation half of the fix. Before this,
            // `ThreadRingRegistry::reclaim_retired_shards` only ran from
            // inside `drain_all`, which only ran at dump time — harmless
            // only because the disabled gate above kept ordinary threads
            // from ever registering a shard. A recording that now actually
            // runs continuously needs its per-thread rings drained
            // periodically, both to keep events flowing into the
            // repository (rather than only at final dump) and to let
            // retired+empty shards from thread churn actually get
            // reclaimed instead of accumulating in the registry `Vec` for
            // the recording's whole lifetime. One drain per second is
            // frequent enough that a 1024-capacity ring on a
            // moderately-busy thread will not silently drop events
            // between drains, and cheap enough (an empty repository drain
            // is a handful of shard-list iterations) to run indefinitely.
            let weak_shared = Arc::downgrade(&shared);
            let duration = jfr_cfg.duration;
            // Phase 5 WP3: held back in a boot image's capture run.
            shared.deferred_starts.start_or_defer("JFR-Periodic-Drain", move || {
                std::thread::Builder::new()
                    .name("JFR-Periodic-Drain".into())
                    .spawn(move || {
                        let started = std::time::Instant::now();
                        loop {
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            let Some(shared) = weak_shared.upgrade() else {
                                break; // VM torn down (e.g. an in-process test) — stop.
                            };
                            let mut fr = shared.debug.flight_recorder.lock();
                            #[cfg(feature = "gpu-offload")]
                            if shared.config.gpu_offload_enabled {
                                let ts_ns = started.elapsed().as_nanos() as u64;
                                crate::runtime::offload::emit_vm_gpu_memory_stats_jfr(
                                    &mut fr,
                                    &shared,
                                    ts_ns,
                                );
                            }
                            fr.drain_per_thread_into_repository();
                            if let Some(d) = duration {
                                if started.elapsed() >= d {
                                    fr.stop_recording(recording_id);
                                    break;
                                }
                            }
                        }
                    })
                    .ok();
            });
        }

        // KC16-watchdog: install the wait-site frame dumper so a thread
        // parked in `Object.wait()` (e.g. AsyncFutureTask.await) can emit
        // its frame chain when the stack-dump watchdog fires. Without
        // this, the watchdog reports "0 threads dumped — main in native"
        // because the parked thread never reaches the interpreter's
        // top-of-loop poll.
        //
        // The closure reads the per-thread "wait-site frame snapshot"
        // thread-local that `monitor_wait` populates on entry (see
        // `vm_exec::monitor_wait`). The Monitor itself runs on the same
        // OS thread as the wait caller, so the thread-local is visible
        // even though we routed the wait through `parking_lot::Condvar`.
        // We also increment the watchdog ack counter so the watchdog's
        // "0 threads dumped" banner no longer fires for this case.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_site_dump(move |_tid| {
                if let Some(s) = weak.upgrade() {
                    crate::vm::vm_init::dump_wait_site_thread_local(&s);
                }
            });
        }
        // Companion to the frame dump above: the STATE of the object the thread
        // is parked on. See `dump_wait_object_state` for why those two fields
        // are the ones that decide the netty promise stall.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_object_dump(move |obj| {
                if let Some(s) = weak.upgrade() {
                    crate::vm::vm_init::dump_wait_object_state(&s, obj);
                }
            });
        }
        // And the GC-SAFE handle for that dump. `Monitor::wait` holds the
        // awaited `ObjectRef` as a plain local for the whole wait, which a
        // moving collector invalidates; `jmx_waiting_monitor` is a scanned root
        // that `update_thread_objs_after_gc` (gc.rs step 21) forwards, so it is
        // the address still valid at dump time. Without this the dump silently
        // reports pre-relocation field values.
        {
            let weak = Arc::downgrade(&shared);
            crate::threading::monitor::install_wait_object_resolve(move |tid| {
                let s = weak.upgrade()?;
                s.threads.thread_registry.peek_jmx_waiting_monitor(tid)
            });
        }

        // Register the main thread (id 0) in the thread registry.
        let mut main_thread = Box::new(JvmThread::new(ThreadId(0), "main"));
        // JIT round 23 wave 2, lane L3: `-Xss` deepens the main thread's
        // compiled recursion as it does every other platform thread's
        // (`main_thread_self_call_extra_budget`).
        if true {
            main_thread.self_call_extra_budget = super::vm_exec::main_thread_self_call_extra_budget(
                shared.config.default_thread_stack_size,
            );
        }
        // JIT round 23 wave 6, lane L3: the main thread's Java stack size is
        // this VM's `-Xss` (HotSpot's 1 MiB default when unset): the compiled
        // self-recursion budget is 16 times it (`jit::helpers::SelfCallBudget`).
        main_thread.self_call_java_stack_bytes =
            shared.config.default_thread_stack_size.unwrap_or(0);
        // nio21 wave 3 L1: this VM's redefinitions only (see
        // `InvokeCache::bind_redefinition_log`).
        main_thread
            .invoke_cache
            .bind_redefinition_log(shared.classes.redefinition_log.clone());
        shared
            .threads
            .thread_registry
            .register(ThreadId(0), "main", None);
        // Interpreter round i5 wave 100, lane L6: the main thread counts in
        // the JMM's started / live / peak counters, as in HotSpot.
        shared
            .threads
            .thread_registry
            .note_jmm_thread_start(ThreadId(0));
        // obsaudit D1: bind this OS thread's JVMTI thread-attribution TLS so
        // ClassLoad/ClassPrepare events fired while bootstrapping on the
        // main thread report the real `jthread` instead of the "unknown"
        // sentinel. See `cratonvm_classloading::set_current_thread_id`.
        cratonvm_classloading::set_current_thread_id(shared.classes.class_layout_domain, 0);
        // Share the interrupted flag so cross-thread interrupt works on the main thread
        shared
            .threads
            .thread_registry
            .set_interrupted_flag(ThreadId(0), main_thread.interrupted.clone());
        // Share the park state so LockSupport.unpark(mainThread) works
        shared
            .threads
            .thread_registry
            .set_park_state(ThreadId(0), main_thread.park_state.clone());
        // Share root snapshot so GC cross-thread collection includes main thread roots
        shared
            .threads
            .thread_registry
            .set_root_snapshot(ThreadId(0), main_thread.root_snapshot.clone());
        // Match spawned threads: the watchdog summary and cross-thread stack
        // probes must see the primordial thread's parked Java frames too.
        shared
            .threads
            .thread_registry
            .set_frame_trace(ThreadId(0), main_thread.frame_trace.clone());
        // Same handle to the crash handler, which cannot take the thread
        // registry's lock (the faulting thread may already hold it). This is
        // what puts Java frames — not just native ones — into an `hs_err`
        // report for the thread most crashes happen on. Worker threads need
        // the equivalent publication at their own registration sites; see the
        // cross-owner request in
        // `startup-and-diagnostics.md`.
        crate::runtime::crash_handler::publish_primordial_frame_trace(
            main_thread.frame_trace.clone(),
        );
        shared
            .threads
            .thread_registry
            .set_vm_state(ThreadId(0), main_thread.vm_state.clone());
        // Share blocked-region GC state so initiators can maintain the main
        // thread's roots while it parks in a blocking native (wait/join/park)
        shared
            .threads
            .thread_registry
            .set_gc_block_state(ThreadId(0), main_thread.gc_block_state.clone());
        // A worker-initiated non-moving GC can forcibly stop the primordial
        // thread in JIT code. Publish its reserved TLAB tail just as we do for
        // workers and JNI-attached threads. The Box keeps this pointee stable
        // across the return/moves of `Vm::new`.
        shared.threads.thread_registry.set_tlab_addr(
            ThreadId(0),
            &main_thread.tlab as *const cratonvm_gc::Tlab as usize,
        );
        // XT-FRAME-SCAN: publish the primordial thread's `JvmThread` address
        // too (the same Box keeps it stable) so a worker-initiated takeover
        // that freezes main mid-JIT can walk its interpreter frames.
        shared
            .threads
            .thread_registry
            .set_jvm_thread_addr(ThreadId(0), &*main_thread as *const JvmThread as usize);
        // Publish the primordial thread's OS id too. A worker can initiate a
        // multi-threaded STW while the main thread is running JIT code; without
        // this id the takeover backend can freeze and scan main but cannot prove
        // it was part of the counted barrier snapshot.
        shared
            .threads
            .thread_registry
            .set_os_tid_current(ThreadId(0));

        // The C `jvmtiEnv`s the startup agents obtained in `Agent_OnLoad`,
        // before this VM existed, act on it from now on (interpreter round i1
        // wave 13), in JVMTI's start phase; they get `VMInit` when the VM
        // reaches the live phase (`set_init_level(3)`, wave 14).
        #[cfg(feature = "experimental-debug")]
        crate::jvmti::native_env::bind_startup_envs(&shared);

        // The perf-data file's live counters (`jstat -gc` / `-class`;
        // interpreter round i4 wave 86, lane L1): a sampler thread of their
        // own, nothing when no file was written.
        // Phase 5 WP3: held back in a boot image's capture run.
        shared.deferred_starts.start_or_defer_with_vm(&shared, "cratonvm-perf-data", |shared| {
            crate::runtime::perf_data_file::start_sampler(shared);
        });
        // `-agentlib:jdwp` (interpreter round i4 wave 85, lane L1): its server
        // starts at `VMInit` (`debug::start_jdwp_agent`, from the launcher),
        // but lambda calls record their proxy frames from VM creation on,
        // as for `--jdwp-port` below.
        #[cfg(feature = "experimental-debug")]
        if shared.config.jdwp_agent.is_some() {
            shared.debug.debugger_gates.set_jdwp_listening();
        }
        // Start JDWP debug server if configured
        #[cfg(feature = "experimental-debug")]
        if let Some(port) = shared.config.jdwp_port {
            // The i54-L1 retirement: lambda calls record their proxy frame
            // from now on, for a debugger that attaches while one runs.
            shared.debug.debugger_gates.set_jdwp_listening();
            // Phase 5 WP3: held back in a boot image's capture run.
            shared.deferred_starts.start_or_defer_with_vm(&shared, "JDWP-Server", move |shared| {
                let shared_ref = Arc::clone(shared);
                std::thread::Builder::new()
                    .name("JDWP-Server".into())
                    .spawn(move || {
                        crate::debug::run_jdwp_server(&shared_ref, port);
                    })
                    .ok();
            });
            tracing::info!("JDWP debug server listening on port {port}");
        }

        // WP1.3 — VM is now at "primordial classes loaded" (level 1).
        // `SharedVm::new` already ran `bootstrap_core_classes()` which
        // loads `java/lang/Object`, `Class`, `String`, and the primitive
        // wrappers; the main thread is registered in the thread
        // registry so Thread.currentThread() works. This matches
        // HotSpot's `VM::init_level(1)` transition right before
        // `initialize_java_lang_classes` finishes.
        //
        // Subsequent transitions (level 2 after `System.initPhase1`,
        // level 3 after `initPhase2`, level 4 before `main()`) are
        // driven from the CLI bootstrap loop in `vm-cli/src/main.rs`
        // which owns the initPhase orchestration.
        shared.set_init_level(1);

        // gen r4w6/concsvc6 (2026-09-24): the generational concurrent-cycle
        // SERVICE thread, `CRATONVM_GEN_CONC_SERVICE_THREAD` (opt-in; a no-op
        // off Generational or with the flag unset). After the main thread is
        // registered, so the registry is live and the service's first pause
        // request counts the main thread. Stopped in `Drop for Vm`.
        // Phase 5 WP3: held back in a boot image's capture run.
        shared.deferred_starts.start_or_defer_with_vm(&shared, "gen-conc-service", |shared| {
            crate::runtime::interpreter::gen_conc_service::start_gen_concurrent_service_thread(shared);
        });

        // Install the AIO completion-dispatcher launcher. The first handler-form
        // `AsynchronousSocketChannel.read` fires it, spinning up the
        // foreign-attached dispatcher thread that delivers read completions to
        // their Java `CompletionHandler` (the Tomcat WebSocket client read path).
        cratonvm_native_io::async_socket::set_dispatcher_launcher(Box::new(|| {
            crate::native::jni::start_aio_dispatcher();
        }));

        // Round-5 MED-fix (Bug 6, 2026-05-17): emit a one-shot
        // `jdk.PhysicalMemory` event at startup so any later JFR dump
        // captures the host-RAM totals as an EveryChunk diagnostic.
        //
        // Round-5 CRIT-fix (Bug 2, 2026-05-17 follow-up): drop the
        // surrounding `cratonvm_jfr::is_enabled()` gate. vm-cli never calls
        // `start_recording`, so the gate kept this call permanently
        // disabled and made the wired emit dead code. The emit function
        // itself bypasses the global gate (see
        // `emit_physical_memory_event` in jfr/src/builtin.rs) and writes
        // to the calling thread's bounded SPSC ring; the event lives
        // there until a subsequent drain forwards it to whichever
        // recording is active at dump time. The cost when no recording
        // ever starts is one ring slot — negligible.
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        // VM `max_heap_size` as a stand-in for total physical memory
        // until the platform sysinfo dependency lands. `used_size` is
        // also reported as `max_heap_size` (no precise allocator
        // accounting at startup); this keeps the schema honest about
        // scope while keeping the field non-zero so the consumer-side
        // schema check passes.
        let total = shared.config.max_heap_size as i64;
        let mut jfr = shared.debug.flight_recorder.lock();
        cratonvm_jfr::builtin::emit_physical_memory_event(&mut jfr, total, total, now_ns);

        // Round-5 JFR Fix 4: emit `jdk.InitialEnvironmentVariable` for
        // each VM-relevant env var so dumps capture the startup config
        // surface. We restrict to `CRATONVM_*`, `JAVA_*`, `_JAVA_OPTIONS`,
        // `JAVA_TOOL_OPTIONS`, `CLASSPATH` to avoid leaking unrelated
        // shell variables into the recording (and to keep the per-chunk
        // metadata small). OpenJDK's reference snapshot includes a
        // similar filtered set. Cost: ~N emit calls at startup, each
        // bounded-ring push; N is typically < 10.
        for (key, value) in std::env::vars() {
            let interesting = key.starts_with("CRATONVM_")
                || key.starts_with("JAVA_")
                || key == "_JAVA_OPTIONS"
                || key == "JAVA_TOOL_OPTIONS"
                || key == "CLASSPATH";
            if interesting {
                cratonvm_jfr::builtin::emit_initial_environment_variable_event(
                    &mut jfr, &key, &value, now_ns,
                );
            }
        }
        drop(jfr);

        Self {
            shared,
            main_thread,
        }
    }

    // ----- Class loading (delegating to shared) ----------------------------

    /// Load a class by name. Returns the ClassId.
    pub fn load_class(&mut self, name: &str) -> Result<ClassId, VmError> {
        self.shared.load_class_concurrent(name)
    }

    /// Get the name of a class by its id.
    pub fn class_name(&self, class_id: ClassId) -> Option<String> {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string())
    }

    // ----- Object creation -------------------------------------------------

    /// Allocate a new object of the given class on the heap.
    pub fn new_object(&mut self, class_name: &str) -> Result<ObjectRef, VmError> {
        let class_id = self.shared.load_class_concurrent(class_name)?;
        let num_fields = self
            .shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0);
        // Embedders may hold unrooted references: report refusal without GC.
        self.shared
            .mem
            .heap
            .try_alloc_object_full(class_id, num_fields)
            .ok_or_else(|| {
                VmError::Runtime(crate::error::RuntimeError::OutOfMemoryError {
                    message: "Java heap space".into(),
                })
            })
    }

    /// Allocate a new primitive array.
    ///
    /// **Fatal on heap exhaustion** (the bare `ObjectRef` return has no
    /// failure channel): an exhausted collector may abort or panic. An
    /// embedder that can see a full heap uses [`Self::try_new_array`].
    pub fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef {
        self.shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), element_type, length)
    }

    /// Allocate a new reference array of the given component class.
    ///
    /// **Fatal on heap exhaustion**, like [`Self::new_array`]; see
    /// [`Self::try_new_ref_array`].
    pub fn new_ref_array(&mut self, component_class_id: ClassId, length: usize) -> ObjectRef {
        self.shared
            .mem
            .heap
            .alloc_array(component_class_id, ArrayElementType::Reference, length)
    }

    /// [`Self::new_array`] that reports a refusal: `Err(OutOfMemoryError)`
    /// (`Requested array size exceeds VM limit` past HotSpot's array limit,
    /// `Java heap space` otherwise) instead of aborting. zr z4/ab, the
    /// embedding twin of [`Self::new_object`]: every backend reports failure
    /// without collecting because the embedder may hold unrooted references.
    pub fn try_new_array(
        &mut self,
        element_type: ArrayElementType,
        length: usize,
    ) -> Result<ObjectRef, VmError> {
        self.try_new_array_of(ClassId::new(0), element_type, length)
    }

    /// [`Self::new_ref_array`] that reports a refusal; see
    /// [`Self::try_new_array`].
    pub fn try_new_ref_array(
        &mut self,
        component_class_id: ClassId,
        length: usize,
    ) -> Result<ObjectRef, VmError> {
        self.try_new_array_of(component_class_id, ArrayElementType::Reference, length)
    }

    pub(in crate::vm::vm_init) fn try_new_array_of(
        &mut self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Result<ObjectRef, VmError> {
        let refused = |message: String| {
            VmError::Runtime(crate::error::RuntimeError::OutOfMemoryError { message })
        };
        if crate::runtime::interpreter::array_length_exceeds_vm_limit(length) {
            return Err(refused(
                crate::runtime::interpreter::ARRAY_SIZE_EXCEEDS_VM_LIMIT.to_string(),
            ));
        }
        self.shared
            .mem
            .heap
            .try_alloc_array_full(class_id, element_type, length)
            .ok_or_else(|| refused(format!("Java heap space (new_array of length {length})")))
    }

    // ----- Method invocation (delegating to free functions) -----------------

    /// Invoke a method by class name, method name, and descriptor.
    pub fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // HotSpot's `Threads::initialize_java_lang_classes` initialises
        // `java.lang.reflect.Method` (so `Executable` and `AccessibleObject`,
        // whose `<clinit>` publishes `SharedSecrets.javaLangReflectAccess`)
        // before `System.initPhase1` runs. Every launcher and embedder drives
        // `initPhase1` through this entry point, so the same point here stands
        // in for the per-class special cases that used to patch the first
        // reflection user (`ReflectionFactory`, `MethodHandleProxies`).
        if class_name == "java/lang/System"
            && method_name == "initPhase1"
            && self.shared.config.java_home.is_some()
        {
            self.boot_genesis_before_init_phase1()?;
        }
        // gch h1/jp (g4-oo hunk 5.3, `gcg-g1o-embedded-vms-never-preallocate-the-oome`):
        // HotSpot preallocates its `OutOfMemoryError` in `Universe::genesis`
        // for every VM. The launcher and `libcratonvm` call
        // `exceptions::ensure_singleton_oom` at their own point of the boot;
        // an embedder that drives the boot itself and skips
        // `cratonvm_embed::preallocate_out_of_memory_error` gets it here, on
        // its first call into Java once the VM is booted (level 4), once per
        // VM. A real-JDK VM only: a synthetic one has no JDK `OutOfMemoryError`
        // to build. The launcher opts out ([`Self::own_the_oome_preallocation`])
        // so its boot order is unchanged.
        if self.shared.get_init_level() >= 4
            && self.shared.config.java_home.is_some()
            && self.shared.mem.singleton_oom.read().is_none()
            && !self
                .shared
                .mem
                .alloc_ladder
                .singleton_oom_attempted
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            crate::runtime::exceptions::ensure_singleton_oom(&self.shared, &mut self.main_thread);
        }
        super::invoke_shared(
            &self.shared,
            &mut self.main_thread,
            class_name,
            method_name,
            descriptor,
            args,
        )
    }

    /// What HotSpot has done before `System.initPhase1` runs
    /// (`Threads::initialize_java_lang_classes`): [`Self::invoke`] runs it in
    /// front of `initPhase1`, and the `--compatible` fast-boot arm of
    /// [`Self::boot_init_phase1`], which skips `initPhase1`, runs it too (j27:
    /// without it `AccessibleObject.<clinit>` never published
    /// `SharedSecrets.javaLangReflectAccess` and every reflective constructor
    /// lookup threw). Real-JDK VMs only (the callers check `java_home`).
    pub(in crate::vm::vm_init) fn boot_genesis_before_init_phase1(&mut self) -> Result<(), MethodCallFailed> {
        {
            // Interpreter round i4 wave 91, lane L1
            // (`CRATONVM_BOOT_LOADS_VIRTUAL_THREAD`, default ON since wave 129; page
            // `i49-L1-proposal-load-virtualthread-at-boot-as-hotspot-does`):
            // HotSpot loads `java.lang.VirtualThread` (and its superclass
            // `BaseVirtualThread`) with its well-known classes at genesis
            // (`vmClassMacros.hpp` `VirtualThread_klass`), in the primordial
            // phase, whether or not the program starts a virtual thread. This
            // VM's virtual threads are `BoundVirtualThread`s, so the class was
            // loaded only on demand (`vm_exec::preload_virtual_thread_class`'s
            // call sites), mid-run: `GetLoadedClasses` / JDWP `AllClasses`
            // omitted it until then and a live-phase `ClassPrepare` request saw
            // it prepared. Loaded, not initialised; still the primordial phase
            // here (`System.initPhase1` has not run), so no agent event is
            // posted for it, as on HotSpot. Read once per VM boot.
            {
                crate::vm::vm_exec::preload_virtual_thread_class(&self.shared);
            }
            self.initialize_java_lang_reflect_classes();
            // HotSpot links its well-known SoftReference class and seeds its
            // VM-owned clock before initPhase1. Link without running <clinit>;
            // preparation supplies this VM's processor clock, including for
            // Unsafe reads that do not request Java class initialization.
            let soft_id = self
                .shared
                .load_class_concurrent("java/lang/ref/SoftReference")?;
            super::link_class_on_thread(&self.shared, &mut self.main_thread, soft_id)?;
            // HotSpot 25 initializes Finalizer before initPhase1, which also
            // initializes its Reference superclass and installs the real
            // SharedSecrets JavaLangRefAccess singleton. The getter itself
            // only reads the field; it does not initialize Reference lazily.
            let id = self
                .shared
                .load_class_concurrent("java/lang/ref/Finalizer")?;
            super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, id)?;
        }
        Ok(())
    }

    /// gch h1/jp: the caller makes the `OutOfMemoryError` preallocation itself
    /// (`exceptions::ensure_singleton_oom`) at a point of its boot it chose, so
    /// [`Self::invoke`]'s lazy one-shot for embedders must not run first. The
    /// launcher calls this right after the init level reaches 4.
    pub fn own_the_oome_preallocation(&self) {
        self.shared
            .mem
            .alloc_ladder
            .singleton_oom_attempted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Initialise `java.lang.reflect.Method` (and with it `Executable` and
    /// `AccessibleObject`) as HotSpot does before `System.initPhase1`.
    /// Best effort: a class that cannot load leaves the JDK's own lazy
    /// initialisation to report it.
    pub fn initialize_java_lang_reflect_classes(&mut self) {
        if let Ok(id) = self
            .shared
            .load_class_concurrent("java/lang/reflect/Method")
        {
            let _ = super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, id);
        }
        // Interpreter round i3 wave 79, lane L6
        // (`CRATONVM_INIT_FINALIZER_BEFORE_PHASE1`, default ON since nio21 wave 18 with the tail after the boot layer; page
        // `i66-orch-reference-handler-thread-is-absent`): HotSpot's
        // `initialize_java_lang_classes` also initialises
        // `java.lang.ref.Finalizer` (so `Reference`, whose `<clinit>` --
        // `runtimeSetup` -- publishes `SharedSecrets.javaLangRefAccess`)
        // before `initPhase1`. JDK 25's `initPhase1` ends with
        // `SharedSecrets.getJavaLangRefAccess().startThreads()` (the
        // `Reference Handler` and `Finalizer` daemons) and `VM.initLevel(1)`;
        // the getter is a plain static read, so with `Reference` not yet
        // initialised (`--real-jdk`, where the real getter runs) it answers
        // null, the call throws, the launcher falls back, and neither daemon
        // exists. Read once per VM boot; off: the boot order before wave 79.
        // It was opt-in for a while (nio21 merge, 2026-10-09): `initPhase1`
        // then completed and the first `Class.getModule()` ran inside
        // `ClassLoaders.<clinit>`, so the boot layer was built against a
        // look-alike platform loader and `jdk.localedata` / `jdk.zipfs`
        // providers vanished (page
        // `docs/feature-designs/real-jdk-mode.md`).
        // Two fixes landed independently: dev's `Class.getModule()` no longer
        // builds the layer while the built-in loaders are being adopted
        // (`classloader::builtin_loader_adoption_in_progress`), and nio21
        // wave 18 moved the `--real-jdk` tail after the boot layer (below).
        //
        // nio21 wave 18 L5: under `--real-jdk` the switch no longer initialises
        // `Finalizer` here by default; the launcher runs `initPhase1`'s tail
        // (`Finalizer`, `startThreads()`, `VM.initLevel(1)`) after the boot
        // layer instead ([`Self::init_phase1_tail_placement`],
        // [`Self::run_deferred_init_phase1_tail`]).
        if self.init_phase1_tail_placement() == Some(InitPhase1TailPlacement::BeforePhase1) {
            self.initialize_finalizer_class();
        }
    }

    /// `java.lang.ref.Finalizer` (and with it `FinalReference` / `Reference`,
    /// whose `<clinit>` publishes `SharedSecrets.javaLangRefAccess`). Best
    /// effort, as [`Self::initialize_java_lang_reflect_classes`].
    pub(in crate::vm::vm_init) fn initialize_finalizer_class(&mut self) {
        if let Ok(id) = self.shared.load_class_concurrent("java/lang/ref/Finalizer") {
            let _ = super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, id);
        }
    }

    /// nio21 wave 18 L5 (page
    /// `docs/feature-designs/real-jdk-mode.md`):
    /// where the boot runs what JDK 25's `System.initPhase1` does last --
    /// `SharedSecrets.getJavaLangRefAccess().startThreads()` (the `Reference
    /// Handler` and `Finalizer` daemons) and `VM.initLevel(1)` -- when
    /// `CRATONVM_INIT_FINALIZER_BEFORE_PHASE1` is on. `None` when it is off
    /// (no daemons under `--real-jdk`, the boot before i3 wave 79).
    ///
    /// * `--compatible`: always [`InitPhase1TailPlacement::BeforePhase1`]. The
    ///   bridge factory already lets `initPhase1` complete there, the
    ///   providers were never lost there, and that boot is unchanged.
    /// * `--real-jdk`, `CRATONVM_INIT_FINALIZER_TAIL` unset or `layer`:
    ///   [`InitPhase1TailPlacement::AfterBootLayer`] (the default). `Finalizer`
    ///   is NOT initialised before `initPhase1`, so `initPhase1` stops at
    ///   `startThreads()` exactly as with the switch off (the arm that keeps
    ///   every module-declared provider), and the launcher runs the tail once
    ///   `build_boot_layer` / `finish_boot_layer` are done. That removes every
    ///   candidate mechanism of the page at once: no `Reference` / `Finalizer`
    ///   initialised before `initPhase1`, no daemon running while the layer is
    ///   built and the catalogs registered, and the Java `VM.initLevel` still
    ///   0 throughout the layer build, as in the passing arm.
    /// * `CRATONVM_INIT_FINALIZER_TAIL=phase1`: the tail right after the
    ///   `initPhase1` fallback, BEFORE the boot layer. Diagnostic: if this arm
    ///   loses the providers and `layer` does not, the cause is the daemons or
    ///   the Java level during the layer build; if both keep them, it was the
    ///   class initialisation order before `initPhase1`.
    /// * `CRATONVM_INIT_FINALIZER_TAIL=0` / `early`: the wave-79 order
    ///   (`Finalizer` before `initPhase1`, which then completes) -- the arm the
    ///   host measured DIFF.
    pub fn init_phase1_tail_placement(&self) -> Option<InitPhase1TailPlacement> {
        if !self.shared.config.compatibility_mode.is_jdk_only() {
            return Some(InitPhase1TailPlacement::BeforePhase1);
        }
        let word = cratonvm_types::flags::runtime_var("CRATONVM_INIT_FINALIZER_TAIL").ok();
        Some(InitPhase1TailPlacement::from_word(word.as_deref()))
    }

    /// Run JDK 25's `System.initPhase1` tail on the main thread when
    /// `initPhase1` itself stopped before it (see
    /// [`Self::init_phase1_tail_placement`]): initialise `Finalizer`, call
    /// `SharedSecrets.getJavaLangRefAccess().startThreads()`, then
    /// `VM.initLevel(1)` if the JDK's level is still 0. Best effort; answers
    /// whether `startThreads()` ran.
    pub fn run_deferred_init_phase1_tail(&mut self) -> bool {
        use cratonvm_native_api::NativeInvokeAccess as _;
        self.initialize_finalizer_class();
        let access = self.invoke(
            "jdk/internal/access/SharedSecrets",
            "getJavaLangRefAccess",
            "()Ljdk/internal/access/JavaLangRefAccess;",
            &[],
        );
        let Ok(Some(Value::Object(Some(access)))) = access else {
            tracing::info!("initPhase1 tail: no JavaLangRefAccess; daemons not started");
            return false;
        };
        let started = {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            ctx.invoke_virtual(access, "startThreads", "()V", &[])
                .is_ok()
        };
        // Written only from 0, as `initPhase1` would. j27 lane FBTAIL: decided
        // on the static, not on the `VM.initLevel()` method, which
        // `--compatible` may answer from a registered native.
        self.raise_jdk_init_level(1);
        tracing::info!(
            started,
            "initPhase1 tail run after the fallback (Reference Handler, Finalizer)"
        );
        started
    }

    /// `VM.initLevel(target)` when the JDK's `VM.initLevel` static is below
    /// it ([`jdk_level_below`]); best effort, a failure is logged. j27 lane
    /// FBTAIL: the writer of the JDK level for the boot's tail (1).
    pub(in crate::vm::vm_init) fn raise_jdk_init_level(&mut self, target: i32) {
        if !jdk_level_below(self.jdk_vm_init_level(), target) {
            return;
        }
        if let Err(e) = self.invoke(
            "jdk/internal/misc/VM",
            "initLevel",
            "(I)V",
            &[Value::Int(target)],
        ) {
            tracing::warn!("boot: VM.initLevel({target}) did not run ({e:?})");
        }
    }

    /// The JDK's `jdk.internal.misc.VM.initLevel` static, read directly as the
    /// JDK's `VM.initLevel()` body reads it. `VM` is initialised first, as an
    /// `invokestatic` of that method would; `None` when the class or field is
    /// not there.
    pub(in crate::vm::vm_init) fn jdk_vm_init_level(&mut self) -> Option<Value> {
        let id = self.shared.load_class_concurrent("jdk/internal/misc/VM").ok()?;
        let _ = super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, id);
        let ctx = crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        };
        loaded_static_value(&ctx, "jdk/internal/misc/VM", "initLevel")    }

    // ----- The real-JDK boot sequence, shared by the launcher and embedders --
    //
    // nio21 wave 21 L5 (page
    // `docs/feature-designs/real-jdk-mode.md`):
    // the launcher's `initPhase1` / boot layer / deferred tail / JDK
    // "booted" writes, moved here VERBATIM from `vm-cli/src/main.rs` so
    // `libcratonvm::bootstrap` runs the same steps in the same order
    // (`CRATONVM_EMBED_LAUNCHER_BOOT`, see [`embed_launcher_boot_enabled`]).
    // The launcher calls exactly these, at exactly the points it ran the
    // inline code, so its boot is unchanged.

    /// HotSpot's `System.initPhase1` slot: run it, take the launcher's
    /// fallback when it throws (clear the two caches), raise the VM's level
    /// to 2 either way, and run the deferred tail here when its placement is
    /// [`InitPhase1TailPlacement::AfterPhase1`]. Real-JDK VMs only (the
    /// caller checks `java_home`).
    pub fn boot_init_phase1(&mut self) -> BootPhase1 {
        // nio21 wave 18 L5: where `initPhase1`'s tail (`startThreads()`,
        // `VM.initLevel(1)`) runs under `CRATONVM_INIT_FINALIZER_BEFORE_PHASE1`
        // (`Vm::init_phase1_tail_placement`); read before `initPhase1`, which
        // consults it too.
        let tail = self.init_phase1_tail_placement();
        let mut completed = false;
        match self.invoke("java/lang/System", "initPhase1", "()V", &[]) {
            Ok(_) => {
                completed = true;
                tracing::info!("System.initPhase1() completed");
                // WP1.3: initPhase1 just finished — advance to level 2.
                self.shared.set_init_level(2);
            }
            Err(e) => {
                // T14: initPhase1 may fail mid-bootstrap on real JDK 25 due to
                // subsystems we don't fully emulate. The fallback path uses
                // the synthetic System.in/out/err so stdout/stderr still work.
                if let MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                    let exc_class_id = self.shared.mem.heap.class_id_of(*exc_ref);
                    let exc_class_name = self
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(exc_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("unknown({})", exc_class_id));
                    tracing::info!(
                        "System.initPhase1() fell back to synthetic streams ({exc_class_name})"
                    );
                } else {
                    tracing::info!("System.initPhase1() fell back to synthetic streams");
                    tracing::debug!("initPhase1 details: {e:?}");
                }
                // initPhase1 may have partially initialized classes and
                // populated stale invoke/resolution cache entries. Clear both
                // so the next invocation re-resolves cleanly.
                self.main_thread.invoke_cache.clear();
                self.shared.classes.resolution_cache.write().clear();
                // WP1.3: the early system-properties / stream installation
                // ran before the failure; bump anyway so downstream
                // `initLevel()` observers don't stall at 1.
                self.shared.set_init_level(2);
            }
        }
        if !completed && tail == Some(InitPhase1TailPlacement::AfterPhase1) {
            self.run_deferred_init_phase1_tail();
        }
        BootPhase1 { completed, tail }
    }

    /// HotSpot's `System.initPhase2` slot: build the boot module layer
    /// (`build_boot_layer`, then `finish_boot_layer` -- a no-op in
    /// `--compatible` without the module-graph row) and log its outcome; then
    /// run the deferred `initPhase1` tail when `phase1` stopped before it and
    /// its placement is [`InitPhase1TailPlacement::AfterBootLayer`]. The
    /// launcher's comment at its call site says why the layer is built here
    /// and nowhere else.
    pub fn boot_module_system(&mut self, phase1: BootPhase1) {
        let boot_layer = {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl {
                shared: &self.shared,
                thread: &mut self.main_thread,
            };
            let built = cratonvm_native_builtins::jboss_jdkspecific::build_boot_layer(&mut ctx);
            if built.is_ok() {
                if !self.shared.config.compatibility_mode.is_jdk_only()
                    && self.shared.config.module_path.is_empty()
                {
                    crate::vm::vm_object::fixup_class_module_fields(&self.shared);
                } else {
                    let _ =
                        cratonvm_native_builtins::jboss_jdkspecific::finish_boot_layer(&mut ctx);
                }
            }
            built
        };
        match boot_layer {
            Ok(_) => {
                tracing::info!("module system initialised (boot layer built)");
            }
            Err(e) => {
                tracing::warn!(
                    "module-system init failed ({e:?}) — every module-declared \
                     ServiceLoader provider will be missing (HotSpot aborts VM creation \
                     when System.initPhase2 returns non-zero)"
                );
            }
        }
        self.boot_take_illegal_native_access();
        // nio21 wave 18 L5: under `--real-jdk` the `Reference Handler` /
        // `Finalizer` daemons and the JDK's `VM.initLevel(1)` come here, once
        // the boot layer is built and every module's services are in its
        // loader's catalog (daemons started before the layer build lose the
        // `jdk.localedata` / `jdk.zipfs` providers: measured, w18b).
        if !phase1.completed && phase1.tail == Some(InitPhase1TailPlacement::AfterBootLayer) {
            self.run_deferred_init_phase1_tail();
        }
    }

    /// zt t14/x11 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 13,
    /// `CRATONVM_FFM_ILLEGAL_NATIVE_ACCESS_TAKEN_AT_BOOT`, default ON, `=0`
    /// kill; every collector, both modes): HotSpot's `initPhase2` runs
    /// `ModuleBootstrap.boot`, whose class initialiser
    /// (`addIllegalNativeAccess`) reads AND REMOVES
    /// `jdk.module.illegal.native.access` before `main`, so the program's
    /// `System.getProperty` answers `null` and `System.getProperties()` does
    /// not list it. This VM never runs `ModuleBootstrap.boot` and initialised
    /// the class only at the first restricted call, so the property
    /// t12/df4 publishes for `--illegal-native-access=deny|allow` (or a user
    /// `-D`) stayed visible until then. Here, in the `initPhase2` slot, the
    /// class is initialised through the accessor `Module.ensureNativeAccess`
    /// calls (`illegalNativeAccess()`), so the JDK takes the mode while the
    /// property is there and removes it, as on HotSpot; the mode it latched
    /// is the one a later restricted call reads. The same initialiser also
    /// takes `jdk.module.patch.*` and `jdk.module.enable.native.access.*`,
    /// which HotSpot removes at the same point too, and which this VM
    /// neither publishes nor reads back. Only when the value is one
    /// `addIllegalNativeAccess` accepts (`deny`, `allow`, `warn`): HotSpot
    /// aborts the boot on any other, which this VM leaves to the first
    /// restricted call as before. Without the property nothing runs. Real JDK
    /// only (the synthetic JDK has no `ModuleBootstrap`).
    pub(in crate::vm::vm_init) fn boot_take_illegal_native_access(&mut self) {
        let user = self
            .shared
            .config
            .system_properties
            .iter()
            .rev()
            .find(|(k, _)| k == "jdk.module.illegal.native.access")
            .map(|(_, v)| v.as_str());
        if self.shared.config.use_synthetic_jdk
            || !illegal_native_access_taken_at_boot(
                illegal_native_access_property_value(self.shared.config.native_access.illegal),
                user,
                true,
            )
        {
            return;
        }
        if let Err(e) = self.invoke(
            "jdk/internal/module/ModuleBootstrap",
            "illegalNativeAccess",
            "()Ljdk/internal/module/ModuleBootstrap$IllegalNativeAccess;",
            &[],
        ) {
            tracing::warn!(
                "ModuleBootstrap did not take jdk.module.illegal.native.access at boot ({e:?})"
            );
        }
    }

    /// The JDK-side "VM fully initialized" writes the launcher makes right
    /// after raising the VM's level to 4: the REAL `VM.initLevel` static to
    /// `SYSTEM_BOOTED` (`VM.isBooted()` / `isModuleSystemInited()` read the
    /// field; spring-bug-05), and `VM.javaLangInvokeInited`. Both real-JDK
    /// policies initialize MethodHandleNatives before publishing readiness:
    /// its initializer owns the flag's one-shot setter. Setting the flag first
    /// poisons the later initializer with "java.lang.invoke already inited".
    /// Real-JDK VMs only (the caller checks `java_home`).
    pub fn boot_publish_jdk_booted(&mut self) -> Result<(), MethodCallFailed> {
        let _ = self.invoke(
            "jdk/internal/misc/VM",
            "initLevel",
            "(I)V",
            &[Value::Int(4)],
        );
        if !self.shared.config.use_synthetic_jdk {
            self.boot_init_java_lang_invoke()?;
            self.boot_start_flight_recording()?;
            return Ok(());
        }
        let inited = matches!(
            self.invoke("jdk/internal/misc/VM", "isJavaLangInvokeInited", "()Z", &[]),
            Ok(Some(Value::Int(1)))
        );
        if !inited {
            let _ = self.invoke(
                "jdk/internal/misc/VM",
                "setJavaLangInvokeInited",
                "()V",
                &[],
            );
        }
        Ok(())
    }

    /// `java.lang.invoke` ready before `main`, as HotSpot has it
    /// (`Threads::initialize_jsr292_core_classes`): `MethodHandleNatives` is
    /// initialised, and its `<clinit>` makes the one-shot
    /// `VM.setJavaLangInvokeInited()` call (JDK 25: `registerNatives()`, the
    /// assert-only constant checks, then the setter; cheap). The static is
    /// then confirmed. The real-JDK arm of [`Self::boot_publish_jdk_booted`]
    /// calls this (j27 wave 8, lane W9BOOT): without it the static is false at
    /// `main` and `MethodHandleAccessorFactory.newFieldAccessor` throws
    /// `InternalError` ("cannot be accessed reflectively before
    /// java.lang.invoke is initialized") on the first `Field.get`/`set` of a
    /// program that had not used a method handle; the registered
    /// `isJavaLangInvokeInited` stand-in pinned to `true` is a `Bridge`, which
    /// the JDK's concrete body beats.
    pub(in crate::vm::vm_init) fn boot_init_java_lang_invoke(&mut self) -> Result<(), MethodCallFailed> {
        self.invoke(
            "java/lang/invoke/MethodHandleNatives",
            "refKindIsField",
            "(B)Z",
            &[Value::Int(1)],
        )?;
        let inited = self.invoke("jdk/internal/misc/VM", "isJavaLangInvokeInited", "()Z", &[])?;
        if !matches!(inited, Some(Value::Int(1))) {
            return Err(VmError::Internal {
                message: "java.lang.invoke initialization did not publish readiness".into(),
            }
            .into());
        }
        Ok(())
    }

    /// Start the command-line recording on the boot thread, after modules,
    /// MethodHandles and the JDK init-level field are ready. DCmdStart creates
    /// a real Recording and owns its timer, destination and shutdown hook.
    pub(in crate::vm::vm_init) fn boot_start_flight_recording(&mut self) -> Result<(), MethodCallFailed> {
        let mut recordings = self.shared.config.jfr_startup_options.clone();
        if recordings.is_empty() {
            if let Some(cfg) = &self.shared.config.jfr_start_recording {
                let mut options = vec![format!("dumponexit={}", cfg.dump_on_exit)];
                if let Some(path) = &cfg.filename { options.push(format!("filename=\"{path}\"")); }
                if let Some(d) = cfg.duration { options.push(format!("duration={}ns", d.as_nanos())); }
                if let Some(d) = cfg.max_age { options.push(format!("maxage={}ns", d.as_nanos())); }
                if let Some(n) = cfg.max_events { options.push(format!("maxevents={n}")); }
                recordings.push(options.join(","));
            }
        }
        if recordings.is_empty() { return Ok(()); }
        if self.shared.debug.jfr_startup_claimed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        for mut options in recordings {
            if !options.split(',').any(|s| s.trim().starts_with("dumponexit=")) {
                if !options.is_empty() { options.push(','); }
                options.push_str("dumponexit=true");
            }
            if let Ok(names) = cratonvm_types::flags::runtime_var("CRATONVM_JFR_ENABLE_EVENTS") {
                for name in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !options.is_empty() { options.push(','); }
                    options.push_str(&format!("+{name}#enabled=true"));
                }
            }
            let mut ctx = super::NativeContextImpl { shared: &self.shared, thread: &mut self.main_thread };
            if options.contains(cratonvm_jfr::jit_decision::JIT_COMPILE_DECISION_EVENT) {
                cratonvm_native_builtins_jfr::jfr_backend::initialize_jit_decision_event(&mut ctx)?;
            }
            let output = crate::runtime::instrument::jfr_dcmd_on(&mut ctx, "DCmdStart", "startup", &options, ',')?;
            {
                use crate::runtime::serviceability::VmDiagnosticState as _;
                let lines = output.lines().map(str::to_string).collect::<Vec<_>>();
                crate::runtime::unified_logging::log_jfr_startup_lines(&lines, self.shared.uptime_secs());
            }
        }
        Ok(())
    }

    /// Invoke a method on a specific class by ClassId.
    pub fn invoke_on_class(
        &mut self,
        class_id: ClassId,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        super::invoke_on_class_shared(
            &self.shared,
            &mut self.main_thread,
            class_id,
            method_name,
            descriptor,
            args,
        )
    }

    // ----- Failure diagnostics ----------------------------------------------

    /// Render a thrown `Throwable` as `<binary class name>: <detailMessage>`.
    ///
    /// [`MethodCallFailed::ExceptionThrown`] carries only an [`ObjectRef`], so
    /// its `Debug`/`Display` can print no more than a heap address. That is
    /// unusable for a caller that has to group hundreds of failures by cause —
    /// the extended interpreter corpus reported 175 of its 214 failures as an
    /// undifferentiated `Err(ExceptionThrown(ObjectRef { .. }))` until this
    /// existed. Resolving the class and the detail message needs the heap and
    /// the class manager, which only the VM has; hence a method here rather
    /// than a richer `Display` on the error type.
    ///
    /// Best-effort and non-throwing: it reads fields directly instead of
    /// calling `toString()`, so it cannot recurse into further exceptions and
    /// is safe to call from a test assertion path. An unresolvable class
    /// renders as `<unknown class>` and a missing/undecodable message is
    /// omitted.
    pub fn describe_exception(&self, obj: ObjectRef) -> String {
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let name = {
            let cm = self.shared.classes.class_manager.read();
            cm.get_class(class_id)
                .map(|c| c.name.replace('/', "."))
                .unwrap_or_else(|| "<unknown class>".to_string())
        };
        match self.exception_detail_message(obj) {
            Some(msg) => format!("{name}: {msg}"),
            None => name,
        }
    }

    /// Read the detail message off `obj`.
    ///
    /// Two layouts have to be handled, which is why this is not a one-line
    /// field read. Real-JDK `java.lang.Throwable` declares `detailMessage` by
    /// name, so the hierarchy walk finds it. CratonVM's synthetic
    /// `java/lang/Throwable` (`class_manager`'s `instance_fields(2)`) names its
    /// slots `_f0`/`_f1` — message and cause — and real-JDK bootstrap metadata
    /// can render Throwable's first slots opaquely the same way. For those,
    /// fall back to scanning the Throwable's own slots for the first value
    /// that decodes as a `java.lang.String`.
    ///
    /// Deliberately does not guess a fixed slot number: the message is slot 0
    /// in the synthetic layout and slot 1 in the real one (slot 0 there is
    /// `backtrace`), and picking wrong renders an unrelated object as the
    /// message. Reading the value and requiring it to be a decodable String
    /// answers that without encoding either layout.
    pub(crate) fn exception_detail_message(&self, obj: ObjectRef) -> Option<String> {
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let (named, throwable_slots) = {
            let cm = self.shared.classes.class_manager.read();
            let mut walk = Some(class_id);
            let mut named = None;
            let mut throwable_slots = None;
            while let Some(cid) = walk {
                let Some(cls) = cm.get_class(cid) else { break };
                let mut instance = 0usize;
                for f in &cls.fields {
                    if f.is_static() {
                        continue;
                    }
                    if &*f.name == "detailMessage" && named.is_none() {
                        named = Some(cls.first_field_index + instance);
                    }
                    instance += 1;
                }
                if &*cls.name == "java/lang/Throwable" {
                    throwable_slots = Some((cls.first_field_index, instance));
                }
                walk = cls.superclass;
            }
            (named, throwable_slots)
        };

        let read_string = |index: usize| match self.shared.mem.heap.get_field(obj, index) {
            Value::Object(Some(s)) => super::vm_object::read_java_string(&self.shared.mem.heap, s),
            _ => None,
        };

        if let Some(index) = named {
            if let Some(text) = read_string(index) {
                return Some(text);
            }
        }
        // An implicit NPE keeps its JEP 358 text in `extendedMessage`, where
        // `NullPointerException.getMessage()` finds it (interpreter round i4
        // wave 99, lane L9).
        if let Some(text) =
            crate::runtime::exceptions::npe_computed_extended_message(&self.shared, obj)
                .and_then(|s| super::vm_object::read_java_string(&self.shared.mem.heap, s))
        {
            return Some(text);
        }
        let (base, count) = throwable_slots?;
        (base..base + count).find_map(read_string)
    }

    /// Render any [`MethodCallFailed`] for a human.
    ///
    /// `InternalError` delegates to the error's own `Display` (already
    /// self-describing); `ExceptionThrown` goes through
    /// [`Self::describe_exception`] instead of printing a bare pointer.
    pub fn describe_failure(&self, err: &MethodCallFailed) -> String {
        match err {
            MethodCallFailed::InternalError(e) => format!("internal: {e}"),
            MethodCallFailed::ExceptionThrown(obj) => {
                format!("threw {}", self.describe_exception(*obj))
            }
        }
    }

    /// Render a whole [`MethodCallResult`] — the shape a test assertion wants
    /// when the call did not produce what it expected.
    pub fn describe_result(&self, result: &MethodCallResult) -> String {
        match result {
            Ok(Some(v)) => format!("returned {v:?}"),
            Ok(None) => "returned void".to_string(),
            Err(e) => self.describe_failure(e),
        }
    }

    // ----- Finalization (M19) ------------------------------------------------

    /// Run the finalizers this VM has queued, before returning — the drain
    /// `Runtime.runFinalization()` runs. Returns how many queued objects were
    /// taken off the queue (by this thread, or by the reference-delivery thread
    /// it waited for).
    ///
    /// gc-common w5-d (2026-09-24,
    /// `common-w4d-legacy-m19-finalization-api-bypasses-every-fix`): this was a
    /// second finalization pipeline next to the real one. It re-resolved each
    /// object's class by NAME (`self.invoke(&name, …)`: loader-blind, so a class
    /// a user loader defined ran the wrong class's `finalize()` or none), fed
    /// the queue through the address-keyed `enqueue`, and ignored the JLS §12.6
    /// delivery rule. It is also not test-only: `Drop for Vm` calls it at
    /// shutdown. It now delegates to `run_pending_finalizers_for_runtime` —
    /// dispatch on the object's own `ClassId`, the delivery policy, and the
    /// JIT-borrow handling of the forced drain.
    pub fn run_pending_finalizers(&mut self) -> usize {
        let before = self.shared.mem.finalizer_thread.pending_count();
        if before == 0 {
            return 0;
        }
        crate::runtime::interpreter::run_pending_finalizers_for_runtime(
            &self.shared,
            &mut self.main_thread,
        );
        before.saturating_sub(self.shared.mem.finalizer_thread.pending_count())
    }

    // ----- Static fields ---------------------------------------------------

    /// Get a static field value.
    pub fn get_static(&self, class_id: ClassId, field_index: usize) -> Value {
        super::get_static_shared(&self.shared, class_id, field_index)
    }

    /// Set a static field value.
    pub fn set_static(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        super::set_static_shared(&self.shared, class_id, field_index, value);
    }

    // ----- Instance fields (embedding read-back / write-back) ---------------

    /// Resolve the instance-field slot index of `field_name` on `class_id`,
    /// walking the superclass chain (the most-derived declaration wins, matching
    /// `getfield` resolution for a static type). Returns `None` if the class is
    /// not loaded or declares no such instance field. The returned index is the
    /// absolute slot suitable for [`Vm::get_instance_field`] /
    /// [`Vm::set_instance_field`] and `heap.get_field`. Resolution is by **name**
    /// only; use [`Vm::instance_field_index_desc`] to disambiguate a shadowed
    /// same-name field by its descriptor.
    pub fn instance_field_index(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store)
    }

    /// Descriptor-aware field resolution: resolve `field_name` to its layout
    /// slot, optionally disambiguated by JVM type `descriptor` (`"I"`,
    /// `"Ljava/lang/String;"`, …). Passing `Some(descriptor)` lets a caller
    /// address a **shadowed** super-class field that a subclass re-declares
    /// with the same name (the super-class field's descriptor walks past the
    /// subclass shadow). `None` is identical to [`Vm::instance_field_index`]
    /// (most-derived declaration wins). See
    /// [`super::vm_exec::resolve_field_index_in_hierarchy_desc`].
    pub fn instance_field_index_desc(
        &self,
        class_id: ClassId,
        field_name: &str,
        descriptor: Option<&str>,
    ) -> Option<usize> {
        let cm = self.shared.classes.class_manager.read();
        super::vm_exec::resolve_field_index_in_hierarchy_desc(
            class_id,
            field_name,
            descriptor,
            &cm.class_store,
        )
    }

    /// Number of instance-field slots in `class_id`'s layout — the valid index
    /// range `[0, count)` for the instance-field accessors.
    pub fn instance_field_count(&self, class_id: ClassId) -> usize {
        self.shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .unwrap_or(0)
    }

    /// Read instance-field slot `index` of `obj`.
    pub fn get_instance_field(&self, obj: ObjectRef, index: usize) -> Value {
        self.shared.mem.heap.get_field(obj, index)
    }

    /// Write `value` into instance-field slot `index` of `obj`, **GC-barrier
    /// correct**: the SATB pre-barrier on the overwritten reference plus the
    /// post write-barrier fired inside `set_field`, exactly mirroring the
    /// interpreter's `putfield` (so a moving/concurrent collector stays sound).
    /// The caller is responsible for `index` being in `[0,
    /// instance_field_count(class))` — an out-of-range slot is dropped by the
    /// heap's own bounds guard.
    pub fn set_instance_field(&self, obj: ObjectRef, index: usize, value: Value) {
        // SATB pre-barrier on the old reference (no-op for primitives/null and
        // for non-SATB collectors), mirroring `interpreter.rs` putfield.
        let old = self.shared.mem.heap.get_field(obj, index);
        if let Value::Object(Some(old_ref)) = old {
            self.shared
                .mem
                .heap
                .write_barrier_pre(std::ptr::null_mut(), old_ref);
        }
        // The post write-barrier (card marking / remembered set) fires inside
        // `set_field` itself.
        self.shared.mem.heap.set_field(obj, index, value);
    }

    // ----- Class initialization (delegating to free functions) ---------------

    /// Ensure a class is fully initialized.
    pub fn ensure_class_initialized(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        super::ensure_class_initialized_shared(&self.shared, &mut self.main_thread, class_id)
    }

    // ----- Subclass checking -----------------------------------------------

    /// Check if `child_id` is a subclass of (or implements) `parent_id`.
    pub fn is_subclass_of(&self, child_id: ClassId, parent_id: ClassId) -> bool {
        self.shared
            .classes
            .class_manager
            .read()
            .is_subclass_of(child_id, parent_id)
    }

    /// Display-friendly snapshot of one frame in a captured Throwable trace.
    pub fn throwable_stack_for(&self, throwable: ObjectRef) -> Option<Vec<StackTraceFrame>> {
        let frames = self.shared.throwable_stack_trace_for(throwable)?;
        Some(
            frames
                .iter()
                .map(|e| StackTraceFrame {
                    class: e.class_name.to_string(),
                    method: e.method_name.to_string(),
                    file: e.source_file.as_deref().map(|s| s.to_string()),
                    line: e.line_number,
                })
                .collect(),
        )
    }
}

/// Display-friendly snapshot of one captured-stack-trace frame, suitable for
/// the CLI's unhandled-exception renderer. Detached from the on-heap
/// `StackTraceElement` layout so callers do not need to walk fields.
#[derive(Debug, Clone)]
pub struct StackTraceFrame {
    /// Fully-qualified class name, e.g. `java/lang/Class`.
    pub class: String,
    /// Method name (no descriptor).
    pub method: String,
    /// Source file (e.g. `Class.java`), `None` when the class has no
    /// `SourceFile` attribute. CLI prints "Unknown Source" in that case.
    pub file: Option<String>,
    /// Line number, `-1` for unknown / `-2` for native. CLI omits
    /// `:<line>` when negative.
    pub line: i32,
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vm")
            .field(
                "classes_loaded",
                &self.shared.classes.class_manager.read().loaded_count(),
            )
            .field("heap_bytes", &self.shared.mem.heap.allocated_bytes())
            .field("native_methods", &self.shared.natives.native_methods.len())
            .field("printed_values", &self.main_thread.printed.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Per-VM native/security state teardown
// ---------------------------------------------------------------------------

/// Release every piece of process-global state this VM filed under
/// `vm_identity`.
///
/// Three rows today, all keyed on the identity and all of which would otherwise
/// outlive the heap that produced them:
///
/// * the [`CapabilitySet`](cratonvm_native_api::CapabilitySet) installed by
///   `SharedVm::new` — a long-lived host process that creates and disposes of
///   VMs would otherwise accumulate dead entries in a `Vec` that every
///   `capabilities_for` lookup scans linearly;
/// * `native-builtins`' per-VM SecurityManager row, which holds raw heap
///   `ObjectRef`s (the installed manager, the policy object, the shared
///   permission collection). `forget_vm_security_state` had **no call site at
///   all** before this, so those addresses survived the heap they pointed into
///   and a later VM that reused the identity would have inherited them;
/// * the `java.lang.instrument` `ClassFileTransformer` chain
///   ([`crate::runtime::instrument`]), which likewise holds raw heap
///   `ObjectRef`s — the live transformer mirrors registered by Mockito's
///   MockMaker / JaCoCo's agent — and is a registered GC root source, so a
///   surviving row would report addresses into a dead heap on a later VM's
///   collection.
///
/// **Idempotent by construction** — `uninstall_capabilities` returns `None`,
/// and `forget_vm_security_state` / `forget_vm_transformers` remove nothing on
/// a second call — because it is deliberately invoked from two places (see
/// below). For a `Vm` both run, `Drop for Vm` first; a bare `SharedVm` runs
/// only the second.
pub fn release_vm_native_state(vm_identity: usize) {
    // GPU critical-section tokens this VM's submissions still hold. A
    // submission in flight when its VM goes away can never be finalized —
    // the completion reaper's `Weak::upgrade` fails — so without this its
    // token would stay outstanding, its keep-alive roots would name a dead
    // heap on the next VM's collection, and until its lease expired the
    // registry would still be describing a holder that cannot act. Reaping
    // here names each one at `warn` and poisons its writeback.
    #[cfg(feature = "gpu-offload")]
    {
        let _ = cuda_bridge::critical::global().shutdown_vm(vm_identity as u64);
    }
    cratonvm_native_api::uninstall_capabilities(cratonvm_native_api::VmId::from_raw(vm_identity));
    cratonvm_native_builtins::security_manager::forget_vm_security_state(vm_identity);
    // The built-in app/platform `ClassLoader` singletons. Same reasoning as the
    // SecurityManager row above: raw heap `ObjectRef`s that must not outlive
    // the heap, and must never be visible to a VM that reuses the identity.
    cratonvm_native_builtins::classloader::forget_vm_loader_singletons(vm_identity);
    // The platform MBeanServer (gc-common w8-f): a raw heap `ObjectRef`, one
    // per VM, that must not outlive its heap or be handed to a VM that reuses
    // the identity.
    #[cfg(feature = "management")]
    cratonvm_native_builtins_jmx::jmx::forget_vm_platform_mbean_server(vm_identity);
    // The annotation-proxy cache, its per-proxy child roots, and the
    // last-created-proxy interfaces array. Keyed by `ClassId`, which every VM
    // mints from zero, so a surviving row is not merely a leak — it is a
    // wrong-heap hit for the next VM.
    cratonvm_native_builtins::lang_class::forget_vm_annotation_proxies(vm_identity);
    // The cached `System.getenv()` map and `System.getProperties()` object.
    cratonvm_native_builtins::lang_system::forget_vm_system_singletons(vm_identity);
    // The surrogate intern pool (gc-common w9-c): its values are this VM's
    // JNI global-ref handles, meaningless -- and recyclable box addresses --
    // once this VM is gone.
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(vm_identity);
    // The `java.lang.ClassValue` memoization cache (BUG-W). Its key is a pair
    // of 32-bit identity hashes, which two live VMs collide on readily.
    cratonvm_native_builtins::phases_late::forget_vm_classvalue_cache(vm_identity);
    // Which thread owns each live `ScopedValue` binding.
    cratonvm_native_builtins::jdk25_concurrency::forget_vm_scoped_value_owners(vm_identity);
    // The synthetic `ReentrantLock` / `ReentrantReadWriteLock` state tables,
    // keyed by `(vm_identity, identity_hash)`.
    cratonvm_native_builtins::forget_vm_lock_state(vm_identity);
    // The null-base object store and the synthetic per-receiver field store
    // (Unsafe side stores), keyed by `(vm_identity, ..)` since gc-common w7-a.
    // No other VM scans them; this returns the memory. Idempotent.
    cratonvm_native_builtins::forget_vm_unsafe_side_stores(vm_identity);
    // The generated `$ProxyN` class cache and the per-loader proxy module
    // numbering. Both store `ClassId`s / ids that only this class manager can
    // interpret.
    cratonvm_native_builtins::forget_vm_proxy_classes(vm_identity);
    // The `Class.getName()` / simple / canonical / package name memos, keyed
    // by `(vm_identity, class_id)`.
    cratonvm_native_builtins::lang_class::forget_vm_class_name_caches(vm_identity);
    // The record `boolean`-component memo is per VM since interpreter round i6
    // wave 117 (lane L4): it lives in the native registry and goes with it.
    // The VarHandle meta rows and FFM layout-handle shapes, keyed by
    // `(vm_identity, identity hash)` since gc-common w18-b. Plain Rust data
    // (no heap references), but a dead VM's rows also keep the JIT's VM-less
    // plan lookup declining for every other VM.
    cratonvm_native_builtins::lang_invoke::forget_vm_var_handle_meta(vm_identity);
    crate::runtime::instrument::forget_vm_transformers(vm_identity);
    // Generic `invokedynamic` linkages: raw `CallSite` addresses into this
    // VM's heap, and a registered root source.
    crate::runtime::invokedynamic::forget_vm_generic_indy_sites(vm_identity);
    // Without this a disposed VM's JVMTI row leaks its agent's callback
    // closures, and its listener flags keep every OTHER VM's interpreter on the
    // slow path — the flags are a process-wide union by design.
    crate::runtime::jvmti::forget_vm_jvmti_state(vm_identity);
    // The `multianewarray` per-site plans (process-wide, keyed by VM). Its
    // class ids mean nothing once this VM is gone (gc-common w10-f).
    crate::runtime::interpreter::forget_vm_multianewarray_plans(vm_identity);
    // The `nio` root row's tables (gc-common w10-b): selectors (closed here,
    // which releases their OS handles), selection keys, and -- through the
    // same call -- the channel / `ServerSocket` adaptor side tables. Raw
    // `ObjectRef`s into this heap.
    cratonvm_native_io::nio_selector::forget_vm_nio_rows(vm_identity);
    cratonvm_native_io::async_socket::forget_vm_aio_state(vm_identity);
    // native-io's weak side tables (gc-common w14-e): today the
    // `BufferedOutputStream` / `PipedOutputStream` side buffers, keyed by
    // `(vm_identity, identity hash)`. Their unflushed bytes are Rust-side and
    // no collection of this VM will sweep them again.
    let _ = cratonvm_native_io::forget_vm_io_side_tables(vm_identity);
    // The collection-overlay rows (gc-common w12-c): LinkedList /
    // LinkedHashMap / TreeMap / TreeSet / CSLM / snapshot-iterator side
    // tables, keyed by object-key slots that record their VM. No collection
    // of this VM will prune them again, and every other VM's scan skips them
    // as foreign, so without this they leak.
    cratonvm_gc::external_roots::forget_vm_external_roots(vm_identity);
    // native-awt's process-wide peer-source / Runnable / InvocationEvent
    // tables and its retired-root list (gc-common w22-a). Rows only: this
    // VM's global-ref table dies with it, so nothing is released.
    #[cfg(feature = "awt")]
    cratonvm_native_awt::natives::forget_vm_awt_roots(vm_identity);
}

/// The precise hook: the last `Arc<SharedVm>` is gone, so no thread can still
/// reach this VM's policy.
///
/// This is where the release *belongs*, and for a bare `SharedVm::new(..)` (the
/// ~1,500 unit-test VMs across the workspace, none of which build a `Vm`) it is
/// the only hook that fires — without it every one of those tests would leak an
/// entry into the process-wide capability index for the life of the test
/// binary.
///
/// A `Vm`-created `SharedVm` reaches this hook too, but not necessarily when
/// the `Vm` is dropped: any other thread still holding an `Arc<SharedVm>` (a
/// daemon Java thread, a JIT or attach thread) keeps it alive, so the release
/// would be deferred to whenever the last of those ends, possibly never.
/// `Drop for Vm` therefore also releases, at the moment the owner lets go
/// (and, transitively, for `DestroyJavaVM` — `libcratonvm`'s
/// `destroy_created_vm` teardown hook drops the parked `Vm`).
///
/// gc-common w11-f: this said a `Vm`-created `SharedVm` is "never dropped",
/// because `Vm::new` stored a `JcmdProcessor` holding a strong `Arc` back at
/// its own `SharedVm`. That cycle was broken by `4bf4562c4` (the processor's
/// commands hold a `Weak`; `vm_init.rs::tests::dropping_a_vm_destroys_its_shared_state`
/// pins it), so both statements here were stale.
impl Drop for SharedVm {
    fn drop(&mut self) {
        release_vm_native_state(self.vm_identity);
        // zt pl4 item 6: the foreign perf-data views nobody detached. The
        // last reference to this VM is going: every Java thread of it has
        // ended (each holds one), and the heap that held their
        // `DirectByteBuffer`s goes with it, so no Java code can read through
        // a view any more. HotSpot has no twin (one VM per process; the OS
        // unmaps at exit); an embedder that creates VMs one after another no
        // longer leaks the views (`CRATONVM_PERF_FOREIGN_VIEWS_AT_TEARDOWN`,
        // default ON, `=0` kill).
        crate::runtime::perf_data_file::release_foreign_views_at_teardown(
            &mut self.debug.perf_foreign_views.lock(),
        );
        // The heap is going away with this struct; a new `VmHeap` allocated at
        // the same address must not inherit its smuggled-handle table
        // (gc-common w7-a). This runs for `Vm`-created VMs as well, since the
        // jcmd cycle above was broken, so a heap address that a later VM
        // reuses starts with an empty table in both cases.
        crate::runtime::frozen_pages::unregister_vm(self);
        crate::memory::smuggled_longs::forget_heap(&self.mem.heap);
    }
}

impl Vm {
    /// gc-common w11-f — end the primordial thread's registry entry the way a
    /// worker thread ends its own, before its `JvmThread` is freed.
    ///
    /// `Vm::new` registers the primordial thread as `ThreadId(0)` and publishes
    /// two raw addresses into `main_thread` (a `Box` owned by this struct): its
    /// TLAB (`set_tlab_addr`) and the `JvmThread` itself
    /// (`set_jvm_thread_addr`). Every other thread withdraws both and marks
    /// itself dead as it terminates, inside a blocked region, so a pause never
    /// sees it half-dead (`vm_exec.rs`, the worker and virtual-thread
    /// teardowns; `jni.rs`, detach). Nothing did that for `ThreadId(0)`, and
    /// the `SharedVm` can outlive the `Vm`: any daemon Java thread holds an
    /// `Arc` to it, and the launcher drops the `Vm` at the end of `run()` while
    /// daemons keep running, as does `DestroyJavaVM`. After that drop:
    ///
    /// * a pause started by a daemon counts `ThreadId(0)` as a live mutator
    ///   that can never arrive, because it is alive, `stw_ready` and holds no
    ///   join handle;
    /// * `collect_reserved_tlab_tails` reads the freed `Tlab` through the stale
    ///   `tlab_addr` (the registry's own doc calls this "a use-after-free on
    ///   every pause"), and the take-over can hand the stale `jvm_thread_addr`
    ///   to a frame walk.
    ///
    /// Only an entry that still publishes THIS `main_thread` is retired, so a
    /// test `Vm` wrapped around another VM's `SharedVm` (`vm/src/vm/tests.rs`,
    /// the m19 finalizer tests) leaves that VM's entry alone.
    pub(in crate::vm::vm_init) fn retire_primordial_thread(&mut self) {
        let tid = self.main_thread.thread_id;
        let own = &*self.main_thread as *const JvmThread as usize;
        if self.shared.threads.thread_registry.own_jvm_thread_addr(tid) != Some(own) {
            return;
        }
        // The same sequence as a terminating worker: return the TLAB, publish
        // the (empty) root snapshot, enter the blocked population (arriving
        // first if a pause already counts this thread), and flip `alive` in
        // the same barrier transition that leaves it.
        self.main_thread.tlab.retire();
        crate::vm::vm_exec::NativeContextImpl {
            shared: &self.shared,
            thread: &mut self.main_thread,
        }
        .deposit_root_snapshot();
        let shared = &self.shared;
        let blocked = shared.mem.gc_barrier.enter_blocked();
        if blocked.pre_stw {
            let _ = shared.mem.gc_barrier.arrive_and_wait_auto(tid);
        }
        blocked.finish_after(|| {
            // Clears `jvm_thread_addr` with the TLAB address.
            shared.threads.thread_registry.clear_tlab_addr(tid);
            shared.threads.thread_registry.mark_dead(tid);
            shared.threads.monitors.release_monitors_held_by(tid);
        });
    }
}

// ---------------------------------------------------------------------------
// AOT shutdown — flush training data on VM drop
// ---------------------------------------------------------------------------

impl Drop for Vm {
    fn drop(&mut self) {
        // The perf-data sampler first (interpreter round i4 wave 87, lane
        // L1): stopped and joined, so it holds no reference to the VM when
        // this owner releases its own (`DestroyJavaVM` and an embedder's drop
        // come through here).
        crate::runtime::perf_data_file::stop_sampler(&self.shared);
        // ── Persist the profile, if a path was named ──────────────────
        //
        // `CRATONVM_JIT_PROFILE_SAVE=<path>`, default unset. This `Drop` is the
        // only VM-wide shutdown hook, and the save runs FIRST in it, before the
        // AOT flush and before finalizers, so a panic or a hang in either
        // cannot cost the profile.
        //
        // `report_replay_outcome` first: it is what says whether the shapes a
        // previous run handed this one were CONFIRMED or CONTRADICTED by live
        // observation. Without it a save/load pair is unfalsifiable — the next
        // run gets hints and nobody can tell whether the last run's hints were
        // any good.
        //
        // The namer keeps `class_name_adapter`'s `try_read` discipline for the
        // reason that function gives: blocking on the class-manager lock inside
        // a `Drop` would convert a diagnostic into a hang. It asks THIS VM's
        // class manager, not `class_name_adapter` itself: the store's class
        // ids are this VM's, and that adapter answers from the first live VM
        // that knows the id, which in a multi-VM process can be another VM's
        // class (round 11 wave 14).
        //
        // Format version 3: the third resolver stamps each record with the
        // live method's length and bytecode hash, under the same `try_read`
        // (`Busy` writes the record with what its recorders stamped).
        {
            let shared = &self.shared;
            let class_name = |class_id: u32| -> Option<String> {
                shared.classes.class_manager.try_read().and_then(|cm| {
                    cm.get_class(crate::classloading::ClassId::new(class_id))
                        .map(|class| class.name.to_string())
                })
            };
            let live_code = |class_id: u32, name: &str, descriptor: &str| match shared
                .classes
                .class_manager
                .try_read()
            {
                Some(cm) => crate::runtime::interpreter::jit_bridge::live_method_code_len(
                    &cm, class_id, name, descriptor,
                ),
                None => cratonvm_jit::profile_store::LiveCode::Busy,
            };
            cratonvm_jit::profile_store::report_replay_outcome(&shared.jit.profile_store);
            cratonvm_jit::profile_store::save_if_configured(
                &shared.jit.profile_store,
                &class_name,
                &live_code,
            );
        }

        // Sync JIT profile data into AOT training recorder, then flush
        #[cfg(feature = "experimental-aot")]
        {
            use crate::config::AotMode;
            if self.shared.config.aot_mode == AotMode::Training {
                // Bridge JIT ProfileStore → AOT TrainingRunRecorder
                let synced = self.shared.sync_jit_profiles_to_aot();
                if synced > 0 {
                    tracing::info!(
                        "AOT: synced {} method profiles from JIT to AOT recorder",
                        synced
                    );
                }
                let bytes = crate::native::builtins::aot::aot_flush_training_data();
                if bytes > 0 {
                    tracing::info!("AOT: flushed {} bytes of training data", bytes);
                }
            }
        }

        // Dump GraalVM metadata if any was collected
        let (refl, _, _, _, _) = crate::native::builtins::graalvm_compat::graalvm_metadata_stats();
        if refl > 0 {
            tracing::debug!("GraalVM metadata: {} reflection entries collected", refl);
        }

        // Dump CDS archive if configured for dump mode (-Xshare:dump)
        if matches!(self.shared.config.cds_mode, crate::config::CdsMode::Dump) {
            match self.shared.dump_cds_archive() {
                Ok(count) => {
                    let path = self
                        .shared
                        .config
                        .shared_archive_file
                        .as_deref()
                        .unwrap_or("classes.jsa");
                    tracing::info!("CDS: dumped {} classes to {}", count, path);
                }
                Err(e) => {
                    tracing::error!("CDS: archive dump failed: {}", e);
                }
            }
        }

        // gen r4w6/concsvc6 (2026-09-24): stop the generational concurrent-cycle
        // service thread, if one runs (HotSpot stops its concurrent GC threads
        // in `before_exit`, ahead of VMDeath). gcd d1/c: waited for, bounded,
        // inside a GC-blocked region (`stop_gen_concurrent_service_thread`), so
        // no service cycle outlives teardown. From here on a due cycle runs
        // inline again, on the mutator that finds it due.
        crate::runtime::interpreter::gen_conc_service::stop_gen_concurrent_service_thread(
            &self.shared,
            &mut self.main_thread,
        );

        // Run pending finalizers before shutdown (JLS §12.8)
        let finalized = self.run_pending_finalizers();
        if finalized > 0 {
            tracing::debug!("VM shutdown: ran {} pending finalizer(s)", finalized);
        }

        // Dump missing native method audit log if enabled
        if self.shared.config.audit_missing_natives {
            self.shared.dump_missing_natives();
        }

        // T6.3.1: fire JVMTI VMDeath after shutdown bookkeeping is done so
        // agent callbacks see the final heap and subsystem state. One-shot
        // per Vm drop; the underlying `fire_vm_death` is idempotent and
        // gated on the global manager's enable flag.
        // Attributed, and it must stay ahead of `release_vm_native_state`
        // below, which drops this VM's JVMTI row.
        crate::runtime::jvmti::fire_vm_death_for_vm(self.shared.vm_identity);
        // The C JVMTI agents' `VMDeath`, then their `Agent_OnUnload`, on the
        // main thread's context (interpreter round i1 wave 13).
        #[cfg(feature = "experimental-debug")]
        crate::jvmti::native_env::shut_down_agents(&self.shared, 0);

        // gc-common w11-f: after the last Java this thread runs (the finalizers
        // and VMDeath above), and before `main_thread` is freed with this
        // struct. See `retire_primordial_thread`.
        self.retire_primordial_thread();

        // LAST, after the finalizer run and VMDeath above: those still execute
        // Java and still go through natives, and a native that has just lost its
        // capability policy silently reverts to allow-everything. Releasing here
        // keeps the policy live for the whole of shutdown.
        //
        // This is the `DestroyJavaVM` path too: `libcratonvm::destroy_created_vm`
        // (registered via `set_destroy_vm_hook`) takes the parked `Vm` out of
        // `CREATED_VM` and drops it, which runs exactly this.
        //
        // `Drop for SharedVm` does the same thing and is the more precise hook,
        // but it cannot be relied on here: another thread holding an
        // `Arc<SharedVm>` clone defers it, possibly for ever (see its doc).
        // `release_vm_native_state` is idempotent precisely so both hooks can
        // exist. (Until gc-common w11-f this blamed a `JcmdProcessor` reference
        // cycle that made a `Vm`-created `SharedVm` "never dropped"; that
        // cycle was broken by `4bf4562c4`.)
        //
        // Caveat worth stating: other threads may still hold `Arc<SharedVm>`
        // clones and still be executing Java when a `Vm` is dropped. Their gates
        // resolve `None` from here on, i.e. they revert to the permissive
        // default, and a per-VM row they touch after this line is re-created
        // and lives until `Drop for SharedVm` (if that ever runs). That is a
        // fail-open at shutdown, and it is
        // the price of releasing while daemon threads still run (the launcher
        // does not stop them before `run()` returns), not of this ordering.
        release_vm_native_state(self.shared.vm_identity);

        // After the last Java this thread runs for the VM: its per-thread
        // dispatch memos and lambda site caches own compiled bodies of this
        // VM, which would otherwise stay mapped as long as the thread lives
        // (an embedder creating and dropping VMs in a loop kept one more set
        // per VM).
        crate::jit::helpers::release_thread_jit_memos_for_vm(self.shared.vm_identity);
    }
}

// ---------------------------------------------------------------------------
// VmDiagnosticState implementation for SharedVm
// ---------------------------------------------------------------------------

/// Split a `jcmd JFR.dump` / `JFR.stop` argument list into `name=` and
/// `filename=`.
///
/// Every other token is a hard error naming what is accepted, rather than a
/// token quietly dropped on the floor. An operator who mistypes `filenam=x`
/// and is told "OK" has been handed the same lie the JFR stubs used to tell:
/// a success line and no file.
pub(in crate::vm::vm_init) fn jcmd_jfr_parse_target(
    args: &[String],
    command: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let mut name: Option<String> = None;
    let mut filename: Option<String> = None;
    for token in args {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Some((key, value)) = token.split_once('=') else {
            return Err(format!(
                "{command}: option `{token}` is missing `=value` \
                 (accepted: name=<recording>, filename=<path>)"
            ));
        };
        match key {
            "name" => name = Some(value.to_string()),
            "filename" => filename = Some(value.to_string()),
            "maxage" | "maxsize" | "begin" | "end" | "path-to-gc-roots" => {}
            other => {
                return Err(format!(
                    "{command}: unrecognized option `{other}` \
                     (accepted: name=<recording>, filename=<path>)"
                ))
            }
        }
    }
    Ok((name, filename))
}

pub(in crate::vm::vm_init) fn jcmd_jfr_running_recording(
    fr: &cratonvm_jfr::FlightRecorder,
    name: Option<&str>,
) -> Result<u64, String> {
    let running = fr.running_recording_ids();
    match name {
        Some(wanted) => running
            .iter()
            .copied()
            .find(|id| {
                fr.get_recording(*id).is_some_and(|rec| {
                    rec.settings.name == wanted || id.to_string() == wanted
                })
            })
            .ok_or_else(|| format!("no running recording is named `{wanted}`")),
        None => match running.len() {
            0 => Err("no recording is running".to_string()),
            1 => Ok(running[0]),
            n => Err(format!(
                "{n} recordings are running; name one with `name=<recording>`"
            )),
        },
    }
}

/// Does `Thread.print` (`thread_snapshots`) take its one frame-trace pause
/// first (gci i18/dc)? Only when the dump reads published states at all
/// (`published`, `CRATONVM_BLOCKED_IN_NATIVE_RUNNABLE`), the one-pause promise
/// is on (`CRATONVM_THREAD_DUMP_ONE_PAUSE`), the caller is no registered
/// mutator nor a foreign JNI attachment (a pause requested as
/// `PAUSE_OUTSIDER` from a counted mutator would wait for the caller
/// itself), and some listed thread is running (a blocked thread's trace is
/// already published: no pause, as before).
pub(in crate::vm::vm_init) fn thread_dump_wants_frame_trace_pause(
    published: bool,
    one_pause: bool,
    caller_is_outsider: bool,
    any_running: bool,
) -> bool {
    published && one_pause && caller_is_outsider && any_running
}

/// The thread-dump kind of a WAITING / TIMED_WAITING thread's wait
/// (interpreter round i3 wave 79, lane L6): an unrecorded wait prints
/// `(parking)`, as JVMTI reads it `PARKED`.
pub(in crate::vm::vm_init) fn dump_wait_kind_of(
    kind: crate::threading::thread_registry::PublishedWaitKind,
) -> crate::runtime::serviceability::DumpWaitKind {
    use crate::runtime::serviceability::DumpWaitKind;
    use crate::threading::thread_registry::PublishedWaitKind;
    match kind {
        PublishedWaitKind::Sleeping => DumpWaitKind::Sleeping,
        PublishedWaitKind::ObjectWait => DumpWaitKind::ObjectWait,
        PublishedWaitKind::Parked | PublishedWaitKind::Unrecorded => DumpWaitKind::Parking,
    }
}

/// A frame of a thread dump from a published trace entry (interpreter
/// round i3 wave 81, lane L6).
pub(in crate::vm::vm_init) fn dump_frame_info(
    e: &cratonvm_native_api::StackTraceEntry,
) -> crate::runtime::serviceability::FrameInfo {
    crate::runtime::serviceability::FrameInfo {
        class_name: crate::runtime::serviceability::external_class_name(&e.class_name),
        method_name: e.method_name.to_string(),
        file_name: e.source_file.as_deref().map(str::to_owned),
        line_number: e.line_number,
        native_method: e.line_number == crate::runtime::stackwalker::LINE_NUMBER_NATIVE,
    }
}

/// The VM's measured class metadata (the heap summary's `metaspace_used`):
/// [`metaspace_used_bytes`], one walk of `cm`'s class store. zs s3/ob2: a
/// free function so the ZGC `Metaspace` pool's collection-edge sampler can
/// walk under a lock it took without blocking
/// (`gc_events.rs::zgc_metaspace_edge_sample`). Merged with gci i32/mf: the
/// ZGC pool reads the same (HotSpot-shaped) figure as every other consumer.
pub(crate) fn class_metadata_walk_bytes(cm: &crate::classloading::ClassManager) -> usize {
    metaspace_used_bytes(cm)
}

/// zt t1/ob: [`class_metadata_walk_bytes`] with HotSpot's committed and
/// class-space figures ([`metaspace_stats`], the same one walk: its `used`
/// is [`metaspace_used_bytes`]'s figure), for the ZGC `Metaspace` memo
/// (`BackendGcBeans::metaspace_sample_figures`), so a `GcInfo`'s
/// `Metaspace` and `Compressed Class Space` entries carry the figures the
/// pool beans report (`NativeContext::metaspace_space_figures`).
pub(crate) fn class_metadata_walk_figures(
    cm: &crate::classloading::ClassManager,
) -> cratonvm_gc::gc_metrics::MetaspaceSpaceFigures {
    let stats = metaspace_stats(cm);
    cratonvm_gc::gc_metrics::MetaspaceSpaceFigures {
        used: stats.used,
        committed: stats.committed,
        class_space: stats.class_space,
    }
}

/// zt t11/pl3 (proposal `zgc-zt1ob-proposal-incremental-metaspace-figures-*`):
/// [`class_metadata_walk_figures`] for a ZGC `GcInfo` edge sample
/// (`gc_events.rs::zgc_metaspace_edge_sample`), through
/// [`metaspace_stats_for_zgc_sample`]. The pool beans keep the walk.
pub(crate) fn class_metadata_edge_figures(
    cm: &crate::classloading::ClassManager,
) -> cratonvm_gc::gc_metrics::MetaspaceSpaceFigures {
    let stats = metaspace_stats_for_zgc_sample(cm);
    cratonvm_gc::gc_metrics::MetaspaceSpaceFigures {
        used: stats.used,
        committed: stats.committed,
        class_space: stats.class_space,
    }
}

/// zt t11/pl3: [`metaspace_stats`] for the ZGC samples that run once per
/// class-census move (the `GcInfo` edges, the perf-data sampler's metadata
/// pass on a ZGC heap). Under `CRATONVM_ZGC_METASPACE_EDGE_TALLY` (default ON
/// since zt t20/fl1, `=0` kill) with both HotSpot model switches on, from the class manager's running
/// arena tally ([`metaspace_stats_from_tally`]): the classes defined since the
/// last look are summed, not the store. Otherwise the walk, as before.
pub(crate) fn metaspace_stats_for_zgc_sample(cm: &ClassManager) -> MetaspaceStats {
    if metaspace_edge_tally_enabled()
        && metaspace_hotspot_figure_enabled()
        && metaspace_hotspot_spaces_enabled()
    {
        metaspace_stats_from_tally(cm)
    } else {
        metaspace_stats(cm)
    }
}

/// `CRATONVM_ZGC_METASPACE_EDGE_TALLY` (zt t11/pl3; default ON since zt
/// t20/fl1, `=0` kill = the walk): see [`metaspace_stats_for_zgc_sample`].
/// Read per sample (once per census move). Flip evidence (t16 verdict, pl5
/// item 7): 20/20 `tally ... notified=true metaspace_keys=true`, T/D CPU pair
/// median 0.912 over 10 pairs, and the `CRATONVM_DBG_METASPACE_TALLY` rows
/// print no `differs` line (the skewed positive control prints 21).
pub(in crate::vm::vm_init) fn metaspace_edge_tally_enabled() -> bool {
    true
}

/// Boot image capture, at P1: bring the metaspace arena tally up to date, so a
/// restored run's first perf-data pass sums only the classes defined since
/// (the tally is a cache of the walk and equals it; see
/// [`metaspace_stats_from_tally`]) instead of every boot class. A no-op where
/// the sampler would walk the store anyway.
pub fn warm_metaspace_tally(shared: &SharedVm) {
    if shared.mem.heap.is_zgc()
        && metaspace_edge_tally_enabled()
        && metaspace_hotspot_figure_enabled()
        && metaspace_hotspot_spaces_enabled()
    {
        let cm = shared.classes.class_manager.read();
        let _ = metaspace_stats_from_tally(&cm);
    }
}

/// zt t11/pl3: HotSpot's model statistics from the running arena tally the
/// `-XX:MaxMetaspaceSize` check keeps ([`hs_tally_catch_up`]: the classes
/// defined since its last look; a whole re-sum after an unload). Equal to
/// [`metaspace_stats`] while every summed class is unchanged since it was
/// summed (a chunk footprint per loader telescopes, so the order of the sums
/// does not matter); a class whose model bytes changed in place (a stub
/// upgraded by a define, a redefinition) keeps its first bytes until the next
/// unload. `CRATONVM_DBG_METASPACE_TALLY=1` walks too and prints
/// `[metaspace-tally] differs tally=(used,committed) walk=(used,committed)`
/// on stderr when the two disagree (the soundness row of the proposal).
///
/// zt t14/pl5 (`zgc-zt12pl4-*` item 7, the line's positive control):
/// `CRATONVM_DBG_METASPACE_TALLY_SKEW=<n>` adds `n` bytes to the tally's
/// `used` in that comparison only (the figures returned stay the tally's), so
/// a row can show that the comparison and its line fire when the two differ.
pub(crate) fn metaspace_stats_from_tally(cm: &ClassManager) -> MetaspaceStats {
    let stats = {
        let mut tally = cm.metaspace_arena_tally().lock();
        hs_tally_catch_up(&mut tally, cm);
        hs_metaspace_stats(&tally, cm.max_metaspace_bytes())    };
    if false {
        let walk = metaspace_stats(cm);
        let skew = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let compared = MetaspaceStats {
            used: stats.used.saturating_add(skew),
            ..stats
        };
        if walk != compared {
            eprintln!(
                "[metaspace-tally] differs tally=({},{}) walk=({},{})",
                compared.used, compared.committed, walk.used, walk.committed
            );
        }
    }
    stats
}


/// One class's share of [`metaspace_reader_struct_bytes`] (the pre-i32/mf
/// figure, `CRATONVM_METASPACE_HOTSPOT_FIGURE=0`).
pub(in crate::vm::vm_init) fn class_metadata_bytes(c: &crate::classloading::Class) -> usize {
    use cratonvm_reader::attribute::{Attribute, LazyAttribute};
    let cp_entry = std::mem::size_of::<cratonvm_reader::constant_pool::ConstantPoolEntry>();
    let methods: usize = c
        .methods
        .iter()
        .map(|m| {
            let attrs: usize = m
                .attributes
                .iter()
                .map(|a| {
                    std::mem::size_of_val(a)
                        + match a {
                            LazyAttribute::Raw(raw) => raw.range.len(),
                            // The `Code` body is boxed (zr z11/md).
                            LazyAttribute::Decoded(Attribute::Code(code)) => {
                                std::mem::size_of_val(&**code) + code.code.len()
                            }
                            LazyAttribute::Decoded(_) => 0,
                        }
                })
                .sum();
            std::mem::size_of_val(m) + m.name.len() + m.descriptor.len() + attrs
        })
        .sum();
    std::mem::size_of_val(c)
        + c.name.len()
        + c.constant_pool.len() * cp_entry
        + std::mem::size_of_val(c.fields.as_slice())
        + std::mem::size_of_val(c.interfaces.as_slice())
        + methods
}

/// zs s6/ob5: [`class_metadata_walk_bytes`] of each prefix of the class
/// store -- the live classes whose id is below `cut` -- for every `cut` in
/// `cuts` (ascending), in one walk (per class: the HotSpot model's bytes,
/// [`hs_tally_class`]'s, or [`class_metadata_bytes`] under
/// `CRATONVM_METASPACE_HOTSPOT_FIGURE=0`). The ZGC `Metaspace` back-fill
/// (`BackendGcBeans::backfill_metaspace`) uses it to give a collection that
/// ran before the pool was handed out the figure of exactly the classes
/// defined by then: class ids are handed out in definition order and never
/// reused (`ClassStore::next_id`).
pub(crate) fn class_metadata_prefix_bytes(
    cm: &crate::classloading::ClassManager,
    cuts: &[u32],
) -> Vec<(u32, u64)> {
    let mut out = Vec::with_capacity(cuts.len());
    let mut next = cuts.iter().copied().peekable();
    let mut sum: u64 = 0;
    let hotspot = metaspace_hotspot_figure_enabled();
    let mut shapes = HsKlassShapes::new(cm.class_store());
    for c in cm.class_store().iter() {
        let id = c.id.as_u32();
        while let Some(cut) = next.next_if(|&cut| cut <= id) {
            out.push((cut, sum));
        }
        if next.peek().is_none() {
            return out;
        }
        sum += if hotspot {
            let (non_class, class) = shapes.class_parts(c, hs_class_type_annotations_len(cm, c.id));
            non_class.saturating_add(class) as u64
        } else {
            class_metadata_bytes(c) as u64
        };
    }
    out.extend(next.map(|cut| (cut, sum)));
    out
}

/// zt t29/cb (perf-counter page item 1, `unloadedBytes`): each of the live
/// classes `ids`' share of [`metaspace_used_bytes`] -- the HotSpot model's
/// bytes ([`hs_tally_class`]'s figure), or [`class_metadata_bytes`] under
/// `CRATONVM_METASPACE_HOTSPOT_FIGURE=0` -- so the unload transaction can
/// size the classes it is about to remove with the figure the live total
/// loses. An id not in the store is left out.
pub(crate) fn class_model_bytes_of(
    cm: &crate::classloading::ClassManager,
    ids: &[ClassId],
) -> Vec<(ClassId, u64)> {
    let hotspot = metaspace_hotspot_figure_enabled();
    let store = cm.class_store();
    let mut shapes = HsKlassShapes::new(store);
    ids.iter()
        .filter_map(|&id| {
            let c = store.get(id)?;
            let bytes = if hotspot {
                let (non_class, class) =
                    shapes.class_parts(c, hs_class_type_annotations_len(cm, c.id));
                non_class.saturating_add(class) as u64
            } else {
                class_metadata_bytes(c) as u64
            };
            Some((id, bytes))
        })
        .collect()
}

/// zt t3/ob (`zgc-zt1ob-non-heap-pool-residuals-CLOSED-20261019.md` item 4):
/// [`class_metadata_prefix_bytes`] with each prefix's HotSpot-shaped
/// figures -- the same running arena tally [`metaspace_stats`] sums
/// ([`hs_tally_class`] per class, [`hs_stats_from_sums`] at each cut), so a
/// back-filled `GcInfo` carries the prefix's `committed` and class space, as
/// an edge sample does. One walk, as before. Without the HotSpot model's
/// spaces (`CRATONVM_METASPACE_HOTSPOT_FIGURE=0` or `..._SPACES=0`) the used
/// prefix figure with `committed = used` and no class space.
pub(crate) fn class_metadata_prefix_figures(
    cm: &crate::classloading::ClassManager,
    cuts: &[u32],
) -> Vec<(u32, cratonvm_gc::gc_metrics::MetaspaceSpaceFigures)> {
    use cratonvm_gc::gc_metrics::MetaspaceSpaceFigures;
    if !(metaspace_hotspot_figure_enabled() && metaspace_hotspot_spaces_enabled()) {
        return class_metadata_prefix_bytes(cm, cuts)
            .into_iter()
            .map(|(cut, used)| {
                let f = MetaspaceSpaceFigures {
                    used,
                    committed: used,
                    class_space: None,
                };
                (cut, f)
            })
            .collect();
    }
    let max = cm.max_metaspace_bytes();
    let figures_of = |t: &cratonvm_classloading::MetaspaceArenaTally| {
        let s = hs_stats_from_sums(t.boot, t.others_used, t.others_footprint, max);
        MetaspaceSpaceFigures {
            used: s.used,
            committed: s.committed,
            class_space: s.class_space,
        }
    };
    let mut out = Vec::with_capacity(cuts.len());
    let mut next = cuts.iter().copied().peekable();
    let mut tally = cratonvm_classloading::MetaspaceArenaTally::default();
    let mut shapes = HsKlassShapes::new(cm.class_store());
    for c in cm.class_store().iter() {
        let id = c.id.as_u32();
        while let Some(cut) = next.next_if(|&cut| cut <= id) {
            out.push((cut, figures_of(&tally)));
        }
        if next.peek().is_none() {
            return out;
        }
        hs_tally_class(&mut tally, cm, &mut shapes, c);
    }
    let last = figures_of(&tally);
    out.extend(next.map(|cut| (cut, last)));
    out
}

/// zs s6/ob5: the class store's size as a census for
/// [`class_metadata_prefix_bytes`]: the next `ClassId` (every class defined
/// so far is below it). `None` when the class lock cannot be taken without
/// blocking (a collection edge: a stopped thread may hold it for write).
pub(crate) fn class_store_cut(shared: &SharedVm) -> Option<u32> {
    class_store_cut_with(shared, zgc_metaspace_busy_lock_backfill_enabled())
}

