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
/// L1; performance, default OFF): the dispatcher waits on an OS object the
/// handler raises -- a non-blocking self-pipe on POSIX (`write(2)` is
/// async-signal-safe), an auto-reset event on Windows (the console handler
/// runs on an ordinary thread) -- instead of waking every 20 ms to look at
/// the counters. HotSpot's Signal Dispatcher waits on a semaphore the
/// handler posts, so a caught signal reaches Java at once and an idle VM
/// takes no periodic wake-ups; here the default polls (up to 20 ms late,
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
            // SAFETY: one byte from a live local; the descriptor is never
            // closed (see the type). A full pipe (EAGAIN) already wakes.
            unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
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
    let direct = cratonvm_types::flags::runtime_flag_on("CRATONVM_LAUNCHER_SIGNAL_DIRECT_WAKE");
    let wake = if direct { DirectWake::create() } else { None };
    if let Some(wake) = &wake {
        wake.publish();
    }
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
                        let (started, _) = std::sync::mpsc::sync_channel(1);
                        let (tx, rx) =
                            std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
                        let _ = tx.send(Box::new(move |shared, thread| {
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
