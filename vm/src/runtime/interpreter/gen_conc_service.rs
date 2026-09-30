// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The generational concurrent-cycle SERVICE thread: the VM half of
//! `cratonvm_gc::ConcurrentGcState::serve` (gen r4w6/concsvc6, 2026-09-24).
//!
//! Opt-in, `CRATONVM_GEN_CONC_SERVICE_THREAD` (`CRATONVM_GC=gen-conc-service-thread`),
//! Generational backend only. Closes the VM half of
//! `docs/known-issues/gc/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md`.
//!
//! # What the thread is
//!
//! HotSpot's `G1ConcurrentMarkThread`, in this VM's thread model: a daemon,
//! registry-registered thread with its own `JvmThread`, modelled on the
//! reference-delivery thread (`gc_and_alloc.rs`, `finalizer_thread_main`):
//!
//! * **Idle** it is GC-blocked (root snapshot deposited, TLAB retired, counted
//!   in `threads_blocked`), parked on the heap's per-VM service condvar
//!   (`ConcurrentGcState::service_wait`), and holds only a `Weak<SharedVm>`.
//!   No pause ever waits for it.
//! * **Running** a cycle it is a counted mutator: it leaves the blocked region
//!   (waiting out a pause in progress), and runs the VM's existing driver,
//!   `maybe_concurrent_gc_at`. That driver requests the initial-mark and remark
//!   pauses itself (`request_non_collection_pause` names this thread as the
//!   initiator; `stw_take_over_and_wait` stops and scans the mutators, freezing
//!   the ones in compiled code), polls `safepoint_check` between Phase-2 and
//!   sweep slices so another thread's pause gets this thread's arrival within
//!   one slice, and returns when the cycle completed, was abandoned, or did
//!   not open. So the phases advance on the service's schedule; mutators only
//!   meet the two pauses.
//!
//! What wakes it, the periodic check, the back-off and the fail-safe detach
//! are the collector's (`ConcurrentGcState::serve`); this file supplies the
//! four `ConcurrentServiceHooks` only the VM can implement.
//!
//! # Lifecycle
//!
//! * **Start**: `Vm::new`, once the main thread is registered
//!   ([`start_gen_concurrent_service_thread`]). The thread registers STARTING
//!   (not counted by any pause) and becomes STW-visible itself, exactly as a
//!   `Thread.start()` worker does.
//! * **Stop**: `Drop for Vm` calls [`stop_gen_concurrent_service_thread`]
//!   (gcd d1/c; before it, `ConcurrentGcState::shutdown_service` alone). The
//!   service returns from `serve` at its next wait (at once if idle; a
//!   running cycle is abandoned at its next Phase-2 slice,
//!   `ConcurrentGcState::service_shutting_down`), then leaves the blocked
//!   population and is marked dead in one barrier-serialized step, retiring
//!   its TLAB and withdrawing its published TLAB address first, like any
//!   exiting thread. The dropping thread waits for that, BOUNDED
//!   (`SERVICE_STOP_WAIT`), inside a GC-blocked region: it is a counted
//!   mutator that does not poll, so a plain wait for a service requesting a
//!   pause would deadlock (HotSpot stops its concurrent threads at a
//!   safepoint; this VM has no such point in `Drop`). `System.exit` and a
//!   normal process exit end the (daemon) thread with the process.
//!   (gcd d5/r: this bullet still said "the stop does not wait".)
//!
//! # Per VM
//!
//! Nothing here is a process global: the thread's registration and wake state
//! live on the heap's `ConcurrentGcState` (one per VM), its identity in the
//! VM's thread registry, and its only handle on the VM is a `Weak`.

use std::sync::{Arc, Weak};

use cratonvm_gc::concurrent_mark::{ConcurrentServiceHooks, GEN_CONC_SERVICE_PERIOD_MS};
use cratonvm_gc::g1::MarkDoor;
use cratonvm_gc::ConcurrentGcState;
use cratonvm_native_api::NativeThreadAccess;

use super::gc_and_alloc::{apply_pointer_map_to_thread, maybe_concurrent_gc_at, safepoint_check};
use crate::threading::jvm_thread::JvmThread;
use crate::vm::SharedVm;

/// The service thread's OS and registry name (shown in crash reports and the
/// watchdog's thread summary). HotSpot's G1 names its equivalent
/// `G1 Main Marker`.
pub(crate) const GEN_CONC_SERVICE_THREAD_NAME: &str = "Craton Gen Concurrent Mark";

/// Start this VM's generational concurrent-cycle service thread, if the heap
/// wants one (`VmHeap::wants_concurrent_service_thread`: the Generational
/// backend with `CRATONVM_GEN_CONC_SERVICE_THREAD` set). `true` when a thread
/// was spawned.
///
/// Called once, by `Vm::new`, after the main thread is registered. A
/// `SharedVm` not built by `Vm::new` (unit fixtures) has no owning handle to
/// give a thread and gets no service: its mutators run due cycles inline, as
/// with the flag unset.
pub(crate) fn start_gen_concurrent_service_thread(shared: &SharedVm) -> bool {
    spawn_gen_concurrent_service_thread(shared).is_some()
}

/// [`start_gen_concurrent_service_thread`], returning the registry id of the
/// spawned thread.
fn spawn_gen_concurrent_service_thread(shared: &SharedVm) -> Option<crate::ThreadId> {
    if !shared.mem.heap.wants_concurrent_service_thread() {
        return None;
    }
    let owner = shared.try_get_arc()?;
    let weak = Arc::downgrade(&owner);
    drop(owner);
    let state = Arc::clone(&shared.mem.concurrent_gc_state);
    let registry = &shared.threads.thread_registry;
    let tid = registry.next_thread_id();
    // STARTING, daemon: not counted by any STW census until the thread itself
    // passes `mark_stw_ready`, and never waited for at exit.
    registry.register_starting_with_daemon(tid, GEN_CONC_SERVICE_THREAD_NAME, None, true);
    let spawned = std::thread::Builder::new()
        .name(GEN_CONC_SERVICE_THREAD_NAME.to_string())
        .spawn(move || gen_concurrent_service_main(weak, state, tid));
    match spawned {
        // gen r5w5/conc9: the handle goes to the registry, as a
        // `Thread.start()` worker's does. A service that UNWINDS (a panic in
        // the driver, outside a pause) never reaches `mark_dead`, and without
        // the handle `entry_is_stw_live` kept counting the finished OS thread
        // as a live, running mutator: the next pause's take-over could never
        // stop it (`Take::Gone`) and every mutator stayed parked for good.
        // With it, a finished service is dead to every census
        // (`note_finished_but_alive`). A service that exits normally still
        // marks itself dead first; the handle is then simply detached there.
        Ok(handle) => {
            registry.set_join_handle(tid, handle);
            Some(tid)
        }
        Err(e) => {
            registry.mark_dead(tid);
            tracing::warn!(
                target: "cratonvm::gc::concurrent",
                error = %e,
                "could not start the generational concurrent-cycle service thread; \
                 concurrent cycles keep running inline on the mutator that finds them due"
            );
            None
        }
    }
}

/// How long VM teardown waits for a stopped service to detach
/// ([`stop_gen_concurrent_service_thread`]).
const SERVICE_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// gcd d1/c (2026-09-27) — stop this VM's service thread and wait, BOUNDED,
/// for it to detach, from VM teardown (`Drop for Vm`), with `thread` the
/// dropping thread's own `JvmThread` (the primordial one).
///
/// # Why (`docs/internal/gc/gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown-FIXED-20260928.md`)
///
/// `shutdown_service` alone does not wait: the dropping thread is a counted
/// mutator that does not poll, so waiting for a service that is requesting a
/// pause would deadlock. But not waiting let a service mid-cycle keep
/// requesting its remark and sweeping through the rest of teardown, where
/// every other mutator stalled on pauses the dropping thread reached only at
/// its next poll, and a cycle could still re-create per-VM rows after
/// `release_vm_native_state`. So the wait runs INSIDE a GC-blocked region:
/// the dropping thread is then not counted, the service's pauses proceed
/// without it, and teardown continues once the service has detached (a
/// stopped service abandons its cycle at the next Phase-2 slice and starts no
/// new one; `ConcurrentGcState::service_shutting_down`). The bound keeps a
/// wedged service from hanging exit: past it, teardown goes on as before and
/// says so.
///
/// Stops without waiting (the old behaviour) when no service is attached,
/// when called on the service itself, or when `thread` is not the thread the
/// registry holds for its id (a blocked region needs a registered thread).
pub(crate) fn stop_gen_concurrent_service_thread(shared: &SharedVm, thread: &mut JvmThread) {
    let state = &shared.mem.concurrent_gc_state;
    state.shutdown_service();
    if !state.service_attached() || state.on_service_thread() {
        return;
    }
    let tid = thread.thread_id;
    let own = &*thread as *const JvmThread as usize;
    if shared.threads.thread_registry.own_jvm_thread_addr(tid) != Some(own) {
        return;
    }
    let deadline = std::time::Instant::now() + SERVICE_STOP_WAIT;
    let mut ctx = crate::vm::NativeContextImpl { shared, thread };
    ctx.begin_blocking_region();
    while state.service_attached() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    ctx.end_blocking_region();
    if state.service_attached() {
        tracing::warn!(
            target: "cratonvm::gc::concurrent",
            wait_ms = SERVICE_STOP_WAIT.as_millis() as u64,
            "the generational concurrent-cycle service thread did not stop in time; \
             VM teardown continues without it"
        );
    }
}

/// The four VM callbacks `ConcurrentGcState::serve` needs.
///
/// `running` is `Some` exactly while the thread is a counted mutator: it is
/// the strong handle the running phase holds, dropped when the thread goes
/// idle, so an idle service never keeps the VM alive.
struct ServiceHooks<'t> {
    weak: Weak<SharedVm>,
    state: Arc<ConcurrentGcState>,
    thread: &'t mut JvmThread,
    running: Option<Arc<SharedVm>>,
}

impl ConcurrentServiceHooks for ServiceHooks<'_> {
    fn enter_idle(&mut self) {
        let Some(shared) = self.running.take() else {
            return;
        };
        self.thread.set_vm_state("gen-conc-service:idle");
        // GC-blocked: retires the (empty) TLAB, deposits the (empty) root
        // snapshot, and arrives at a pause already requested, so no pause
        // waits for an idle service.
        let mut ctx = crate::vm::NativeContextImpl {
            shared: &shared,
            thread: &mut *self.thread,
        };
        ctx.begin_blocking_region();
    }

    fn leave_idle(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(shared) = self.weak.upgrade() else {
            // The VM is gone: nothing to run a cycle on. Stop the loop.
            self.state.shutdown_service();
            return;
        };
        {
            // Waits out an in-flight pause and applies what it moved.
            let mut ctx = crate::vm::NativeContextImpl {
                shared: &shared,
                thread: &mut *self.thread,
            };
            ctx.end_blocking_region();
        }
        self.thread.set_vm_state("gen-conc-service:cycle");
        self.running = Some(shared);
    }

    fn cycle_due(&mut self) -> bool {
        // Asked while idle (GC-blocked): takes only heap locks. On this
        // thread the answer is the policy's verdict, never a hand-off.
        match self.weak.upgrade() {
            Some(shared) => shared.mem.heap.concurrent_cycle_due(),
            None => {
                self.state.shutdown_service();
                false
            }
        }
    }

    fn run_cycle(&mut self) {
        let Some(shared) = self.running.clone() else {
            // `leave_idle` found the VM gone and asked the loop to stop.
            return;
        };
        // `MaybeGc`: the door census is G1's; the Generational driver does
        // not record one. The driver re-asks the trigger, so a request that
        // went stale while the service woke opens nothing.
        maybe_concurrent_gc_at(&shared, &mut *self.thread, MarkDoor::MaybeGc);
    }
}

/// Body of the service thread. See the module doc.
fn gen_concurrent_service_main(
    weak: Weak<SharedVm>,
    state: Arc<ConcurrentGcState>,
    tid: crate::ThreadId,
) {
    let Some(shared) = weak.upgrade() else {
        return;
    };
    // Never moved after its addresses are published below: the hooks borrow
    // it in place, and it is dropped only after `mark_dead`.
    let mut thread = JvmThread::new(tid, GEN_CONC_SERVICE_THREAD_NAME);
    thread.kind = crate::threading::ThreadKind::Platform;
    thread.daemon = true;
    {
        let registry = &shared.threads.thread_registry;
        registry.set_interrupted_flag(tid, thread.interrupted.clone());
        registry.set_park_state(tid, thread.park_state.clone());
        registry.set_root_snapshot(tid, thread.root_snapshot.clone());
        registry.set_frame_trace(tid, thread.frame_trace.clone());
        registry.set_vm_state(tid, thread.vm_state.clone());
        registry.set_gc_block_state(tid, thread.gc_block_state.clone());
        registry.set_tlab_addr(tid, &thread.tlab as *const cratonvm_gc::Tlab as usize);
        registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
        registry.set_os_tid_current(tid);
    }
    cratonvm_classloading::set_current_thread_id(shared.classes.class_layout_domain, tid.0);
    let _crash_frames = crate::vm::PublishedFrameTrace::publish(
        GEN_CONC_SERVICE_THREAD_NAME,
        tid.0,
        thread.frame_trace.clone(),
    );
    thread.set_vm_state("gen-conc-service:registered");
    // Become STW-visible exactly as a `Thread.start()` worker does.
    loop {
        let ready = shared.mem.gc_barrier.run_if_no_stw_requested(|| {
            shared.threads.thread_registry.mark_stw_ready(tid);
        });
        if ready {
            break;
        }
        let pointer_map = shared.mem.gc_barrier.arrive_and_wait_excluded(tid);
        if !pointer_map.is_empty() {
            apply_pointer_map_to_thread(
                &mut thread,
                &pointer_map,
                &shared.mem.heap,
                &shared.classes.type_maps,
            );
        }
    }
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(&shared, &mut thread);
    }

    // The loop. It starts running (a counted mutator); `serve` puts it idle
    // before its first wait, and returns with it idle after a shutdown.
    let mut hooks = ServiceHooks {
        weak: weak.clone(),
        state: Arc::clone(&state),
        thread: &mut thread,
        running: Some(shared),
    };
    let served = state.serve(
        std::time::Duration::from_millis(GEN_CONC_SERVICE_PERIOD_MS),
        &mut hooks,
    );
    if !served {
        // Refused: this VM already has a service, or it was shut down before
        // this thread attached. Still running; go idle so the exit below has
        // one shape.
        tracing::debug!(
            target: "cratonvm::gc::concurrent",
            "generational concurrent-cycle service not attached (one already runs, \
             or the VM is shutting down); the thread exits"
        );
    }
    hooks.enter_idle();
    drop(hooks);

    // Exit from the idle (GC-blocked) state: leave the blocked population and
    // become dead in ONE barrier-serialized step, so no pause can count this
    // thread in between (the terminal transition of `thread_start`'s workers
    // and the AIO dispatcher). The TLAB is retired and its published address
    // withdrawn before the `JvmThread` is dropped.
    let Some(shared) = weak.upgrade() else {
        // The VM, and its registry, are gone already.
        return;
    };
    thread.set_vm_state("gen-conc-service:exited");
    shared.mem.gc_barrier.mark_blocked_region_leave_after(|| {
        thread.tlab.retire();
        shared.threads.thread_registry.clear_tlab_addr(tid);
        shared.threads.thread_registry.mark_dead(tid);
    });
    // After the leave, not inside it (round 12 wave 2, lane lock): the sweep
    // walks every inflated monitor, and the closure above holds the GC
    // barrier's transition lock that every blocked-region enter and leave in
    // the VM queues on. As in `thread_start`'s termination; the walk tolerates
    // a concurrent index re-key (`MonitorTable::release_monitors_held_by_except`).
    shared.threads.monitors.release_monitors_held_by(tid);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};
    use crate::vm::Vm;

    /// Poll `cond` every millisecond for up to `secs` seconds.
    fn eventually(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        cond()
    }

    fn generational_vm(service: bool) -> Vm {
        let cfg = VmConfig {
            gc_algorithm: GcAlgorithm::Generational,
            ..VmConfig::default()
        };
        let edit: &[(&str, Option<&str>)] = if service {
            &[("CRATONVM_GEN_CONC_SERVICE_THREAD", Some("1"))]
        } else {
            &[("CRATONVM_GEN_CONC_SERVICE_THREAD", None)]
        };
        cratonvm_types::flags::with_thread_overrides(edit, || Vm::new(cfg))
    }

    /// gen r4w6/concsvc6 — with the flag, `Vm::new` starts the service: it
    /// attaches to the heap's concurrent state, idles GC-blocked (so a pause
    /// requested now does not wait for it), and a mutator's due cycle is
    /// handed to it; `Drop for Vm` stops it, and it detaches and is marked
    /// dead in the registry.
    #[test]
    fn the_service_thread_starts_with_a_generational_vm_and_stops_with_it() {
        let vm = generational_vm(true);
        let shared = Arc::clone(&vm.shared);
        let state = Arc::clone(&shared.mem.concurrent_gc_state);
        assert!(
            eventually(30, || state.service_attached()),
            "the service thread attached"
        );
        assert!(!state.on_service_thread(), "the test thread is a mutator");
        let line = state.driver_census_line();
        assert!(line.contains(" concdrv_service_attached=true "), "{line}");
        // A second service on the same VM is refused and exits cleanly (the
        // not-attached exit path).
        let second = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GEN_CONC_SERVICE_THREAD", Some("1"))],
            || spawn_gen_concurrent_service_thread(&shared),
        )
        .expect("a second thread is spawned");
        assert!(
            eventually(30, || !shared.threads.thread_registry.is_alive(second)),
            "the refused service marked itself dead"
        );
        assert!(state.service_attached(), "the first service is unaffected");

        drop(vm);
        assert!(
            eventually(30, || !state.service_attached()),
            "shutdown detached the service"
        );
        assert!(
            state.attach_service_thread().is_none(),
            "the shutdown is permanent"
        );
    }

    /// gcd d1/c (`gengc-r5w5-conc9-the-service-thread-can-outlive-vm-teardown`):
    /// the teardown stop waits, inside a blocked region, until the service
    /// has detached, and leaves the dropping thread a counted mutator again.
    /// Without a service it returns at once.
    #[test]
    fn gcd_d1c_the_teardown_stop_waits_for_the_service_to_detach() {
        let mut vm = generational_vm(true);
        let state = Arc::clone(&vm.shared.mem.concurrent_gc_state);
        assert!(
            eventually(30, || state.service_attached()),
            "the service thread attached"
        );
        let shared = Arc::clone(&vm.shared);
        let started = std::time::Instant::now();
        stop_gen_concurrent_service_thread(&shared, &mut vm.main_thread);
        assert!(state.service_shutting_down());
        assert!(
            !state.service_attached() || started.elapsed() >= SERVICE_STOP_WAIT,
            "returned before the service detached and before the bound"
        );
        assert!(
            !vm.main_thread
                .gc_block_state
                .in_blocked_region
                .load(std::sync::atomic::Ordering::Acquire),
            "the dropping thread left the blocked region"
        );
        drop(shared);
        drop(vm);

        let plain = generational_vm(false);
        let shared = Arc::clone(&plain.shared);
        let mut plain = plain;
        stop_gen_concurrent_service_thread(&shared, &mut plain.main_thread);
        assert!(!shared.mem.concurrent_gc_state.service_attached());
    }

    /// Without the flag (the default) no service is started and every path
    /// stays inline.
    #[test]
    fn no_service_thread_without_the_flag() {
        let vm = generational_vm(false);
        assert!(!start_gen_concurrent_service_thread(&vm.shared));
        assert!(!vm.shared.mem.concurrent_gc_state.service_attached());
        assert!(vm
            .shared
            .mem
            .concurrent_gc_state
            .driver_census_line()
            .contains(" concdrv_service_attached=false "));
    }
}
