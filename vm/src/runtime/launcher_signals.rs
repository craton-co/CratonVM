// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//! Catchable launcher termination reaches the VM's ordinary exit path.
//! Signal handlers publish only a process event; configuration, thread lifetime
//! and the VM to which it is delivered belong to the launcher guard.
//!
//! POSIX: `SIGHUP`, `SIGINT` and `SIGTERM` handlers. Windows (interpreter
//! round i6 wave 115, lane L1, `CRATONVM_WINDOWS_CONSOLE_SIGNALS`, default
//! ON, `=0` off): a console control handler, HotSpot 25's `consoleHandler`
//! (os_windows.cpp) -- `Ctrl-C` raises `SIGINT`; console close, logoff (of an
//! interactive session) and system shutdown raise `SIGTERM`; `Ctrl-Break`
//! is left to the thread-dump latch (`serviceability::install_ctrl_break_latch`).
//! Before, the console's default action ended the process at `Ctrl-C` with
//! `STATUS_CONTROL_C_EXIT` and no shutdown hooks, where HotSpot runs them and
//! exits 130, or runs a `Signal.handle("INT", ..)` handler.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use crate::vm::SharedVm;

/// The signals the launcher catches, by the numbers both the POSIX headers
/// and the Windows CRT give them (`SIGHUP` is never posted on Windows, which
/// has none), in [`PENDING`]'s order.
#[cfg(any(unix, windows))]
const CAUGHT: [i32; 3] = [1, 2, 15];

/// Handler entries per [`CAUGHT`] signal, not yet delivered (slots 0-2), and
/// the dispatcher's direct-wake object ([`WAKE_SLOT`]). A process static by
/// nature: a signal (a console event) is the process's, and its handler can
/// reach nothing else; the launcher guard delivers it to its VM.
#[cfg(any(unix, windows))]
static PENDING: [AtomicU32; 4] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// [`PENDING`]'s slot for `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE` (interpreter
/// round i6 wave 116, lane L1; default OFF): the published [`DirectWake`]'s
/// raw word (POSIX: the wake pipe's write end + 1; Windows: the wake event's
/// handle, which fits 32 bits), 0 when none. In the counters' array so the
/// handler reaches it without another static.
#[cfg(any(unix, windows))]
const WAKE_SLOT: usize = 3;

/// Count one entry of `signal`'s handler (async-signal-safe: one atomic
/// add, then, with a direct wake published, one `write(2)` / `SetEvent`).
/// Java custom handlers must see each delivered signal even when several
/// arrive before the worker's next poll.
#[cfg(any(unix, windows))]
fn post_signal(signal: i32) {
    if let Some(index) = CAUGHT.iter().position(|&s| s == signal) {
        PENDING[index].fetch_add(1, Ordering::Release);
        DirectWake::raise_published();
    }
}

/// `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE` (interpreter round i6 wave 116, lane
/// L1; performance, default ON since the round-i7 triage, `=0` off): the dispatcher waits on an OS object the
/// handler raises -- a non-blocking self-pipe on POSIX (`write(2)` is
/// async-signal-safe), an auto-reset event on Windows (the console handler
/// runs on an ordinary thread) -- instead of waking every 20 ms to look at
/// the counters. HotSpot's Signal Dispatcher waits on a semaphore the
/// handler posts, so a caught signal reaches Java at once and an idle VM
/// takes no periodic wake-ups; the `=0` arm polls (up to 20 ms late,
/// 50 wake-ups a second for the whole run). The dispatcher still re-reads
/// the counters every second, so a lost raise only costs that.
///
/// The object is never closed (one pipe or event per launcher run, which
/// runs once per process): a handler that read the slot just before the
/// guard withdrew it must not write into a descriptor the process has since
/// reused.
#[cfg(any(unix, windows))]
struct DirectWake {
    /// What [`WAKE_SLOT`] holds while published.
    word: u32,
    /// POSIX: the pipe's read end.
    #[cfg(unix)]
    read_fd: libc::c_int,
}

#[cfg(windows)]
mod direct_wake_ffi {
    #[link(name = "Kernel32")]
    extern "system" {
        pub fn CreateEventW(
            attributes: *mut std::ffi::c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> isize;
        pub fn SetEvent(event: isize) -> i32;
        pub fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
    }
}

#[cfg(any(unix, windows))]
impl DirectWake {
    /// Make the object, or `None` (the dispatcher then polls).
    #[cfg(unix)]
    fn create() -> Option<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a live two-element array; the descriptors are
        // this process's own and only flagged here.
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return None;
            }
            for fd in fds {
                let fl = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        let word = u32::try_from(fds[1]).ok()?.checked_add(1)?;
        Some(Self {
            word,
            read_fd: fds[0],
        })
    }

    #[cfg(windows)]
    fn create() -> Option<Self> {
        // SAFETY: an unnamed auto-reset event, not signalled.
        let event = unsafe {
            direct_wake_ffi::CreateEventW(std::ptr::null_mut(), 0, 0, std::ptr::null())
        };
        // Cast: a kernel handle's significant bits are its low 32 (Windows'
        // 32/64-bit handle interoperability rule); 0 is the failure value.
        let word = event as u32;
        if event == 0 || word as i32 as isize != event {
            return None;
        }
        Some(Self { word })
    }

    /// Make the object reachable by the handler.
    fn publish(&self) {
        PENDING[WAKE_SLOT].store(self.word, Ordering::Release);
    }

    /// No handler raises it any more (the object stays open, see the type).
    fn withdraw(&self) {
        let _ = PENDING[WAKE_SLOT].compare_exchange(
            self.word,
            0,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }

    /// Raise the published object, if any (async-signal-safe).
    fn raise_published() {
        let word = PENDING[WAKE_SLOT].load(Ordering::Acquire);
        if word == 0 {
            return;
        }
        #[cfg(unix)]
        {
            // Cast: the write end, stored + 1 (non-zero).
            let fd = (word - 1) as libc::c_int;
            let byte = 1u8;
            // Interpreter round i7 wave 135 (lane L1b): a failed `write` (a
            // full pipe's EAGAIN) sets `errno` on the thread the signal
            // interrupted, which may be between a libc call and its errno
            // check; HotSpot's handlers save and restore it
            // (`ErrnoPreserver`). The slot is per-thread, read and written
            // without allocation (async-signal-safe).
            let slot = errno_slot();
            // SAFETY: the calling thread's own errno slot, when one is known.
            let saved = slot.map(|slot| unsafe { *slot });
            // SAFETY: one byte from a live local; the descriptor is never
            // closed (see the type). A full pipe (EAGAIN) already wakes.
            unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
            if let (Some(slot), Some(saved)) = (slot, saved) {
                // SAFETY: as above.
                unsafe { *slot = saved };
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: the published event handle, never closed.
            unsafe { direct_wake_ffi::SetEvent(word as i32 as isize) };
        }
    }

    /// Wait until raised, or `millis` pass; consume the raises.
    fn wait(&self, millis: u32) {
        #[cfg(unix)]
        {
            let mut poll = libc::pollfd {
                fd: self.read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let mut drain = [0u8; 64];
            // SAFETY: a live one-element `pollfd` array; reads into a live
            // local buffer from this object's own non-blocking read end.
            unsafe {
                libc::poll(&mut poll, 1, millis as libc::c_int);
                while libc::read(self.read_fd, drain.as_mut_ptr().cast(), drain.len()) > 0 {}
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: the object's own event handle.
            unsafe { direct_wake_ffi::WaitForSingleObject(self.word as i32 as isize, millis) };
        }
    }
}

/// The calling thread's `errno` slot, where this target's libc names one
/// (async-signal-safe: a stable per-thread address, no allocation).
#[cfg(unix)]
fn errno_slot() -> Option<*mut libc::c_int> {
    // Interpreter round i7 wave 136 (lane L1 review): the `libc` crate names
    // DragonFly's accessor `__errno_location` (not `__error`, which did not
    // resolve there) and Android's `__errno`.
    #[cfg(any(target_os = "linux", target_os = "dragonfly"))]
    {
        // SAFETY: the libc accessor of the calling thread's errno.
        Some(unsafe { libc::__errno_location() })
    }
    #[cfg(target_os = "android")]
    {
        // SAFETY: the libc accessor of the calling thread's errno.
        Some(unsafe { libc::__errno() })
    }
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        // SAFETY: the libc accessor of the calling thread's errno.
        Some(unsafe { libc::__error() })
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    )))]
    {
        None
    }
}

/// The dispatcher's wait between two looks at the counters.
#[cfg(any(unix, windows))]
fn dispatcher_wait(wake: Option<&DirectWake>) {
    match wake {
        Some(wake) => wake.wait(1_000),
        None => std::thread::park_timeout(std::time::Duration::from_millis(20)),
    }
}

/// Wake the dispatcher for its guard's `drop` (after `stop` is raised).
#[cfg(any(unix, windows))]
fn wake_dispatcher_to_stop(worker: &std::thread::JoinHandle<()>) {
    worker.thread().unpark();
    DirectWake::raise_published();
}

/// The "Launcher Signal Dispatcher" OS thread: every 20 ms (or when
/// unparked; with `CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE`, when a handler
/// raises its [`DirectWake`], or every second) it takes the posted counts
/// and delivers each signal on a "Signal Dispatcher" attached service
/// thread of the VM, until `stop`.
#[cfg(any(unix, windows))]
fn spawn_dispatcher(
    shared: &Arc<SharedVm>,
    stop: &Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    let stop = Arc::clone(stop);
    let weak = Arc::downgrade(shared);
    let direct =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE");
    let wake = if direct { DirectWake::create() } else { None };
    if let Some(wake) = &wake {
        wake.publish();
    }
    let backstop = launcher_signal_exit_backstop_enabled();
    let stall_ms = launcher_signal_stall_ms();
    std::thread::Builder::new()
        .name("Launcher Signal Dispatcher".into())
        .spawn(move || {
            let _withdraw = WithdrawOnExit(wake.as_ref());
            while !stop.load(Ordering::Acquire) {
                for (index, signal) in CAUGHT.into_iter().enumerate() {
                    let events = PENDING[index].swap(0, Ordering::AcqRel);
                    for _ in 0..events {
                        let Some(shared) = weak.upgrade() else {
                            return;
                        };
                        // zt t27/fin1: set once the job runs on the attached
                        // thread; the backstop watches it.
                        let entered = Arc::new(AtomicBool::new(false));
                        if backstop {
                            arm_exit_backstop(shared.vm_identity, signal, &entered);
                        }
                        let (started, _) = std::sync::mpsc::sync_channel(1);
                        let (tx, rx) =
                            std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
                        let _ = tx.send(Box::new(move |shared, thread| {
                            if stall_ms > 0 {
                                // `CRATONVM_DBG_LAUNCHER_SIGNAL_STALL_MS`: the
                                // backstop's positive control -- an attached,
                                // unblocked thread that holds every pause.
                                std::thread::sleep(std::time::Duration::from_millis(stall_ms));
                            }
                            entered.store(true, Ordering::Release);
                            deliver_host_signal(shared, thread, signal);
                        }));
                        drop(tx);
                        crate::native::jni::run_attached_service(
                            shared,
                            "Signal Dispatcher",
                            started,
                            rx,
                        );
                    }
                }
                dispatcher_wait(wake.as_ref());
            }
        })
        .ok()
}

/// Withdraws the dispatcher's [`DirectWake`] when the dispatcher thread
/// ends, whichever way it leaves its loop.
#[cfg(any(unix, windows))]
struct WithdrawOnExit<'a>(Option<&'a DirectWake>);

#[cfg(any(unix, windows))]
impl Drop for WithdrawOnExit<'_> {
    fn drop(&mut self) {
        if let Some(wake) = self.0 {
            wake.withdraw();
        }
    }
}

/// One caught host signal, on the "Signal Dispatcher" service thread.
///
/// gci i17/na: this job is not under `safe_native_call_impl`, so a
/// Rust-side allocation the dispatch reaches took the allocators'
/// infallible arm and aborted the process on an exhausted heap. Under
/// `native_oom::catch_alloc_oom_on` it is refused instead; no Java frame
/// waits for the dispatch, so the refusal is logged with its other
/// failures. Java run by the dispatch keeps its own `OutOfMemoryError`.
#[cfg(any(unix, windows))]
fn deliver_host_signal(shared: &SharedVm, thread: &mut crate::threading::JvmThread, signal: i32) {
    use cratonvm_native_api::NativeInvokeAccess;
    let delivered = crate::runtime::native_oom::catch_alloc_oom_on(thread, |thread| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        use cratonvm_native_builtins::deprecated_internal::{
            dispatch_host_signal, host_signal_disposition, HostSignalDisposition,
        };
        match host_signal_disposition(&ctx, signal) {
            HostSignalDisposition::Ignore => {}
            HostSignalDisposition::Java => {
                dispatch_host_signal(&mut ctx, signal);
            }
            HostSignalDisposition::Default => os_default_action(signal),
            HostSignalDisposition::Shutdown => {
                // Same hooks, halt and pre-exit cleanup as System.exit.
                if let Err(error) = ctx.invoke(
                    "java/lang/Shutdown",
                    "exit",
                    "(I)V",
                    &[cratonvm_types::Value::Int(128 + signal)],
                ) {
                    tracing::error!("signal shutdown failed: {error}");
                }
            }
        }
    });
    if let Err(oom) = delivered {
        tracing::error!(
            "signal {signal} dispatch failed: java.lang.OutOfMemoryError: {}",
            oom.java_message()
        );
    }
}

/// An explicit Java `SIG_DFL` requests the OS action, including no hooks
/// and a stale perf file. POSIX: replay only this caught signal after
/// restoring `SIG_DFL`.
#[cfg(unix)]
fn os_default_action(signal: i32) {
    // SAFETY: a zeroed `sigaction` with `SIG_DFL` and an empty mask, then a
    // signal to this process; no memory is shared with the handler.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        libc::kill(libc::getpid(), signal);
    }
}

/// Windows: HotSpot's `consoleHandler` `raise`s the signal into the CRT,
/// whose `SIG_DFL` action for `SIGINT` / `SIGTERM` is `_exit(3)`.
#[cfg(windows)]
fn os_default_action(_signal: i32) {
    #[link(name = "Kernel32")]
    extern "system" {
        fn ExitProcess(code: u32) -> !;
    }
    // SAFETY: ends the process, as the CRT's default action does.
    unsafe { ExitProcess(3) }
}

/// `CRATONVM_LAUNCHER_SIGNAL_EXIT_BACKSTOP` (zt t27/fin1, page
/// `zgc-zt27fin1-finalizable-churn-and-signals-CLOSED-20261022.md` item 2; default
/// ON, `=0` kill). HotSpot 25 answers `SIGTERM` / `SIGINT` / `SIGHUP` on its
/// Signal Dispatcher within milliseconds -- the `Terminator` handler's
/// `Shutdown.exit(128 + signal)` runs the hooks and halts (143 / 130 / 129)
/// -- whatever the other threads are doing, a `System.gc()` or a finalizer
/// wait included. Here the delivery is a job on an attached "Signal
/// Dispatcher" thread, and an attached thread runs Java only once it has
/// left its GC-blocked region: a VM wedged in a pause that never ends (or on
/// a lock the attach needs) never runs the job, and the process ignored the
/// signal until `SIGKILL` (row `x14_N1fc_01`, `--compatible`, 600 s). With
/// the switch a backstop thread, armed for each caught signal, waits
/// [`EXIT_BACKSTOP_GRACE`] for the job to START; a job that started is
/// HotSpot's path and is never second-guessed (shutdown hooks may take as
/// long as they like, as on HotSpot). A job that never started finds the VM
/// unable to run Java, so no hook could run either: the backstop ends the
/// process the way the delivery would have -- `128 + signal` for the
/// shutdown and Java dispositions, the OS default action for an explicit
/// `SIG_DFL`, nothing for `SIG_IGN` -- without the VM. `=0`: no backstop.
#[cfg(any(unix, windows))]
fn launcher_signal_exit_backstop_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_LAUNCHER_SIGNAL_EXIT_BACKSTOP")
}

/// `CRATONVM_DBG_LAUNCHER_SIGNAL_STALL_MS=<ms>` (zt t27/fin1, diagnostic,
/// default 0): the signal job sleeps this long on its attached thread before
/// it counts as started -- a stand-in for a wedged VM, the backstop's
/// positive control (rows `fin1_S*`).
#[cfg(any(unix, windows))]
fn launcher_signal_stall_ms() -> u64 {
    cratonvm_types::flags::runtime_var("CRATONVM_DBG_LAUNCHER_SIGNAL_STALL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// How long [`arm_exit_backstop`] waits for the signal job to start.
#[cfg(any(unix, windows))]
const EXIT_BACKSTOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How often the backstop looks at the job's `entered` flag.
#[cfg(any(unix, windows))]
const EXIT_BACKSTOP_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Start the backstop of [`launcher_signal_exit_backstop_enabled`] for one
/// caught `signal`: a plain OS thread (never attached to the VM, so nothing a
/// wedged VM holds can stop it) that returns as soon as `entered` is raised
/// and otherwise, after [`EXIT_BACKSTOP_GRACE`], ends the process
/// ([`fire_exit_backstop`]). A failed spawn leaves the delivery as before.
#[cfg(any(unix, windows))]
fn arm_exit_backstop(vm_identity: usize, signal: i32, entered: &Arc<AtomicBool>) {
    let entered = Arc::clone(entered);
    let spawned = std::thread::Builder::new()
        .name("Launcher Signal Backstop".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            let deadline = std::time::Instant::now() + EXIT_BACKSTOP_GRACE;
            loop {
                if entered.load(Ordering::Acquire) {
                    return;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::sleep((deadline - now).min(EXIT_BACKSTOP_POLL));
            }
            if entered.load(Ordering::Acquire) {
                return;
            }
            fire_exit_backstop(vm_identity, signal);
        });
    if let Err(error) = spawned {
        tracing::debug!("signal {signal}: no exit backstop ({error})");
    }
}

/// The backstop's action for a delivery that never started: what the job
/// would have ended in, without the VM. The disposition table is the VM's
/// own (`Signal.handle0`'s per-VM rows), read without a thread.
#[cfg(any(unix, windows))]
fn fire_exit_backstop(vm_identity: usize, signal: i32) {
    use cratonvm_native_builtins::deprecated_internal::{
        host_signal_disposition_for_vm, HostSignalDisposition,
    };
    match host_signal_disposition_for_vm(vm_identity, signal) {
        HostSignalDisposition::Ignore => {}
        HostSignalDisposition::Default => os_default_action(signal),
        // The JDK's `Terminator` handler (`--jdk-only`) and the launcher's
        // own shutdown both end in `Shutdown.exit(128 + signal)`. A user
        // handler that would not exit is the recorded edge: it cannot run on
        // a VM that cannot start the job either.
        HostSignalDisposition::Java | HostSignalDisposition::Shutdown => {
            exit_without_the_vm(128 + signal)
        }
    }
}

/// End the process at once, running nothing of the VM (no `atexit` work that
/// could wait on a lock a wedged thread holds).
#[cfg(unix)]
fn exit_without_the_vm(code: i32) -> ! {
    // SAFETY: `_exit` ends the process; no memory is touched.
    unsafe { libc::_exit(code) }
}

/// Windows: the process-exit helper the exit natives use (it runs the exit
/// on a fresh thread whose thread-locals hold nothing of the VM).
#[cfg(windows)]
fn exit_without_the_vm(code: i32) -> ! {
    cratonvm_native_api::process_exit::exit_process(code)
}

#[cfg(all(test, any(unix, windows)))]
mod zt27fin1_exit_backstop_tests {
    use super::*;

    /// The backstop is default ON with a `=0` kill; the stall knob is 0 unless
    /// set to a number.
    #[test]
    fn zt27fin1_backstop_default_on_and_killable() {
        const KEY: &str = "CRATONVM_LAUNCHER_SIGNAL_EXIT_BACKSTOP";
        {
            let _unset = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(KEY, None)]),
            );
            assert!(launcher_signal_exit_backstop_enabled());
        }
        {
            let _off = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(KEY, Some("0"))]),
            );
            assert!(!launcher_signal_exit_backstop_enabled());
        }
        const STALL: &str = "CRATONVM_DBG_LAUNCHER_SIGNAL_STALL_MS";
        {
            let _unset = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(STALL, None)]),
            );
            assert_eq!(launcher_signal_stall_ms(), 0);
        }
        let _set = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(STALL, Some("8000"))]),
        );
        assert_eq!(launcher_signal_stall_ms(), 8000);
    }

    /// A job that started in time disarms the backstop: the thread returns
    /// without acting (were it to act, the test process would end).
    #[test]
    fn zt27fin1_a_started_job_disarms_the_backstop() {
        let entered = Arc::new(AtomicBool::new(true));
        // An identity no VM has: the disposition would read `Shutdown`.
        arm_exit_backstop(usize::MAX, 15, &entered);
        std::thread::sleep(EXIT_BACKSTOP_POLL * 2);
    }

    /// Source pin: the job raises `entered` before it delivers, and the
    /// dispatcher arms the backstop before it runs the job.
    #[test]
    fn zt27fin1_the_job_raises_entered_before_delivering() {
        let src = include_str!("launcher_signals.rs").replace("\r\n", "\n");
        let arm = src
            .find(concat!(
                "arm_exit_backstop(shared.vm_",
                "identity, signal, &entered);"
            ))
            .expect("armed");
        let raise = src
            .find(concat!("entered.store(true, ", "Ordering::Release);"))
            .expect("raised");
        let deliver = src
            .find(concat!("deliver_host_signal(shared, ", "thread, signal);"))
            .expect("delivered");
        let run = src
            .find(concat!("crate::native::jni::run_", "attached_service("))
            .expect("run");
        assert!(arm < raise && raise < deliver && deliver < run);
    }
}

#[cfg(unix)]
mod unix {
    use super::SharedVm;
    use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

    extern "C" fn post(signal: libc::c_int) {
        super::post_signal(signal);
    }

    pub struct LauncherSignals {
        old: Vec<(i32, libc::sigaction)>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl LauncherSignals {
        pub fn start(shared: &Arc<SharedVm>) -> Option<Self> {
            if shared.config.reduce_signal_usage {
                return None;
            }
            let mut guard = Self {
                old: Vec::new(),
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
            };
            for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
                // Preserve inherited ignores and embedding/native handlers. All
                // calls and the restoration are on the launcher thread.
                unsafe {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(signal, std::ptr::null(), &mut old) != 0
                        || old.sa_sigaction != libc::SIG_DFL
                    {
                        continue;
                    }
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = post as usize;
                    action.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut action.sa_mask);
                    if libc::sigaction(signal, &action, std::ptr::null_mut()) == 0 {
                        guard.old.push((signal, old));
                    }
                }
            }
            if guard.old.is_empty() {
                return None;
            }
            guard.worker = super::spawn_dispatcher(shared, &guard.stop);
            guard.worker.as_ref()?;
            Some(guard)
        }
    }

    impl Drop for LauncherSignals {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                super::wake_dispatcher_to_stop(&worker);
                let _ = worker.join();
            }
            for (signal, old) in &self.old {
                // Do not displace a handler subsequently installed by native code.
                unsafe {
                    let mut current: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(*signal, std::ptr::null(), &mut current) == 0
                        && current.sa_sigaction == post as usize
                    {
                        libc::sigaction(*signal, old, std::ptr::null_mut());
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::SharedVm;
    use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const UOI_FLAGS: i32 = 1;
    const WSF_VISIBLE: u32 = 1;

    #[repr(C)]
    struct UserObjectFlags {
        f_inherit: i32,
        f_reserved: i32,
        dw_flags: u32,
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    #[link(name = "User32")]
    extern "system" {
        fn GetProcessWindowStation() -> *mut std::ffi::c_void;
        fn GetUserObjectInformationW(
            h_obj: *mut std::ffi::c_void,
            n_index: i32,
            pv_info: *mut std::ffi::c_void,
            n_length: u32,
            lpn_length_needed: *mut u32,
        ) -> i32;
    }

    /// Is this process in a non-interactive window station (a service)?
    /// HotSpot's handler passes a logoff on to the next handler there.
    fn non_interactive_session() -> bool {
        let mut flags = UserObjectFlags {
            f_inherit: 0,
            f_reserved: 0,
            dw_flags: 0,
        };
        // SAFETY: the station handle is the process's own (not closed); the
        // out-parameter is a live local of the size passed.
        unsafe {
            let station = GetProcessWindowStation();
            !station.is_null()
                && GetUserObjectInformationW(
                    station,
                    UOI_FLAGS,
                    (&mut flags as *mut UserObjectFlags).cast(),
                    std::mem::size_of::<UserObjectFlags>() as u32,
                    std::ptr::null_mut(),
                ) != 0
                && flags.dw_flags & WSF_VISIBLE == 0
        }
    }

    /// HotSpot 25's `consoleHandler`, minus `Ctrl-Break` (the dump latch's).
    /// The console runs it on a thread of its own; it only posts the signal.
    unsafe extern "system" fn console_handler(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT => {
                super::post_signal(SIGINT);
                1
            }
            CTRL_LOGOFF_EVENT if non_interactive_session() => 0,
            CTRL_LOGOFF_EVENT | CTRL_CLOSE_EVENT | CTRL_SHUTDOWN_EVENT => {
                super::post_signal(SIGTERM);
                1
            }
            _ => 0,
        }
    }

    pub struct LauncherSignals {
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl LauncherSignals {
        pub fn start(shared: &Arc<SharedVm>) -> Option<Self> {
            // `-Xrs`: HotSpot installs no console handler either.
            if shared.config.reduce_signal_usage
                || !cratonvm_types::flags::runtime_flag_default_on(
                    "CRATONVM_WINDOWS_CONSOLE_SIGNALS",
                )
            {
                return None;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let worker = super::spawn_dispatcher(shared, &stop)?;
            let handler: unsafe extern "system" fn(u32) -> i32 = console_handler;
            // SAFETY: registers a handler that does one atomic add and
            // returns; removed in `drop`.
            if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
                stop.store(true, Ordering::Release);
                super::wake_dispatcher_to_stop(&worker);
                let _ = worker.join();
                return None;
            }
            Some(Self {
                stop,
                worker: Some(worker),
            })
        }
    }

    impl Drop for LauncherSignals {
        fn drop(&mut self) {
            let handler: unsafe extern "system" fn(u32) -> i32 = console_handler;
            // SAFETY: removes the handler `start` registered.
            unsafe { SetConsoleCtrlHandler(Some(handler), 0) };
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                super::wake_dispatcher_to_stop(&worker);
                let _ = worker.join();
            }
        }
    }
}

#[cfg(unix)]
pub use unix::LauncherSignals;
#[cfg(windows)]
pub use windows::LauncherSignals;
#[cfg(not(any(unix, windows)))]
pub struct LauncherSignals;
#[cfg(not(any(unix, windows)))]
impl LauncherSignals {
    pub fn start(_shared: &std::sync::Arc<crate::vm::SharedVm>) -> Option<Self> {
        None
    }
}
